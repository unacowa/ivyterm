use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
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
    ClipboardPaste,
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
    TabClosed(u32),
    TabRenamed(u32, String),
    SessionChanged(u32, String),
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
    ssh_target: Option<String>,
    event_channel: Sender<TmuxEvent>,
    command_queue: Receiver<TmuxCommand>,
    current_command: Option<TmuxCommand>,
    is_error: bool,
    result_line: usize,
    empty_line_count: usize,
}

impl TmuxParserState {
    fn new(
        tmux_event_sender: Sender<TmuxEvent>,
        cmd_queue_receiver: Receiver<TmuxCommand>,
        ssh_target: Option<String>,
    ) -> Self {
        Self {
            command_queue: cmd_queue_receiver,
            event_channel: tmux_event_sender,
            current_command: None,
            is_error: false,
            ssh_target,
            result_line: 0,
            empty_line_count: 0,
        }
    }
}

impl TmuxAPI {
    pub fn new(
        session_name: &str,
        ssh_target: Option<&str>,
        tmux_command: Option<&str>,
        window: &IvyTmuxWindow,
    ) -> Result<TmuxAPI, IvyError> {
        // Create async channels
        let (tmux_event_sender, tmux_event_receiver): (Sender<TmuxEvent>, Receiver<TmuxEvent>) =
            async_channel::unbounded();

        // Command queue
        let (cmd_queue_sender, cmd_queue_receiver): (Sender<TmuxCommand>, Receiver<TmuxCommand>) =
            async_channel::unbounded();
        // Parse attach output
        cmd_queue_sender.send_blocking(TmuxCommand::Init).unwrap();

        // Spawn TMUX subprocess (through the system ssh client if a host is given)
        let writer = spawn_tmux(
            session_name,
            ssh_target,
            tmux_command,
            tmux_event_sender,
            cmd_queue_receiver,
        )?;

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
    session_name: &str,
    ssh_target: Option<&str>,
    tmux_command: Option<&str>,
    tmux_event_sender: Sender<TmuxEvent>,
    cmd_queue_receiver: Receiver<TmuxCommand>,
) -> Result<Box<dyn Write>, IvyError> {
    // The command which launches Tmux can be overridden, e.g.
    // "distrobox enter arch -- tmux"
    let tmux_command = tmux_command.unwrap_or("tmux");
    let mut tmux_args = tmux_command.split_whitespace();
    let tmux_program = tmux_args.next().unwrap_or("tmux");

    // When an SSH host is given, run Tmux remotely through the system ssh
    // client, so the user's full SSH configuration (aliases, IdentityFile,
    // ProxyJump, ...) applies
    let mut command = if let Some(host) = ssh_target {
        println!(
            "Attaching to Tmux session {} on {} ({})",
            session_name, host, tmux_command
        );
        let mut command = Command::new("ssh");
        command.arg(host).arg(tmux_program);
        command
    } else {
        println!(
            "Attaching to Tmux session {} ({})",
            session_name, tmux_command
        );
        Command::new(tmux_program)
    };
    command.args(tmux_args);

    let mut process = command
        .arg("-2")
        .arg("-C")
        .arg("new-session")
        .arg("-A")
        .arg("-s")
        .arg(session_name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| {
            eprintln!("Failed to spawn Tmux: {}", err);
            IvyError::TmuxSpawnFailed
        })?;

    // Read from Tmux STDOUT and send events to the channel on a separate thread
    let mut stdout_stream = process.stdout.take().expect("Failed to open stdout");
    let ssh_target = ssh_target.map(|host| host.to_string());
    spawn_blocking(move || {
        let mut ring_buffer = Ring::new(16_000).unwrap();
        let mut state = TmuxParserState::new(tmux_event_sender, cmd_queue_receiver, ssh_target);

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
    });

    let stdin_stream = process.stdin.take().expect("Failed to open stdin");
    return Ok(Box::new(stdin_stream));
}
