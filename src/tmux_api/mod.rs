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
use vmap::io::{Ring, SeqRead, SeqWrite};

use crate::helpers::{IvyError, TmuxError};
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
    /// Number of %paste-buffer-changed notifications caused by our own
    /// set_buffer calls; those must not be fetched back into the clipboard
    pending_buffer_echoes: Cell<usize>,
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
    SetBuffer,
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
    /// Output lines received since %begin. They are held back until the
    /// closing %end/%error arrives, because only that line tells whether
    /// they are the command's result or an error message
    block_lines: Vec<Vec<u8>>,
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
            block_lines: Vec::new(),
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
            pending_buffer_echoes: Cell::new(0),
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
        let state = TmuxParserState::new(tmux_event_sender, cmd_queue_receiver);
        reader_thread_main(&mut stdout_stream, state);
    });

    return Ok(Box::new(File::from(master_write)));
}

/// Initial capacity of the ring buffer holding Tmux control-mode output
/// (vmap rounds this up to a multiple of the page size, i.e. 16384)
const INITIAL_RING_CAPACITY: usize = 16_000;

/// Upper bound when growing the ring buffer to fit a single control-mode
/// line. A line larger than this means the stream is corrupt; treat it as
/// a lost connection rather than allocating unbounded memory.
const MAX_RING_CAPACITY: usize = 64 * 1024 * 1024;

/// Why the reader loop stopped
#[derive(Debug)]
enum ReaderStop {
    /// Tmux/ssh closed its stdout (process died, connection dropped)
    Eof,
    /// Reading from the Tmux stdout pipe failed
    /// (the error is only read through the Debug impl when logging)
    ReadFailed(#[allow(dead_code)] std::io::Error),
    /// A single line exceeded MAX_RING_CAPACITY (or growing failed)
    BufferOverflow,
    /// The parser asked to stop (%exit received, event channel closed, ...)
    ParserStop(TmuxError),
}

/// Replaces `ring_buffer` with a larger one holding the same unread data.
/// Fails when the new size would exceed `max_capacity` or mapping fails.
fn grow_ring(ring_buffer: &mut Ring, max_capacity: usize) -> Result<(), ()> {
    let unread = ring_buffer.read_len();
    let new_capacity = (unread * 2).max(INITIAL_RING_CAPACITY);
    if new_capacity > max_capacity {
        return Err(());
    }

    let mut new_ring = Ring::new(new_capacity).map_err(|_| ())?;
    new_ring
        .as_write_slice(unread)
        .copy_from_slice(ring_buffer.as_read_slice(unread));
    new_ring.feed(unread);

    *ring_buffer = new_ring;
    Ok(())
}

/// Reads Tmux control-mode output into `ring_buffer` and feeds it to the
/// parser until the stream ends or the parser stops
fn run_tmux_reader<T: Read>(
    stream: &mut T,
    ring_buffer: &mut Ring,
    state: &mut TmuxParserState,
) -> ReaderStop {
    loop {
        // A full ring without a complete line means the parser can never
        // make progress and read() has no space to write to: grow the
        // buffer instead of spinning on reads that return 0 forever
        if ring_buffer.write_len() == 0 {
            if grow_ring(ring_buffer, MAX_RING_CAPACITY).is_err() {
                return ReaderStop::BufferOverflow;
            }
        }

        match read_into_ringbuffer(stream, ring_buffer) {
            // Reading 0 bytes into a non-full buffer means EOF
            Ok(0) => return ReaderStop::Eof,
            Ok(_) => {
                if let Err(err) = tmux_parse_data(state, ring_buffer) {
                    return ReaderStop::ParserStop(err);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return ReaderStop::ReadFailed(err),
        }
    }
}

fn reader_thread_main<T: Read>(stream: &mut T, mut state: TmuxParserState) {
    let mut ring_buffer =
        Ring::new(INITIAL_RING_CAPACITY).expect("Failed to allocate Tmux parser buffer");

    match run_tmux_reader(stream, &mut ring_buffer, &mut state) {
        // %exit was parsed: TmuxEvent::Exit has already been delivered
        ReaderStop::ParserStop(TmuxError::ExitEventReceived) => {}
        // Nobody is listening anymore (the window was closed)
        ReaderStop::ParserStop(TmuxError::EventChannelClosed) => {}
        stop => {
            // The connection died without a clean %exit (ssh drop, killed
            // process, corrupt stream): notify the window so it can close
            // instead of sitting on a dead session
            eprintln!("Tmux connection lost: {:?}", stop);
            let _ = state.event_channel.send_blocking(TmuxEvent::Exit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn test_state() -> (TmuxParserState, Receiver<TmuxEvent>, Sender<TmuxCommand>) {
        let (event_sender, event_receiver) = async_channel::unbounded();
        let (cmd_sender, cmd_receiver) = async_channel::unbounded();
        let mut state = TmuxParserState::new(event_sender, cmd_receiver);
        // These tests exercise the reader mechanics, so start past the
        // preamble skip (covered by receive::tests) and feed notifications
        // directly
        state.preamble_done = true;
        (state, event_receiver, cmd_sender)
    }

    fn drain_events(receiver: &Receiver<TmuxEvent>) -> Vec<TmuxEvent> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    /// Issue #1: when tmux/ssh dies without %exit (EOF on stdout), the
    /// reader must deliver TmuxEvent::Exit and stop, instead of spinning
    /// forever at 100% CPU without notifying the window.
    /// (Before the fix this test never returned.)
    #[test]
    fn eof_without_exit_notifies_window_and_stops() {
        let (state, events, _cmds) = test_state();
        let mut stream = Cursor::new(b"%output %1 hello\n".to_vec());

        reader_thread_main(&mut stream, state);

        let events = drain_events(&events);
        assert_eq!(events.len(), 2, "expected Output + Exit");
        assert!(
            matches!(&events[0], TmuxEvent::Output(1, output, false) if output == b"hello"),
            "first event should be the parsed %output"
        );
        assert!(
            matches!(events[1], TmuxEvent::Exit),
            "EOF must be turned into an Exit event"
        );
    }

    /// A clean %exit must produce exactly one Exit event (the reader must
    /// not send a second one when it stops afterwards)
    #[test]
    fn clean_exit_sends_exactly_one_exit_event() {
        let (state, events, _cmds) = test_state();
        let mut stream = Cursor::new(b"%exit\n".to_vec());

        reader_thread_main(&mut stream, state);

        let events = drain_events(&events);
        let exit_count = events
            .iter()
            .filter(|event| matches!(event, TmuxEvent::Exit))
            .count();
        assert_eq!(exit_count, 1);
    }

    /// A read error (e.g. broken pipe mid-stream) must also end with an
    /// Exit event instead of leaving the window attached to a dead session
    #[test]
    fn read_error_notifies_window_and_stops() {
        struct FailingReader {
            data: Vec<u8>,
            position: usize,
        }
        impl Read for FailingReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.position < self.data.len() {
                    let n = buf.len().min(self.data.len() - self.position);
                    buf[..n].copy_from_slice(&self.data[self.position..self.position + n]);
                    self.position += n;
                    Ok(n)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "connection reset",
                    ))
                }
            }
        }

        let (state, events, _cmds) = test_state();
        let mut stream = FailingReader {
            data: b"%output %1 hi\n".to_vec(),
            position: 0,
        };

        reader_thread_main(&mut stream, state);

        let events = drain_events(&events);
        assert!(matches!(&events[0], TmuxEvent::Output(1, output, false) if output == b"hi"));
        assert!(matches!(events.last(), Some(TmuxEvent::Exit)));
    }

    /// The reader must not panic when the window is gone (event channel
    /// closed) while output is still arriving
    #[test]
    fn closed_event_channel_stops_reader_without_panic() {
        let (state, events, _cmds) = test_state();
        drop(events);
        let mut stream = Cursor::new(b"%output %1 hi\n%output %1 again\n".to_vec());

        reader_thread_main(&mut stream, state);
    }

    /// Issue #2: a single line larger than the ring buffer used to leave
    /// the reader spinning forever (no \n in a full ring -> parser consumes
    /// nothing -> write_len() == 0 -> read() returns 0 -> continue).
    /// The buffer must grow instead, and the line must survive intact.
    /// (Before the fix this test never returned.)
    #[test]
    fn line_exceeding_ring_capacity_grows_buffer() {
        // Spans multiple growth cycles: 16 KiB -> 32 KiB -> 64 KiB -> 128 KiB
        let payload = vec![b'a'; 100_000];
        let mut data = b"%output %1 ".to_vec();
        data.extend_from_slice(&payload);
        data.push(b'\n');

        let (state, events, _cmds) = test_state();
        let mut stream = Cursor::new(data);

        reader_thread_main(&mut stream, state);

        let events = drain_events(&events);
        assert_eq!(events.len(), 2, "expected Output + Exit");
        assert!(
            matches!(&events[0], TmuxEvent::Output(1, output, false) if *output == payload),
            "oversized %output line must be parsed losslessly after the buffer grows"
        );
        assert!(matches!(events[1], TmuxEvent::Exit));
    }

    /// Growth boundary: a line that fills the ring to the byte before the
    /// newline arrives
    #[test]
    fn line_exactly_at_ring_capacity_is_parsed() {
        // vmap rounds INITIAL_RING_CAPACITY up to the page size multiple
        let ring_capacity = Ring::new(INITIAL_RING_CAPACITY).unwrap().write_len();
        let prefix = b"%output %1 ";
        let payload = vec![b'b'; ring_capacity - prefix.len()];
        let mut data = prefix.to_vec();
        data.extend_from_slice(&payload);
        data.push(b'\n');

        let (state, events, _cmds) = test_state();
        let mut stream = Cursor::new(data);

        reader_thread_main(&mut stream, state);

        let events = drain_events(&events);
        assert!(
            matches!(&events[0], TmuxEvent::Output(1, output, false) if *output == payload)
        );
        assert!(matches!(events.last(), Some(TmuxEvent::Exit)));
    }

    /// grow_ring preserves unread data and respects the capacity limit
    #[test]
    fn grow_ring_preserves_content_and_respects_limit() {
        let mut ring = Ring::new(INITIAL_RING_CAPACITY).unwrap();
        let capacity = ring.write_len();

        // Fill the ring completely
        let content: Vec<u8> = (0..capacity).map(|i| (i % 251) as u8).collect();
        ring.as_write_slice(capacity).copy_from_slice(&content);
        ring.feed(capacity);
        assert_eq!(ring.write_len(), 0);

        // Growing beyond the limit must fail (and not touch the ring)
        assert!(grow_ring(&mut ring, capacity).is_err());
        assert_eq!(ring.read_len(), capacity);

        // Growing within the limit must preserve all unread bytes
        assert!(grow_ring(&mut ring, MAX_RING_CAPACITY).is_ok());
        assert!(ring.write_len() > 0, "grown ring must have free space");
        assert_eq!(ring.read_len(), capacity);
        assert_eq!(ring.as_read_slice(capacity), &content[..]);
    }
}
