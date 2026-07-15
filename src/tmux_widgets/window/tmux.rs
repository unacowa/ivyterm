use std::time::{Duration, Instant};

use glib::subclass::types::ObjectSubclassIsExt;
use gtk4::Orientation;
use libadwaita::{glib, prelude::*};
use log::debug;

use crate::{
    close_on_error,
    config::PredictiveEchoMode,
    helpers::borrow_clone,
    keyboard::Direction,
    tmux_api::{LayoutFlags, LayoutSync, TmuxEvent},
    tmux_widgets::{
        separator::TmuxSeparator, terminal::TmuxTerminal, toplevel::TmuxTopLevel,
        window::get_tmux_ref,
    },
};

use super::IvyTmuxWindow;

const RESIZE_TIMEOUT: Duration = Duration::from_millis(5);
/// A Keypress reply older than this is not a usable RTT sample
const RTT_SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Smoothing factor of the RTT exponential moving average
const RTT_EMA_ALPHA: f64 = 0.3;
/// In auto mode, predictions are displayed above this average RTT
const RTT_PREDICTION_THRESHOLD_MS: f64 = 50.0;

#[derive(Clone, Copy, PartialEq)]
pub enum TmuxInitState {
    SyncingLayout,
    SyncingSize,
    Done,
}

impl Default for TmuxInitState {
    fn default() -> Self {
        TmuxInitState::SyncingLayout
    }
}

// Tmux session initialization:
// 1. %session-changed (Tmux confirmed attached) triggers tmux.get_initial_layout()
// 2. We receive initial layout, which is used to construct the hierarchy
// 3. TopLevel layout.alloc_changed() triggers, which sends Tmux size sync event
// 4. After we receive Tmux size sync conformation, we start getting initial output

impl IvyTmuxWindow {
    pub fn get_char_size(&self) -> (i32, i32) {
        self.imp().char_size.get()
    }

    /// Kill a Tmux window (e.g. because the user closed its Tab)
    pub fn tmux_kill_window(&self, tab_id: u32) {
        if let Some(tmux) = get_tmux_ref(self) {
            close_on_error!(tmux.kill_window(tab_id), self);
        }
    }

    /// Send user input (as produced by VTE's `commit` signal) to Tmux
    pub fn tmux_send_input(&self, pane_id: u32, text: &str) {
        if let Some(tmux) = get_tmux_ref(self) {
            // Record the send time; the %begin/%end reply of the Keypress
            // command (KeypressAck) completes the RTT sample
            {
                let mut sent = self.imp().keypress_sent.borrow_mut();
                // Entries whose reply never came (e.g. %error) would skew
                // the FIFO pairing forever; drop stale ones
                while let Some(first) = sent.front() {
                    if first.elapsed() > RTT_SAMPLE_TIMEOUT {
                        sent.pop_front();
                    } else {
                        break;
                    }
                }
                sent.push_back(Instant::now());
            }
            close_on_error!(tmux.send_input_bytes(pane_id, text), self);
        }
    }

    /// Whether predictions should currently be displayed, given the
    /// configured mode and the measured transport RTT
    pub fn predictive_echo_active(&self, mode: PredictiveEchoMode) -> bool {
        match mode {
            PredictiveEchoMode::Off => false,
            PredictiveEchoMode::Always => true,
            PredictiveEchoMode::Auto => self.imp().echo_rtt_ms.get() > RTT_PREDICTION_THRESHOLD_MS,
        }
    }

    fn record_keypress_ack(&self) {
        let imp = self.imp();
        let sample = imp.keypress_sent.borrow_mut().pop_front();
        // An ack without a matching send (e.g. a function key sent through
        // another code path) is simply ignored
        if let Some(sent_at) = sample {
            let rtt_ms = sent_at.elapsed().as_secs_f64() * 1000.0;
            if rtt_ms > RTT_SAMPLE_TIMEOUT.as_secs_f64() * 1000.0 {
                return;
            }

            let ema = imp.echo_rtt_ms.get();
            let updated = if ema == 0.0 {
                rtt_ms
            } else {
                ema * (1.0 - RTT_EMA_ALPHA) + rtt_ms * RTT_EMA_ALPHA
            };
            imp.echo_rtt_ms.replace(updated);
        }
    }

    /// Sync locally selected text into a Tmux paste buffer, so tmux-side
    /// paste (prefix-], other clients) sees what the user selected
    pub fn tmux_sync_selection(&self, text: &str) {
        if let Some(tmux) = get_tmux_ref(self) {
            close_on_error!(tmux.set_buffer(text), self);
        }
    }


    fn tmux_sync_size(&self) {
        let imp = self.imp();
        let tab_view = borrow_clone(&imp.tab_view);
        let selected_page = tab_view.selected_page();

        if let Some(selected_page) = selected_page {
            let top_level: TmuxTopLevel = selected_page.child().downcast().unwrap();
            debug!(
                "Top Level width {} height {}",
                top_level.width(),
                top_level.height()
            );
            let (cols, rows) = top_level.get_cols_rows();

            if let Some(tmux) = get_tmux_ref(self) {
                // Tell Tmux resize future is no longer running
                tmux.update_resize_future(false);
                close_on_error!(tmux.change_size(cols, rows), self);
            }
        }
    }

    pub fn resync_tmux_size(&self) {
        // First check if a future is already running
        if let Some(tmux) = get_tmux_ref(self) {
            if tmux.update_resize_future(true) {
                // A future is already running, we can stop
                return;
            }

            glib::spawn_future_local(glib::clone!(
                #[weak(rename_to = window)]
                self,
                async move {
                    glib::timeout_future(RESIZE_TIMEOUT).await;
                    window.tmux_sync_size();
                }
            ));
        }
    }

    fn sync_tmux_layout(&self, layout_sync: LayoutSync) {
        let tab_id = layout_sync.tab_id;
        let flags = layout_sync.flags;

        let top_level = if let Some(top_level) = self.get_top_level(tab_id) {
            debug!("Reusing top Level {}", top_level.tab_id());
            top_level
        } else {
            debug!("Creating new Tab (with new top_level)");
            self.new_tab(tab_id)
        };

        // Sync Tab layout
        top_level.sync_tmux_layout(self, layout_sync);

        // If the Tab is focused, we remember that here
        if flags.contains(LayoutFlags::HasFocus) {
            self.imp().focused_tab.replace(tab_id);
        }
    }

    pub fn rename_tmux_tab(&self, tab_id: u32, name: &str) {
        if let Some(tmux) = get_tmux_ref(self) {
            close_on_error!(tmux.rename_tab(tab_id, name.to_string()), self);
        }
    }

    pub fn tmux_event_callback(&self, event: TmuxEvent) {
        let imp = self.imp();

        // If Tmux API is finished, we are not doing anything
        if imp.tmux.borrow().is_none() {
            return;
        }

        // This future runs on main thread of GTK application
        // It receives Tmux events from separate thread and runs GTK functions
        match event {
            TmuxEvent::Output(pane_id, output, initial) => {
                // Ignore Output events until initial output has been captured
                let terminals = imp.terminals.borrow();
                if let Some(pane) = terminals.get(pane_id) {
                    pane.feed_output(output, initial);
                }
            }
            TmuxEvent::KeypressAck => {
                self.record_keypress_ack();
            }
            TmuxEvent::PaneFocusChanged(tab_id, term_id) => {
                if let Some(top_level) = self.get_top_level(tab_id) {
                    top_level.select_terminal_event(term_id);
                }
            }
            TmuxEvent::TabFocusChanged(tab_id) => {
                debug!("TabFocusChanged {}", tab_id);

                let old = imp.focused_tab.replace(tab_id);
                if old != tab_id {
                    let top_level = self.get_top_level(tab_id);

                    if let Some(top_level) = top_level {
                        let tab_view = borrow_clone(&imp.tab_view);
                        let page = tab_view.page(&top_level);
                        tab_view.set_selected_page(&page);
                    }
                }
            }
            TmuxEvent::TabNew(layout_sync) => {
                debug!("\n---------- New tab ----------");
                self.sync_tmux_layout(layout_sync);
            }
            TmuxEvent::WindowAdded(tab_id) => {
                // Window created by another client; ask for its layout so the
                // Tab is created right away instead of on the next
                // %layout-change (e.g. window resize)
                if self.get_top_level(tab_id).is_none() {
                    if let Some(tmux) = get_tmux_ref(self) {
                        close_on_error!(tmux.get_window_layout(tab_id), self);
                    }
                }
            }
            TmuxEvent::TabClosed(tab_id) => {
                if let Some(top_level) = self.get_top_level(tab_id) {
                    self.close_tab(&top_level);
                }
            }
            TmuxEvent::TabRenamed(tab_id, name) => {
                let top_level = self.get_top_level(tab_id);
                if let Some(top_level) = top_level {
                    top_level.tab_renamed(&name);
                }
            }
            TmuxEvent::InitialLayout(layout_sync) => {
                // TODO: Fix Tmux not reporting which Terminal is selected in Initial Layout
                // TODO: Block resize until Tmux layout is parsed (or maybe the other way around?)
                // Also only get initial output when size + layout is OK
                // We can calculate TopLevel size: TotalSize - HeaderBar?

                debug!("\n---------- Initial layout ----------");
                self.sync_tmux_layout(layout_sync);
            }
            TmuxEvent::InitialLayoutFinished => {
                // We have initial layout, meaning we can now calculate cols&rows to sync the
                // Tmux client size
                let current_tab = imp.focused_tab.get();
                let top_level = self.get_top_level(current_tab);
                if let Some(top_level) = top_level {
                    let tab_view = borrow_clone(&imp.tab_view);
                    let page = tab_view.page(&top_level);
                    tab_view.set_selected_page(&page);
                }

                imp.init_layout_finished.replace(TmuxInitState::SyncingSize);
            }
            TmuxEvent::InitialOutputFinished(pane_id) => {
                let terminals = imp.terminals.borrow();
                if let Some(pane) = terminals.get(pane_id) {
                    pane.initial_output_finished();
                }
            }
            TmuxEvent::LayoutChanged(layout_sync) => {
                debug!("\n---------- Layout changed ----------");
                self.sync_tmux_layout(layout_sync);
            }
            TmuxEvent::SizeChanged => {
                if imp.init_layout_finished.get() == TmuxInitState::SyncingSize {
                    imp.init_layout_finished.replace(TmuxInitState::Done);

                    if let Some(tmux) = get_tmux_ref(self) {
                        // If initial output has not been captured yet, now is the time
                        let terminals = imp.terminals.borrow();
                        for sorted in terminals.iter() {
                            if let Err(_) = tmux.get_initial_output(sorted.id) {
                                drop(terminals);
                                self.close();
                                return;
                            }
                        }
                    }
                }
            }
            TmuxEvent::Exit => {
                debug!("Received EXIT event, closing window!");
                self.close();
            }
            TmuxEvent::ScrollOutput(pane_id, empty_lines) => {
                let terminals = &imp.terminals;
                if let Some(pane) = terminals.borrow().get(pane_id) {
                    pane.scroll_view(empty_lines);
                }
            }
            TmuxEvent::SessionChanged(id, name) => {
                let new = (id, name.clone());
                let old = imp.session.replace(Some((id, name)));

                // If session changes (after it was already initialized), then
                // something went wrong
                if let Some(old) = old {
                    if old != new {
                        println!("Session {} changed underneath us, closing Window", old.1);
                        self.close();
                    }
                } else {
                    // Tmux is now attached and owns the transport; it is
                    // safe to start sending commands
                    if let Some(tmux) = get_tmux_ref(self) {
                        close_on_error!(tmux.get_initial_layout(), self);
                    }
                }

                println!("Session {} with name {} initialized", new.0, new.1);
            }
            TmuxEvent::PasteBufferChanged(name) => {
                if let Some(tmux) = get_tmux_ref(self) {
                    // Skip buffers we just set from a local selection;
                    // fetching those back would clobber the system
                    // clipboard with every mouse selection
                    if !tmux.consume_buffer_echo() {
                        close_on_error!(tmux.fetch_buffer(&name), self);
                    }
                }
            }
            TmuxEvent::ClipboardText(text) => {
                // Sync Tmux paste buffer to the system clipboard
                self.clipboard().set_text(&text);
            }
            TmuxEvent::ScrollbackCleared(term_id) => {
                let terminals = &imp.terminals;
                if let Some(terminal) = terminals.borrow().get(term_id) {
                    terminal.clear_scrollback();
                }
            }
        }
    }

    pub fn initial_layout_finished(&self) -> bool {
        self.imp().init_layout_finished.get() == TmuxInitState::Done
    }

    pub fn separator_drag_sync(&self, separator: &TmuxSeparator, amount: i32) {
        let orientation = separator.orientation();
        let direction = match (amount < 0, orientation) {
            (true, Orientation::Horizontal) => Direction::Up,
            (false, Orientation::Horizontal) => Direction::Down,
            (true, _) => Direction::Left,
            (false, _) => Direction::Right,
        };
        let amount = amount.abs() as u32;

        if let Some(tmux) = get_tmux_ref(self) {
            // We need to find widget to the top/left of our separator
            let mut widget = separator.next_sibling().unwrap();
            loop {
                // Check if our sibling is a Terminal
                match widget.downcast::<TmuxTerminal>() {
                    Ok(terminal) => {
                        let id = terminal.id();
                        close_on_error!(tmux.resize_pane(id, direction, amount), self);
                        return;
                    }
                    Err(container) => {
                        widget = container.first_child().unwrap();
                    }
                };
            }
        }
    }
}
