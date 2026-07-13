use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use async_channel::{Receiver, Sender};
use enumflags2::{bitflags, BitFlags};
use glib::JoinHandle;
use gtk4::gio::spawn_blocking;
use gtk4::Orientation;
use receive::tmux_parse_data;
use vmap::io::{Ring, SeqWrite};

use crate::helpers::IvyError;
use crate::keyboard::Direction;
use crate::tmux_widgets::IvyTmuxWindow;

mod parse_layout;
mod receive;
mod send;

pub struct TmuxAPI {
    stdin_stream: RefCell<Box<dyn Write>>,
    command_queue: Sender<TmuxCommand>,
    window_size: Cell<(i32, i32)>,
    resize_future: Cell<bool>,
    receive_future: JoinHandle<()>,
}

impl Drop for TmuxAPI {
    fn drop(&mut self) {
        // Stop main-thread future which receives Tmux events
        self.receive_future.abort();
    }
}

pub struct LayoutSync {
    pub tab_id: u32,
    pub layout: Vec<TmuxPane>,
    pub visible_layout: Vec<TmuxPane>,
    pub flags: BitFlags<LayoutFlags>,
    pub name: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum TmuxCommand {
    Init,
    InitialLayout,
    Keypress,
    TabNew,
    TabClose,
    TabSelect(u32),
    TabRename(u32),
    PaneSplit(bool),
    PaneClose(u32),
    PaneSelect(u32),
    PaneCurrentPath(u32),
    PaneMoveFocus(Direction),
    PaneZoom(u32),
    PaneResize(u32),
    ChangeSize(i32, i32),
    InitialOutput(u32),
    FetchBuffer,
    ClearScrollback(u32),
}

pub enum TmuxEvent {
    ScrollOutput(u32, usize),
    InitialLayout(LayoutSync),
    InitialLayoutFinished,
    InitialOutputFinished(u32),
    LayoutChanged(LayoutSync),
    Output(u32, Vec<u8>, bool),
    SizeChanged,
    PaneFocusChanged(u32, u32),
    TabFocusChanged(u32),
    TabNew(LayoutSync),
    WindowAdded(u32),
    TabClosed(u32),
    TabRenamed(u32, String),
    SessionChanged(u32, String),
    PasteBufferChanged(String),
    ClipboardText(String),
    Exit,
    ScrollbackCleared(u32),
}

#[bitflags]
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LayoutFlags {
    HasFocus,
    IsZoomed,
}

#[derive(Debug, Clone, Copy)]
pub struct Rectangle {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug)]
pub enum TmuxPane {
    Terminal(u32, Rectangle),
    /// Has tuple (is_vertical, bounds)
    Container(Orientation, Rectangle),
    Return,
}

struct TmuxParserState {
    event_channel: Sender<TmuxEvent>,
    command_queue: Receiver<TmuxCommand>,
    current_command: Option<TmuxCommand>,
    is_error: bool,
    result_line: usize,
    empty_line_count: usize,
    /// Accumulates multi-line output of `show-buffer` (FetchBuffer command)
    fetch_buffer: Vec<u8>,
    /// False until the first %begin is seen; shell output arriving before it
    /// (prompt echo over a pty transport, ...) is discarded
    preamble_done: bool,
}

impl TmuxParserState {
    fn new(
        tmux_event_sender: Sender<TmuxEvent>,
        cmd_queue_receiver: Receiver<TmuxCommand>,
    ) -> Self {
        Self {
            command_queue: cmd_queue_receiver,
            event_channel: tmux_event_sender,
            current_command: None,
            is_error: false,
            result_line: 0,
            empty_line_count: 0,
            fetch_buffer: Vec::new(),
            preamble_done: false,
        }
    }
}

impl TmuxAPI {
    pub fn new(argv: &[String], window: &IvyTmuxWindow) -> Result<TmuxAPI, IvyError> {
        // Create async channels
        let (tmux_event_sender, tmux_event_receiver): (Sender<TmuxEvent>, Receiver<TmuxEvent>) =
            async_channel::unbounded();

        // Command queue
        let (cmd_queue_sender, cmd_queue_receiver): (Sender<TmuxCommand>, Receiver<TmuxCommand>) =
            async_channel::unbounded();
        // Parse attach output
        cmd_queue_sender.send_blocking(TmuxCommand::Init).unwrap();

        // Spawn TMUX subprocess
        let writer = spawn_tmux(argv, tmux_event_sender, cmd_queue_receiver)?;

        // Receive events from the channel on main thread
        let receive_future = glib::spawn_future_local(glib::clone!(
            #[weak]
            window,
            async move {
                while let Ok(event) = tmux_event_receiver.recv().await {
                    window.tmux_event_callback(event)
                }
            }
        ));

        // Handle Tmux STDIN
        let tmux = TmuxAPI {
            stdin_stream: RefCell::new(writer),
            command_queue: cmd_queue_sender,
            window_size: Cell::new((0, 0)),
            resize_future: Cell::new(false),
            receive_future,
        };

        Ok(tmux)
    }
}

#[inline]
fn read_into_ringbuffer<T: Read>(
    stream: &mut T,
    ring_buffer: &mut Ring,
) -> Result<usize, std::io::Error> {
    // Construct '&mut [u8]' from '*mut u8'
    let len = ring_buffer.write_len();
    let write_buffer = ring_buffer.as_write_slice(len);

    // Read into byte array
    stream.read(write_buffer).map(|bytes_read| {
        if bytes_read > 0 {
            // Move the ringbuffer write position
            ring_buffer.feed(bytes_read);
        }

        bytes_read
    })
}

fn spawn_tmux(
    argv: &[String],
    tmux_event_sender: Sender<TmuxEvent>,
    cmd_queue_receiver: Receiver<TmuxCommand>,
) -> Result<Box<dyn Write>, IvyError> {
    // The command is responsible for starting Tmux in control mode itself
    // (including "-2 -C new-session ..."), which allows arbitrary transports
    // (ssh, et, distrobox, ...)
    let (program, args) = argv.split_first().ok_or(IvyError::TmuxSpawnFailed)?;
    println!("Attaching to Tmux using command: {}", argv.join(" "));

    // Spawn the command on a pty rather than pipes: some transports
    // (e.g. Eternal Terminal) require a real terminal and silently discard
    // piped stdin. Raw mode prevents the pty from echoing our commands back
    // into the output stream and from mangling line endings
    // A real (nonzero) window size matters: transports forward it to the
    // remote pty, and e.g. `podman exec -t` (distrobox) produces no output
    // on a 0x0 terminal. Tmux itself ignores the control client's size;
    // ivyterm reports pane sizes via refresh-client instead
    let winsize = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None).map_err(|err| {
        eprintln!("Failed to open pty: {}", err);
        IvyError::TmuxSpawnFailed
    })?;
    let mut termios = nix::sys::termios::tcgetattr(&pty.slave).map_err(|err| {
        eprintln!("Failed to get pty termios: {}", err);
        IvyError::TmuxSpawnFailed
    })?;
    nix::sys::termios::cfmakeraw(&mut termios);
    nix::sys::termios::tcsetattr(&pty.slave, nix::sys::termios::SetArg::TCSANOW, &termios)
        .map_err(|err| {
            eprintln!("Failed to set pty termios: {}", err);
            IvyError::TmuxSpawnFailed
        })?;

    let mut command = Command::new(program);
    command.args(args);
    // Transports with a remote pty (et) forward TERM to the target, which
    // may lack the terminfo entry ivyterm inherited (e.g. inside a
    // container); Tmux then refuses to start. The control mode client
    // never draws, so a universally available TERM works everywhere
    command.env("TERM", "xterm-256color");
    let slave_stdout = pty.slave.try_clone().map_err(|err| {
        eprintln!("Failed to clone pty fd: {}", err);
        IvyError::TmuxSpawnFailed
    })?;
    command
        .stdin(pty.slave)
        .stdout(slave_stdout)
        .stderr(Stdio::inherit());
    // Make the pty the controlling terminal of the child, in case the
    // command opens /dev/tty
    unsafe {
        command.pre_exec(|| {
            nix::libc::setsid();
            nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0);
            Ok(())
        });
    }
    command.spawn().map_err(|err| {
        eprintln!("Failed to spawn Tmux: {}", err);
        IvyError::TmuxSpawnFailed
    })?;

    let master_write = pty.master.try_clone().map_err(|err| {
        eprintln!("Failed to clone pty fd: {}", err);
        IvyError::TmuxSpawnFailed
    })?;

    // Read Tmux output from the pty master and send events to the channel
    // on a separate thread
    let mut stdout_stream = File::from(pty.master);
    spawn_blocking(move || {
        let mut ring_buffer = Ring::new(16_000).unwrap();
        let mut state = TmuxParserState::new(tmux_event_sender, cmd_queue_receiver);

        loop {
            match read_into_ringbuffer(&mut stdout_stream, &mut ring_buffer) {
                Ok(bytes_read) => {
                    if bytes_read < 1 {
                        continue;
                    }

                    // Consume the read bytes
                    if let Err(_) = tmux_parse_data(&mut state, &mut ring_buffer) {
                        return;
                    }
                }
                Err(_) => break,
            }
        }

        // The pty read fails once the child exits (EIO); close the window
        // instead of leaving it attached to a dead transport
        let _ = state.event_channel.send_blocking(TmuxEvent::Exit);
    });

    return Ok(Box::new(File::from(master_write)));
}
