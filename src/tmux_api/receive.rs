// BufRead provides Ring::consume
use std::{io::BufRead, str::from_utf8};

use async_channel::Sender;
use log::debug;
use vmap::io::Ring;

use crate::{
    helpers::{open_editor, TmuxError},
    tmux_api::TmuxEvent,
};

use super::{parse_layout::parse_tmux_layout, TmuxCommand, TmuxParserState};

pub fn tmux_parse_data(
    state: &mut TmuxParserState,
    ring_buffer: &mut Ring,
) -> Result<(), TmuxError> {
    let mut consumed_bytes = 0;
    let buffer = ring_buffer.as_ref();

    for (i, b) in buffer.iter().enumerate() {
        if *b == b'\n' {
            let full_line = &buffer[consumed_bytes..i];
            let mut line = full_line;
            // Tolerate \r around lines; the transport may involve a pty
            // (e.g. Eternal Terminal), which turns \n into \r\n. Any \r
            // within %output content is octal-escaped, so a bare \r can
            // only come from the transport
            while line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }

            // A pty transport also mixes shell output (prompt echo, shell
            // integration escape sequences, ...) into the stream before Tmux
            // takes over, and the last chunk of it shares a line with the
            // first %begin. Discard everything until that %begin; control
            // mode output is guaranteed to start with one
            if !state.preamble_done {
                match line.windows(6).position(|w| w == b"%begin") {
                    Some(pos) => {
                        line = &line[pos..];
                        state.preamble_done = true;
                    }
                    None => {
                        // Junk line from before Tmux started
                        consumed_bytes = i + 1;
                        continue;
                    }
                }
            }

            let stripped = full_line.len() - line.len();
            consumed_bytes += tmux_parse_line(state, line)? + stripped + 1; // +1 to account for \n
        }
    }

    // Move the ringbuffer read position
    ring_buffer.consume(consumed_bytes);

    Ok(())
}

/// Parses Tmux output and replaces octal escapes sequences with correct binary
/// characters
#[inline]
fn parse_escaped_output(input: &[u8], prepend_linebreak: bool, empty_lines: usize) -> Vec<u8> {
    let input_len = input.len();
    let mut output = Vec::with_capacity(input_len + (empty_lines * 2) + 3);

    if prepend_linebreak {
        output.push(b'\r');
        output.push(b'\n');
    }

    for _ in 0..empty_lines {
        output.push(b'\r');
        output.push(b'\n');
    }

    let mut i = 0;
    while i < input_len {
        let char = input[i];
        if char != b'\\' {
            output.push(char);
            i += 1;
            continue;
        }

        // "\\" is an escaped backslash
        if input.get(i + 1) == Some(&b'\\') {
            output.push(b'\\');
            i += 2;
            continue;
        }

        // "\ooo" is an octal escape (\000-\377). Anything else after a
        // backslash is not an escape sequence Tmux produces; emit it
        // verbatim instead of panicking on malformed input (e.g. a line
        // ending in "M-\")
        if let Some(digits) = input.get(i + 1..i + 4) {
            let is_octal =
                digits[0] <= b'3' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit));
            if is_octal {
                let byte = digits
                    .iter()
                    .fold(0u8, |acc, digit| (acc << 3) | (digit - b'0'));
                output.push(byte);
                i += 4;
                continue;
            }
        }

        output.push(b'\\');
        i += 1;
    }

    output
}

#[inline]
fn buffer_starts_with(buffer: &[u8], prefix: &str) -> bool {
    if prefix.len() > buffer.len() {
        return false;
    }

    let buffer = &buffer[..prefix.len()];
    let prefix = prefix.as_bytes();

    buffer == prefix
}

#[inline]
fn receive_event(event_channel: &Sender<TmuxEvent>, event: TmuxEvent) -> Result<(), TmuxError> {
    event_channel
        .send_blocking(event)
        .map_err(|_| TmuxError::EventChannelClosed)
}

#[inline]
fn parse_utf8(buffer: &[u8]) -> Result<&str, TmuxError> {
    from_utf8(buffer).map_err(|_| TmuxError::ErrorParsingUTF8)
}

#[inline]
pub fn tmux_parse_line(state: &mut TmuxParserState, buffer: &[u8]) -> Result<usize, TmuxError> {
    let event_channel = &state.event_channel;
    let command_queue = &state.command_queue;

    // All output from Tmux is ASCII, except %output which we handle separately
    if buffer.len() == 0 {
        // A blank line belonging to the output of the current command
        if state.current_command.is_some() {
            state.block_lines.push(Vec::new());
        }
        return Ok(0);
    }

    debug!("Tmux output: .{}.", parse_utf8(&buffer)?);

    if buffer[0] != b'%' {
        // Output of the command currently between %begin and %end/%error.
        // It is held back until the closing line arrives, since only that
        // tells whether this is the result or an error message
        if state.current_command.is_some() {
            state.block_lines.push(buffer.to_vec());
        }
        return Ok(buffer.len());
    }

    // This is a notification
    if buffer_starts_with(&buffer, "%output") {
        // We were given output, we can assume that up until pane_id, output is ASCII
        let (pane_id, chars_read) = read_first_u32(&buffer[9..]);
        let output = parse_escaped_output(&buffer[9 + chars_read..], false, 0);

        receive_event(&event_channel, TmuxEvent::Output(pane_id, output, false))?;
    } else if buffer_starts_with(&buffer, "%begin") {
        // Beginning of output from a command we executed
        state.current_command = Some(command_queue.recv_blocking().unwrap());
        state.block_lines.clear();
    } else if buffer_starts_with(&buffer, "%end") {
        // The command succeeded: dispatch its buffered output
        let lines = std::mem::take(&mut state.block_lines);
        if let Some(command) = state.current_command.take() {
            dispatch_command_result(&command, lines, event_channel)?;
        }
    } else if buffer_starts_with(&buffer, "%error") {
        // The command failed: the buffered lines are the error message
        let lines = std::mem::take(&mut state.block_lines);
        if let Some(command) = state.current_command.take() {
            let message = lines.join(&b'\n');
            eprintln!(
                "Tmux command {:?} failed: {}",
                command,
                String::from_utf8_lossy(&message)
            );
            dispatch_command_failed(&command, event_channel)?;
        }
    } else if buffer_starts_with(&buffer, "%window-pane-changed") {
        // %window-pane-changed @0 %10
        let (tab_id, chars_read) = read_first_u32(&buffer[22..]);
        let buffer = &buffer[22 + chars_read + 1..];
        let (pane_id, _) = read_first_u32(buffer);
        debug!(
            "Tmux event: Window {} focus changed to pane {}",
            tab_id, pane_id
        );
        receive_event(&event_channel, TmuxEvent::PaneFocusChanged(tab_id, pane_id))?;
    } else if buffer_starts_with(&buffer, "%window-add") {
        // %window-add @32
        // The window might have been created by another client; ask for its
        // layout so a Tab is created for it
        let (tab_id, _) = read_first_u32(&buffer[13..]);
        debug!("Tmux event: Window {} added", tab_id);
        receive_event(&event_channel, TmuxEvent::WindowAdded(tab_id))?;
    } else if buffer_starts_with(&buffer, "%session-window-changed") {
        // %session-window-changed $1 @1
        let (session_id, chars_read) = read_first_u32(&buffer[25..]);
        let buffer = &buffer[25 + chars_read + 1..];
        let (tab_id, _) = read_first_u32(buffer);
        debug!(
            "Tmux event: Session {} focus changed to window {}",
            session_id, tab_id
        );
        receive_event(&event_channel, TmuxEvent::TabFocusChanged(tab_id))?;
    } else if buffer_starts_with(&buffer, "%unlinked-window-close") {
        // %unlinked-window-close @6
        let (tab_id, _) = read_first_u32(&buffer[24..]);
        debug!("Tmux event: Tab {} closed", tab_id);
        receive_event(&event_channel, TmuxEvent::TabClosed(tab_id))?;
    } else if buffer_starts_with(&buffer, "%window-close") {
        // %window-close @6
        let (tab_id, _) = read_first_u32(&buffer[15..]);
        debug!("Tmux event: Tab {} closed", tab_id);
        receive_event(&event_channel, TmuxEvent::TabClosed(tab_id))?;
    } else if buffer_starts_with(&buffer, "%layout-change") {
        // Layout has changed
        let layout_sync = parse_tmux_layout(&buffer[15..]);
        receive_event(&event_channel, TmuxEvent::LayoutChanged(layout_sync))?;
    } else if buffer_starts_with(&buffer, "%paste-buffer-changed") {
        // %paste-buffer-changed name
        // A Tmux paste buffer changed (copy-mode yank, OSC 52 with
        // set-clipboard on, ...); fetch it to sync the system clipboard
        let name = parse_utf8(&buffer[22..])?.to_string();
        debug!("Tmux event: Paste buffer changed: {}", name);
        receive_event(&event_channel, TmuxEvent::PasteBufferChanged(name))?;
    } else if buffer_starts_with(&buffer, "%session-changed") {
        // Session has changed
        let (id, bytes_read) = read_first_u32(&buffer[18..]);
        let name = parse_utf8(&buffer[18 + bytes_read..])?.to_string();
        debug!("Tmux event: Session changed ({}): {}", id, name);

        receive_event(&event_channel, TmuxEvent::SessionChanged(id, name))?;
    } else if buffer_starts_with(&buffer, "%window-renamed") {
        // Session has changed
        let (id, bytes_read) = read_first_u32(&buffer[17..]);
        let name = parse_utf8(&buffer[17 + bytes_read..])?.to_string();
        debug!("Tmux event: Tab renamed ({}): {}", id, name);

        receive_event(&event_channel, TmuxEvent::TabRenamed(id, name))?;
    } else if buffer_starts_with(&buffer, "%exit") {
        // Tmux client has exited
        let reason = parse_utf8(&buffer[5..])?;
        debug!("Tmux event: Exit received, reason: {}", reason);
        receive_event(&event_channel, TmuxEvent::Exit)?;
        // Stop receiving events
        return Err(TmuxError::ExitEventReceived);
    } else if buffer_starts_with(&buffer, "%client-session-changed") {
    } else {
        // Unsupported notification
        let notification = parse_utf8(&buffer)?;
        debug!("Tmux event: Unknown notification: {}", notification)
    }

    Ok(buffer.len())
}

/// Handles the buffered output of a command that finished with %end
fn dispatch_command_result(
    command: &TmuxCommand,
    lines: Vec<Vec<u8>>,
    event_channel: &Sender<TmuxEvent>,
) -> Result<(), TmuxError> {
    match command {
        TmuxCommand::TabNew => {
            for line in lines.iter().filter(|line| !line.is_empty()) {
                let layout_sync = parse_tmux_layout(line);
                receive_event(&event_channel, TmuxEvent::TabNew(layout_sync))?;
            }
        }
        TmuxCommand::InitialLayout => {
            // list-windows prints one line per window
            for line in lines.iter().filter(|line| !line.is_empty()) {
                let layout_sync = parse_tmux_layout(line);
                receive_event(&event_channel, TmuxEvent::InitialLayout(layout_sync))?;
            }
            receive_event(&event_channel, TmuxEvent::InitialLayoutFinished)?;
        }
        TmuxCommand::InitialOutput(pane_id) => {
            let pane_id = *pane_id;

            // Replay the captured pane content. Blank lines become \r\n
            // prepended to the next non-empty line, so the restored
            // scrollback keeps its vertical layout
            let mut printed_lines = 0;
            let mut empty_line_count = 0;
            for line in &lines {
                if line.is_empty() {
                    empty_line_count += 1;
                    continue;
                }

                let output = parse_escaped_output(line, printed_lines > 0, empty_line_count);
                receive_event(&event_channel, TmuxEvent::Output(pane_id, output, true))?;
                printed_lines += 1;
                empty_line_count = 0;
            }

            // Trailing blank lines are not fed to the terminal; the widget
            // scrolls the view down instead
            receive_event(
                &event_channel,
                TmuxEvent::ScrollOutput(pane_id, empty_line_count),
            )?;
            receive_event(&event_channel, TmuxEvent::InitialOutputFinished(pane_id))?;
        }
        TmuxCommand::ChangeSize(_, _) => {
            receive_event(&event_channel, TmuxEvent::SizeChanged)?;
        }
        TmuxCommand::ClearScrollback(term_id) => {
            receive_event(&event_channel, TmuxEvent::ScrollbackCleared(*term_id))?;
        }
        TmuxCommand::FetchBuffer => {
            // Multi-line paste buffer content
            let data = lines.join(&b'\n');
            let text = String::from_utf8_lossy(&data).into_owned();
            receive_event(&event_channel, TmuxEvent::ClipboardText(text))?;
        }
        TmuxCommand::PaneCurrentPath(term_id) => {
            for line in lines.iter().filter(|line| line.len() > 7) {
                // Currently Tmux sends paths of all Terminals in the given Tab, so we need
                // to filter manually
                let (pane_id, bytes_read) = read_first_u32(&line[7..]);
                if pane_id == *term_id {
                    let path = parse_utf8(&line[7 + bytes_read..])?;
                    open_editor(path);
                }
            }
        }
        _ => {}
    }

    Ok(())
}

/// A command failed (%error). Still emit the events the UI waits on to
/// make progress, so a failed command cannot wedge the init/resize state
/// machines (the error text itself is only logged)
fn dispatch_command_failed(
    command: &TmuxCommand,
    event_channel: &Sender<TmuxEvent>,
) -> Result<(), TmuxError> {
    match command {
        TmuxCommand::InitialOutput(pane_id) => {
            receive_event(&event_channel, TmuxEvent::InitialOutputFinished(*pane_id))?;
        }
        TmuxCommand::InitialLayout => {
            receive_event(&event_channel, TmuxEvent::InitialLayoutFinished)?;
        }
        TmuxCommand::ChangeSize(_, _) => {
            receive_event(&event_channel, TmuxEvent::SizeChanged)?;
        }
        _ => {}
    }

    Ok(())
}

#[inline]
pub fn read_first_u32(buffer: &[u8]) -> (u32, usize) {
    let mut i = 0;
    let mut number: u32 = 0;

    // Read buffer char by char (assuming ASCII) and parse number
    while i < buffer.len() && buffer[i] > 47 && buffer[i] < 58 {
        number *= 10;
        number += (buffer[i] - 48) as u32;
        i += 1;
    }
    (number, i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::Receiver;
    use std::io::Write;

    fn test_state() -> (TmuxParserState, Receiver<TmuxEvent>, Sender<TmuxCommand>) {
        let (event_sender, event_receiver) = async_channel::unbounded();
        let (cmd_sender, cmd_receiver) = async_channel::unbounded();
        let state = TmuxParserState::new(event_sender, cmd_receiver);
        (state, event_receiver, cmd_sender)
    }

    #[test]
    fn pty_junk_before_first_begin_is_discarded() {
        let (mut state, event_rx, cmd_tx) = test_state();
        cmd_tx.send_blocking(TmuxCommand::InitialLayout).unwrap();

        let mut ring = Ring::new(16_000).unwrap();

        // Simulate a pty transport: prompt echo lines, then shell
        // integration escape sequences sharing a line with the first %begin
        let data = b"tmux -2 -C new-session; exit\r\n\x1b]133;A\x07mu@host:~$ tmux -2 -C new-session; exit\r\n\x1b[?2004l\r\x1b]133;C;\x07%begin 1 1 0\r\n%end 1 1 0\r\n";
        ring.write(&data[..]).unwrap();
        assert!(tmux_parse_data(&mut state, &mut ring).is_ok());

        // If \r%begin was recognized, the queued command was consumed and
        // %end fired InitialLayoutFinished
        assert!(
            state.command_queue.is_empty(),
            "%begin did not consume the queued command"
        );
        let mut got_finished = false;
        while let Ok(ev) = event_rx.try_recv() {
            if matches!(ev, TmuxEvent::InitialLayoutFinished) {
                got_finished = true;
            }
        }
        assert!(got_finished, "InitialLayoutFinished was not emitted");
    }

    fn drain_events(receiver: &Receiver<TmuxEvent>) -> Vec<TmuxEvent> {
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        events
    }

    fn feed_lines(state: &mut TmuxParserState, lines: &[&[u8]]) {
        for line in lines {
            assert!(
                tmux_parse_line(state, line).is_ok(),
                "parser returned an error for line: {}",
                String::from_utf8_lossy(line)
            );
        }
    }

    // ------------------------------------------------------------------
    // Issue #5: parse_escaped_output must never panic on malformed input
    // ------------------------------------------------------------------

    #[test]
    fn escaped_output_plain_passthrough() {
        assert_eq!(parse_escaped_output(b"hello world", false, 0), b"hello world");
    }

    #[test]
    fn escaped_output_decodes_octal_and_double_backslash() {
        assert_eq!(parse_escaped_output(b"a\\033[1mb", false, 0), b"a\x1b[1mb");
        assert_eq!(parse_escaped_output(b"a\\\\b", false, 0), b"a\\b");
        // Boundary values of the \000-\377 range
        assert_eq!(parse_escaped_output(b"\\000", false, 0), [0u8]);
        assert_eq!(parse_escaped_output(b"\\377", false, 0), [255u8]);
    }

    #[test]
    fn escaped_output_prepends_linebreaks_and_blank_lines() {
        assert_eq!(parse_escaped_output(b"x", true, 2), b"\r\n\r\n\r\nx");
        assert_eq!(parse_escaped_output(b"x", false, 1), b"\r\nx");
    }

    /// A line ending in a lone backslash used to read past the end of the
    /// buffer (index out of bounds panic)
    #[test]
    fn escaped_output_lone_trailing_backslash_is_verbatim() {
        assert_eq!(parse_escaped_output(b"abc\\", false, 0), b"abc\\");
    }

    /// A backslash with fewer than 3 bytes after it used to hit an
    /// explicit panic!("Found escape character but string too short")
    #[test]
    fn escaped_output_short_tail_is_verbatim() {
        assert_eq!(parse_escaped_output(b"ab\\12", false, 0), b"ab\\12");
        assert_eq!(parse_escaped_output(b"ab\\1", false, 0), b"ab\\1");
    }

    /// The crash case documented in the old TODO comment: literal
    /// backslashes arriving un-doubled
    #[test]
    fn escaped_output_known_crash_case_meta_backslash() {
        assert_eq!(
            parse_escaped_output(b"danes je pa tako M-\\M-\\", false, 0),
            b"danes je pa tako M-\\M-\\"
        );
    }

    /// Non-octal characters after a backslash used to underflow/overflow
    /// (panic in debug builds, garbage bytes + parser desync in release)
    #[test]
    fn escaped_output_invalid_escapes_are_verbatim() {
        // Digits 8/9 are not octal
        assert_eq!(parse_escaped_output(b"\\999x", false, 0), b"\\999x");
        // \400-\777 would be > 0xFF; tmux never emits them
        assert_eq!(parse_escaped_output(b"\\400", false, 0), b"\\400");
        // Letters are not octal at all
        assert_eq!(parse_escaped_output(b"\\abc", false, 0), b"\\abc");
        // A valid escape right after an invalid one still decodes
        assert_eq!(parse_escaped_output(b"\\9\\033", false, 0), b"\\9\x1b");
    }

    // ------------------------------------------------------------------
    // Issue #3: %error blocks must not be parsed as command results
    // ------------------------------------------------------------------

    /// tmux frames a failing command as %begin / error text / %error
    /// (verified against tmux 3.7b). The error text used to be fed into
    /// parse_tmux_layout, which panics on non-layout input.
    #[test]
    fn error_block_is_not_parsed_as_command_result() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::TabNew).unwrap();
        feed_lines(
            &mut state,
            &[
                b"%begin 1783693906 1149480 1" as &[u8],
                b"create window failed: no space for new pane",
                b"%error 1783693906 1149480 1",
            ],
        );

        let events = drain_events(&events);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TmuxEvent::TabNew(_))),
            "the error text must not be parsed as a TabNew layout"
        );
    }

    /// After an %error block the parser must keep working: the next
    /// queued command still gets its result
    #[test]
    fn parser_recovers_after_error_block() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::TabNew).unwrap();
        cmds.send_blocking(TmuxCommand::ChangeSize(80, 24)).unwrap();
        feed_lines(
            &mut state,
            &[
                b"%begin 100 1 1" as &[u8],
                b"create window failed: no space for new pane",
                b"%error 100 1 1",
                b"%begin 100 2 1",
                b"%end 100 2 1",
            ],
        );

        let events = drain_events(&events);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TmuxEvent::SizeChanged)),
            "the command following an error block must be processed normally"
        );
    }

    /// A failed capture-pane must still unblock the pane (the window
    /// waits for InitialOutputFinished) without feeding the error text
    /// into the terminal
    #[test]
    fn failed_initial_output_still_reports_finished() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::InitialOutput(5)).unwrap();
        feed_lines(
            &mut state,
            &[
                b"%begin 100 1 1" as &[u8],
                b"can't find pane: %5",
                b"%error 100 1 1",
            ],
        );

        let events = drain_events(&events);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TmuxEvent::InitialOutputFinished(5))),
            "a failed capture-pane must still report InitialOutputFinished"
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TmuxEvent::Output(_, _, _))),
            "the error text must not be fed into the terminal"
        );
    }

    /// A failed refresh-client -C must still deliver SizeChanged, or the
    /// init/resize state machine waits forever
    #[test]
    fn failed_resize_still_reports_size_changed() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::ChangeSize(80, 24)).unwrap();
        feed_lines(
            &mut state,
            &[
                b"%begin 100 1 1" as &[u8],
                b"size too small",
                b"%error 100 1 1",
            ],
        );

        let events = drain_events(&events);
        assert!(events
            .iter()
            .any(|event| matches!(event, TmuxEvent::SizeChanged)));
    }

    // ------------------------------------------------------------------
    // Issue #4: blank lines in the initial pane output must be preserved
    // ------------------------------------------------------------------

    #[test]
    fn initial_output_preserves_blank_lines() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::InitialOutput(7)).unwrap();
        feed_lines(
            &mut state,
            &[
                b"%begin 100 1 1" as &[u8],
                b"line1",
                b"",
                b"",
                b"line2",
                b"",
                b"%end 100 1 1",
            ],
        );

        let events = drain_events(&events);
        assert_eq!(events.len(), 4, "expected 2x Output + ScrollOutput + InitialOutputFinished");
        assert!(
            matches!(&events[0], TmuxEvent::Output(7, output, true) if output == b"line1"),
            "first line must arrive unchanged"
        );
        assert!(
            matches!(&events[1], TmuxEvent::Output(7, output, true) if output == b"\r\n\r\n\r\nline2"),
            "the two blank lines + line break must be preserved before line2"
        );
        assert!(
            matches!(events[2], TmuxEvent::ScrollOutput(7, 1)),
            "the trailing blank line must be reported for scrolling"
        );
        assert!(matches!(events[3], TmuxEvent::InitialOutputFinished(7)));
    }

    #[test]
    fn initial_output_of_blank_pane_only_scrolls() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::InitialOutput(3)).unwrap();
        feed_lines(
            &mut state,
            &[
                b"%begin 100 1 1" as &[u8],
                b"",
                b"",
                b"",
                b"%end 100 1 1",
            ],
        );

        let events = drain_events(&events);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, TmuxEvent::Output(_, _, _))),
            "a blank pane must not produce Output events"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, TmuxEvent::ScrollOutput(3, 3))));
    }

    // ------------------------------------------------------------------
    // Regression guards for behavior that already worked
    // ------------------------------------------------------------------

    #[test]
    fn fetch_buffer_joins_lines_including_blanks() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::FetchBuffer).unwrap();
        feed_lines(
            &mut state,
            &[
                b"%begin 100 1 1" as &[u8],
                b"first",
                b"",
                b"third",
                b"%end 100 1 1",
            ],
        );

        let events = drain_events(&events);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TmuxEvent::ClipboardText(text) if text == "first\n\nthird")),
            "multi-line paste buffers must keep their blank lines"
        );
    }

    #[test]
    fn output_notification_is_unescaped() {
        let (mut state, events, _cmds) = test_state();

        feed_lines(&mut state, &[b"%output %2 hi\\033[0m" as &[u8]]);

        let events = drain_events(&events);
        assert!(
            matches!(&events[0], TmuxEvent::Output(2, output, false) if output == b"hi\x1b[0m")
        );
    }

    #[test]
    fn empty_command_block_produces_no_result() {
        let (mut state, events, cmds) = test_state();

        cmds.send_blocking(TmuxCommand::Keypress).unwrap();
        feed_lines(
            &mut state,
            &[b"%begin 100 1 1" as &[u8], b"%end 100 1 1"],
        );

        assert!(drain_events(&events).is_empty());
    }
}
