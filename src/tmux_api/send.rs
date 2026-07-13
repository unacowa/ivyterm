use log::debug;

use crate::{
    helpers::TmuxError,
    keyboard::{Direction, KeyboardAction},
    tmux_api::TmuxCommand,
};

use super::TmuxAPI;

/// Escapes arbitrary text for use inside a double-quoted Tmux command
/// argument. Everything but the safest ASCII characters is emitted as a
/// \ooo octal escape, sidestepping all of Tmux's quoting, format (#{...})
/// and variable ($...) expansion rules; Tmux decodes the octal escapes
/// back to raw bytes
fn escape_for_tmux_quotes(text: &str) -> String {
    use std::fmt::Write;

    let mut escaped = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => escaped.push(byte as char),
            b' ' | b',' | b'.' | b'_' | b'-' | b'/' | b':' | b'=' | b'+' | b'@' => {
                escaped.push(byte as char)
            }
            _ => write!(escaped, "\\{:03o}", byte).unwrap(),
        }
    }
    escaped
}

impl TmuxAPI {
    #[inline]
    fn send_event(&self, event: TmuxCommand, cmd: &str) -> Result<(), TmuxError> {
        use std::io::Write;

        const NEWLINE: &[u8] = &[b'\n'];
        // First we put the Command in Event queue
        let command_queue = &self.command_queue;
        command_queue
            .send_blocking(event)
            .map_err(|_| TmuxError::EventChannelClosed)?;

        // Then we write the buffer to the Tmux input stream
        debug!("Sending event: {}", cmd);
        let mut stdin_stream = self.stdin_stream.borrow_mut();
        stdin_stream
            .write_all(cmd.as_bytes())
            .map_err(|_| TmuxError::EventChannelClosed)?;
        stdin_stream
            .write_all(NEWLINE)
            .map_err(|_| TmuxError::EventChannelClosed)?;

        Ok(())
    }

    pub fn get_initial_layout(&self) -> Result<(), TmuxError> {
        debug!("Getting initial layout");
        let cmd = "list-windows -F \"#{window_id} #{window_layout} #{window_visible_layout} #{window_flags} #{window_name}\"";
        self.send_event(TmuxCommand::InitialLayout, cmd)
    }

    /// Kill a Tmux window (the user closed its Tab)
    pub fn kill_window(&self, tab_id: u32) -> Result<(), TmuxError> {
        debug!("Killing window {}", tab_id);
        let cmd = format!("kill-window -t @{}", tab_id);
        self.send_event(TmuxCommand::TabClose, &cmd)
    }

    /// Ask for the layout of a single window (e.g. one added by another
    /// client), so a Tab can be created for it
    pub fn get_window_layout(&self, tab_id: u32) -> Result<(), TmuxError> {
        debug!("Getting layout of window {}", tab_id);
        let cmd = format!(
            "list-windows -f \"#{{==:#{{window_id}},@{}}}\" -F \"#{{window_id}} #{{window_layout}} #{{window_visible_layout}} #{{window_flags}} #{{window_name}}\"",
            tab_id
        );
        self.send_event(TmuxCommand::TabNew, &cmd)
    }

    pub fn get_initial_output(&self, pane_id: u32) -> Result<(), TmuxError> {
        debug!("Getting initial output of pane {}", pane_id);
        let event = TmuxCommand::InitialOutput(pane_id);
        let cmd = format!("capture-pane -J -p -t %{} -eC -S - -E -", pane_id);
        self.send_event(event, &cmd)
    }

    pub fn change_size(&self, cols: i32, rows: i32) -> Result<(), TmuxError> {
        // If Tmux client size hasn't changed, we don't need to send any update
        if self.window_size.get() == (cols, rows) {
            debug!(
                "Not updating Tmux size to {}x{}, since it did not change",
                cols, rows
            );
            return Ok(());
        }
        self.window_size.replace((cols, rows));

        println!("Resizing Tmux client to {}x{}", cols, rows);
        let event = TmuxCommand::ChangeSize(cols, rows);
        let cmd = format!("refresh-client -C {},{}", cols, rows);
        self.send_event(event, &cmd)
    }

    /// Send raw input bytes (as produced by VTE's `commit` signal) to a pane
    pub fn send_input_bytes(&self, pane_id: u32, text: &str) -> Result<(), TmuxError> {
        use std::fmt::Write;

        if text.is_empty() {
            return Ok(());
        }

        let mut cmd = format!("send-keys -t %{} -H", pane_id);
        for byte in text.bytes() {
            write!(cmd, " {:#04X}", byte).unwrap();
        }

        debug!("send_input_bytes: {}", cmd);
        self.send_event(TmuxCommand::Keypress, &cmd)
    }

    /// Fetch the content of a Tmux paste buffer (to sync the system clipboard)
    pub fn fetch_buffer(&self, name: &str) -> Result<(), TmuxError> {
        debug!("Fetching paste buffer: {}", name);
        let cmd = format!("show-buffer -b \"{}\"", name);
        self.send_event(TmuxCommand::FetchBuffer, &cmd)
    }

    /// Store text in a new automatic Tmux paste buffer, making it the most
    /// recent one (what prefix-] and other clients paste). Used to sync the
    /// local selection to Tmux.
    pub fn set_buffer(&self, text: &str) -> Result<(), TmuxError> {
        // Selections are typically small; anything huge would produce a
        // multi-megabyte command line and stall the UI on the blocking
        // stdin write, so leave those to Tmux copy-mode instead
        const MAX_SYNC_BYTES: usize = 256 * 1024;
        if text.len() > MAX_SYNC_BYTES {
            eprintln!(
                "Not syncing selection to Tmux: {} bytes exceeds the {} byte limit",
                text.len(),
                MAX_SYNC_BYTES
            );
            return Ok(());
        }

        // The %paste-buffer-changed this triggers is our own echo; mark it
        // so the window does not fetch it back into the system clipboard
        self.pending_buffer_echoes
            .set(self.pending_buffer_echoes.get() + 1);

        let cmd = format!("set-buffer -- \"{}\"", escape_for_tmux_quotes(text));
        self.send_event(TmuxCommand::SetBuffer, &cmd)
    }

    /// Returns true when a %paste-buffer-changed notification was caused by
    /// our own set_buffer (and consumes that marker)
    pub fn consume_buffer_echo(&self) -> bool {
        let pending = self.pending_buffer_echoes.get();
        if pending > 0 {
            self.pending_buffer_echoes.set(pending - 1);
            return true;
        }
        false
    }

    // TODO: Too many functions for sending text
    pub fn send_function_key(&self, pane_id: u32, text: &str) -> Result<(), TmuxError> {
        let cmd = format!("send-keys -t %{} -- \"{}\"", pane_id, text);

        debug!("send_function_key: {}", &cmd[..cmd.len() - 1]);
        self.send_event(TmuxCommand::Keypress, &cmd)
    }

    pub fn send_keybinding(&self, action: KeyboardAction, pane_id: u32) -> Result<(), TmuxError> {
        let (event, cmd) = match action {
            KeyboardAction::PaneSplit(horizontal) => {
                let event = TmuxCommand::PaneSplit(horizontal);
                let cmd = format!(
                    "split-window {} -t %{}",
                    if horizontal { "-v" } else { "-h" },
                    pane_id,
                );
                (event, cmd)
            }
            KeyboardAction::PaneClose => {
                let event = TmuxCommand::PaneClose(pane_id);
                let cmd = format!("kill-pane -t %{}", pane_id);
                (event, cmd)
            }
            KeyboardAction::TabNew => {
                // TODO: We should get all required layout info without having to ask directly,
                // since it would allow us to react to external commands
                let cmd = String::from(
                    "new-window -P -F \"#{window_id} #{window_layout} #{window_visible_layout} ${window_flags} #{window_name}\"",
                );
                (TmuxCommand::TabNew, cmd)
            }
            KeyboardAction::TabClose => {
                let cmd = String::from("kill-window");
                (TmuxCommand::TabClose, cmd)
            }
            KeyboardAction::TabRename => {
                // We do nothing, since Tab renaming is handled separately
                return Ok(());
            }
            KeyboardAction::MoveFocus(direction) => {
                let cmd = format!(
                    "select-pane {}",
                    match direction {
                        Direction::Down => "-D",
                        Direction::Left => "-L",
                        Direction::Right => "-R",
                        Direction::Up => "-U",
                    }
                );
                let event = TmuxCommand::PaneMoveFocus(direction);
                (event, cmd)
            }
            KeyboardAction::ToggleZoom => {
                let cmd = format!("resize-pane -Z -t %{}", pane_id);
                let event = TmuxCommand::PaneZoom(pane_id);
                (event, cmd)
            }
            KeyboardAction::CopySelected => {
                todo!();
            }
            KeyboardAction::PasteClipboard => {
                panic!("PasteClipboard keyboard event needs to be handled by Terminal widget");
            }
            KeyboardAction::OpenEditorCwd => {
                // TODO: This prints ALL panes in a Tab, not needed
                let event = TmuxCommand::PaneCurrentPath(pane_id);
                let cmd = format!(
                    "list-panes -t %{} -F \"path: #{{pane_id}} #{{pane_current_path}}\"",
                    pane_id
                );
                (event, cmd)
            }
            KeyboardAction::ClearScrollback => {
                let event = TmuxCommand::ClearScrollback(pane_id);
                let cmd = format!("clear-history -t %{}", pane_id);
                (event, cmd)
            }
            KeyboardAction::ToggleFullscreen => {
                // Fullscreen is handled by the GTK window, not tmux
                return Ok(());
            }
            KeyboardAction::FontScaleIncrease
            | KeyboardAction::FontScaleDecrease
            | KeyboardAction::FontScaleReset => {
                // Font scaling is handled by the GTK window, not tmux
                return Ok(());
            }
        };

        self.send_event(event, &cmd)
    }

    pub fn select_tab(&self, tab_id: u32) -> Result<(), TmuxError> {
        let event = TmuxCommand::TabSelect(tab_id);
        let cmd = format!("select-window -t @{}", tab_id);
        self.send_event(event, &cmd)
    }

    pub fn select_terminal(&self, term_id: u32) -> Result<(), TmuxError> {
        let event = TmuxCommand::PaneSelect(term_id);
        let cmd = format!("select-pane -t %{}", term_id);
        self.send_event(event, &cmd)
    }

    /// Updates resize_future to `new` value, while returning the old value
    pub fn update_resize_future(&self, new: bool) -> bool {
        self.resize_future.replace(new)
    }

    pub fn rename_tab(&self, tab_id: u32, name: String) -> Result<(), TmuxError> {
        // Handle newlines and escape " character
        let name = name.replace('"', "\\\"");
        let name = if let Some(newline) = name.find('\n') {
            &name[..newline]
        } else {
            &name
        };

        let event = TmuxCommand::TabRename(tab_id);
        let cmd = format!("rename-window -t @{} -- \"{}\"", tab_id, name);
        self.send_event(event, &cmd)
    }

    pub fn resize_pane(
        &self,
        term_id: u32,
        direction: Direction,
        amount: u32,
    ) -> Result<(), TmuxError> {
        let event = TmuxCommand::PaneResize(term_id);
        let direction = match direction {
            Direction::Down => "D",
            Direction::Left => "L",
            Direction::Up => "U",
            Direction::Right => "R",
        };
        let cmd = format!("resize-pane -{} -t %{} {}", direction, term_id, amount);
        self.send_event(event, &cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_passes_safe_characters_through() {
        assert_eq!(
            escape_for_tmux_quotes("hello World.txt /path:2, a=b+c@d"),
            "hello World.txt /path:2, a=b+c@d"
        );
    }

    #[test]
    fn escape_encodes_tmux_metacharacters() {
        // Quote and backslash would break out of the double-quoted string
        assert_eq!(escape_for_tmux_quotes("a\"b"), "a\\042b");
        assert_eq!(escape_for_tmux_quotes("a\\b"), "a\\134b");
        // $, #, ; and { would trigger expansion or command separation
        assert_eq!(escape_for_tmux_quotes("$HOME"), "\\044HOME");
        assert_eq!(escape_for_tmux_quotes("#{pane_id}"), "\\043\\173pane_id\\175");
        assert_eq!(escape_for_tmux_quotes("a;b"), "a\\073b");
    }

    #[test]
    fn escape_encodes_newlines_and_control_characters() {
        assert_eq!(escape_for_tmux_quotes("a\nb"), "a\\012b");
        assert_eq!(escape_for_tmux_quotes("a\tb"), "a\\011b");
        assert_eq!(escape_for_tmux_quotes("a\rb"), "a\\015b");
    }

    #[test]
    fn escape_encodes_utf8_bytes_individually() {
        // "あ" = 0xE3 0x81 0x82; Tmux reassembles the raw bytes
        assert_eq!(escape_for_tmux_quotes("あ"), "\\343\\201\\202");
    }
}
