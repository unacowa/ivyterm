mod imp;
mod prediction;

use glib::{subclass::types::ObjectSubclassIsExt, Object, Propagation};
use gtk4::{
    cairo, gdk,
    gdk::{ModifierType, BUTTON_PRIMARY},
    gio, pango, DrawingArea, EventControllerKey, GestureClick, Overlay, PropagationPhase,
    ScrolledWindow,
};
use libadwaita::{glib, prelude::*};
use prediction::{
    classify_commit, confirmed_graphemes, grapheme_offset, layout_cells, pop_grapheme,
    CommitKind, PREDICTION_TIMEOUT,
};
use unicode_width::UnicodeWidthStr;
use vte4::{Format, Regex, Terminal as Vte, TerminalExt};

use crate::{
    application::IvyApplication,
    config::{ColorScheme, TerminalConfig},
    helpers::{borrow_clone, PCRE2_MULTILINE, URL_REGEX_STRINGS},
    keyboard::KeyboardAction,
    unwrap_or_return,
};

use super::{toplevel::TmuxTopLevel, IvyTmuxWindow};

/// VTE's fixed inner border (see also TmuxTopLevel::get_cols_rows)
const VTE_PADDING: f64 = 1.0;

fn rgba_to_tuple(rgba: &gdk::RGBA) -> (f64, f64, f64) {
    (rgba.red() as f64, rgba.green() as f64, rgba.blue() as f64)
}

glib::wrapper! {
    pub struct TmuxTerminal(ObjectSubclass<imp::TerminalPriv>)
        @extends libadwaita::Bin, gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Actionable, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl TmuxTerminal {
    pub fn new(top_level: &TmuxTopLevel, window: &IvyTmuxWindow, pane_id: u32) -> Self {
        let app = window.application().unwrap();
        let app: IvyApplication = app.downcast().unwrap();

        // Get terminal font
        let config = app.get_terminal_config();

        let vte = Vte::builder()
            .vexpand(true)
            .hexpand(true)
            .font_desc(config.font.as_ref())
            .audible_bell(config.terminal_bell)
            .scrollback_lines(config.scrollback_lines)
            .allow_hyperlink(true)
            .build();

        // Add scrollbar
        let scrolled = ScrolledWindow::builder()
            .child(&vte)
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Always)
            .build();

        // Create self
        let terminal: Self = Object::builder().build();

        // Predictions (mosh-style local echo) are drawn on an overlay above
        // the terminal, leaving the VTE buffer untouched (Tmux stays the
        // single source of truth for the screen content)
        let prediction_area = DrawingArea::new();
        prediction_area.set_can_target(false);
        let overlay = Overlay::new();
        overlay.set_child(Some(&scrolled));
        overlay.add_overlay(&prediction_area);
        terminal.set_child(Some(&overlay));

        terminal.imp().init_values(pane_id, &vte);
        {
            let imp = terminal.imp();
            imp.prediction_area.replace(Some(prediction_area.clone()));
            imp.predictive_mode.set(config.predictive_echo);
            imp.prediction_fg.set(rgba_to_tuple(config.foreground.as_ref()));
            imp.prediction_bg.set(rgba_to_tuple(config.background.as_ref()));
        }

        prediction_area.set_draw_func(glib::clone!(
            #[weak]
            terminal,
            move |_, cr, _, _| {
                terminal.draw_predictions(cr);
            }
        ));

        // Reconcile predictions against the authoritative screen content.
        // This must run from these signals: vte.feed() only queues bytes,
        // the screen updates later in the main loop
        vte.connect_contents_changed(glib::clone!(
            #[weak]
            terminal,
            move |vte| {
                terminal.reconcile_predictions(vte);
            }
        ));
        vte.connect_cursor_moved(glib::clone!(
            #[weak]
            terminal,
            move |vte| {
                terminal.reconcile_predictions(vte);
            }
        ));

        // The overlay draws at fixed pixels; repaint when the view scrolls
        if let Some(adjustment) = vte.vadjustment() {
            adjustment.connect_value_changed(glib::clone!(
                #[weak]
                prediction_area,
                move |_| {
                    prediction_area.queue_draw();
                }
            ));
        }

        if window.initial_layout_finished() {
            terminal.imp().set_synced();
        }

        // Add terminal to top level terminal list
        top_level.register_terminal(&terminal);

        // Set terminal colors
        ColorScheme::new(&config).apply(&vte);

        // The Tmux VTE widget has no PTY; input handling (keymap translation,
        // IME filtering, inline preedit display, caret location reporting) is
        // left to VTE itself. VTE emits the translated input bytes on the
        // `commit` signal even without a PTY, which we forward to Tmux.
        // Predictive echo tracks the same bytes; the forwarded input is
        // never modified
        vte.connect_commit(glib::clone!(
            #[weak]
            window,
            #[weak]
            terminal,
            move |vte, text, _| {
                terminal.predict_input(vte, &window, text);
                window.tmux_send_input(pane_id, text);
            }
        ));

        vte.connect_has_focus_notify(glib::clone!(
            #[weak]
            top_level,
            move |vte| {
                if vte.has_focus() {
                    // Notify TopLevel that the focused terminal changed
                    top_level.gtk_terminal_focus_changed(pane_id);
                }
            }
        ));

        // Keep the Tmux paste buffer in sync with the local mouse selection,
        // so tmux-side paste (prefix-], other clients) sees what the user
        // selected. The selection already owns PRIMARY (VTE does that), which
        // is what middle-click paste uses. Debounced, since the signal fires
        // on every pointer motion while dragging out a selection
        vte.connect_selection_changed(glib::clone!(
            #[weak]
            terminal,
            #[weak]
            window,
            move |vte| {
                if !vte.has_selection() {
                    // The selection was cleared; keep the last synced buffer
                    return;
                }
                if terminal.imp().selection_sync_scheduled.replace(true) {
                    return;
                }

                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(200),
                    glib::clone!(
                        #[weak]
                        vte,
                        #[weak]
                        terminal,
                        #[weak]
                        window,
                        move || {
                            terminal.imp().selection_sync_scheduled.replace(false);
                            if !vte.has_selection() {
                                return;
                            }
                            if let Some(text) = vte.text_selected(vte4::Format::Text) {
                                window.tmux_sync_selection(&text);
                            }
                        }
                    ),
                );
            }
        ));

        let eventctl = EventControllerKey::new();
        // Run in the capture phase, so keybindings take priority over VTE's
        // own key handling (IME, keymap translation -> `commit`)
        eventctl.set_propagation_phase(PropagationPhase::Capture);
        eventctl.connect_key_pressed(glib::clone!(
            #[weak]
            vte,
            #[weak]
            top_level,
            #[weak]
            window,
            #[upgrade_or]
            Propagation::Proceed,
            move |eventctl, _, _, _| {
                if let Some(event) = eventctl.current_event() {
                    // Check if pressed keys match a keybinding
                    if let Some(action) = app.handle_keyboard_event(event) {
                        handle_keyboard_event(action, &vte, pane_id, &top_level, &window);
                        return Propagation::Stop;
                    }
                }
                Propagation::Proceed
            }
        ));
        vte.add_controller(eventctl);

        // Add Regex to recognize URLs
        for regex in URL_REGEX_STRINGS {
            let regex = Regex::for_match(regex, PCRE2_MULTILINE).expect("Unable to parse regex");
            let tag = vte.match_add_regex(&regex, 0);
            vte.match_set_cursor_name(tag, "pointer");
        }

        // Allow user to open URLs with Ctrl + click
        let click_ctrl = GestureClick::builder().button(0).build();
        click_ctrl.connect_pressed(glib::clone!(
            #[weak]
            vte,
            move |click_ctrl, n_clicked, x, y| {
                let button = click_ctrl.current_button();
                match button {
                    BUTTON_PRIMARY => {
                        /* Allow user to Ctrl+click URLs */
                        if n_clicked != 1 {
                            return;
                        }

                        // Links are only clickable when user holds Ctrl
                        let event = unwrap_or_return!(click_ctrl.current_event());
                        let state = event.modifier_state();
                        if !state.contains(ModifierType::CONTROL_MASK) {
                            return;
                        }

                        // Open the URL
                        let (url, _) = vte.check_match_at(x, y);
                        let url = unwrap_or_return!(url);
                        match gio::AppInfo::launch_default_for_uri(
                            &url,
                            None::<&gio::AppLaunchContext>,
                        ) {
                            Ok(_) => {}
                            Err(err) => eprintln!("Cannot open URL ({}): {}", url, err),
                        }
                    }
                    // Middle click paste (PRIMARY selection) is handled by
                    // VTE itself (as long as the GTK setting
                    // gtk-enable-primary-paste is on) and forwarded to Tmux
                    // via the `commit` signal
                    _ => {}
                }
            }
        ));
        vte.add_controller(click_ctrl);

        terminal
    }

    pub fn id(&self) -> u32 {
        self.imp().id.get()
    }

    pub fn update_config(&self, config: &TerminalConfig) {
        let imp = self.imp();
        let vte = borrow_clone(&imp.vte);

        vte.set_font(Some(config.font.as_ref()));
        ColorScheme::new(config).apply(&vte);
        vte.set_scrollback_lines(config.scrollback_lines as i64);
        vte.set_audible_bell(config.terminal_bell);

        imp.predictive_mode.set(config.predictive_echo);
        imp.prediction_fg.set(rgba_to_tuple(config.foreground.as_ref()));
        imp.prediction_bg.set(rgba_to_tuple(config.background.as_ref()));
        self.queue_prediction_draw();
    }

    pub fn feed_output(&self, output: Vec<u8>, initial: bool) {
        let imp = self.imp();

        if initial == false && imp.is_synced() == false {
            // Regular output, but we are NOT yet synced!
            return;
        }

        // Drop mouse tracking requests from pane applications before they
        // reach VTE: with mouse tracking enabled VTE stops doing local
        // selection and PRIMARY paste, handing the mouse to the application
        // instead. In ivyterm, select-to-copy and middle-click paste always
        // belong to the terminal
        let filtered = {
            let mut pending = imp.pending_escape.borrow_mut();
            filter_mouse_tracking(&mut pending, &output)
        };

        let vte = borrow_clone(&imp.vte);
        vte.feed(&filtered);
    }

    pub fn initial_output_finished(&self) {
        self.imp().set_synced();
    }

    pub fn scroll_view(&self, empty_lines: usize) {
        if empty_lines < 1 {
            return;
        }

        let mut output = Vec::with_capacity(empty_lines + 16);
        // Scroll down 'empty_lines' lines
        for _ in 0..empty_lines {
            output.push(b'\n');
        }
        // Scroll back up '# = empty_lines' lines using ESC[#A
        output.push(b'\x1b');
        output.push(b'[');
        for d in empty_lines.to_string().as_bytes() {
            output.push(*d);
        }
        output.push(b'A');

        self.feed_output(output, false);
    }

    pub fn get_cols_or_rows(&self) -> (i64, i64) {
        let vte = borrow_clone(&self.imp().vte);
        let cols = vte.column_count();
        let rows = vte.row_count();
        (cols, rows)
    }

    pub fn get_char_width_height(&self) -> (i32, i32) {
        let vte = borrow_clone(&self.imp().vte);
        (vte.char_width() as i32, vte.char_height() as i32)
    }

    pub fn font_scale(&self) -> f64 {
        borrow_clone(&self.imp().vte).font_scale()
    }

    pub fn set_font_scale(&self, scale: f64) {
        borrow_clone(&self.imp().vte).set_font_scale(scale);
        // Cell dimensions changed; predictions are drawn in cell units
        self.queue_prediction_draw();
    }

    pub fn clear_scrollback(&self) {
        let clear_scrollback = [b'\x1b', b'[', b'3', b'J'];
        let vte = borrow_clone(&self.imp().vte);
        vte.feed(&clear_scrollback);
    }

    /// Updates the predictive echo state for one `commit` chunk (the bytes
    /// themselves are forwarded to Tmux unmodified by the caller)
    fn predict_input(&self, vte: &Vte, window: &IvyTmuxWindow, text: &str) {
        let imp = self.imp();

        match classify_commit(text) {
            CommitKind::Append => {
                if !window.predictive_echo_active(imp.predictive_mode.get()) {
                    return;
                }

                let generation = {
                    let mut state = imp.prediction.borrow_mut();
                    if state.text.is_empty() {
                        state.origin = vte.cursor_position();
                    }
                    state.text.push_str(text);
                    state.generation += 1;
                    state.generation
                };
                self.queue_prediction_draw();
                self.arm_prediction_timeout(generation);
            }
            CommitKind::Backspace => {
                let popped = {
                    let mut state = imp.prediction.borrow_mut();
                    let popped = pop_grapheme(&mut state.text);
                    if popped {
                        state.generation += 1;
                    }
                    popped
                };
                if popped {
                    self.queue_prediction_draw();
                }
            }
            CommitKind::Control => {
                // Control keys (Enter, arrows, Ctrl-*, pastes) have
                // unpredictable screen effects
                self.discard_predictions();
            }
        }
    }

    /// Matches pending predictions against the actual screen content.
    /// Confirmed graphemes are removed (the real echo now covers them);
    /// a mismatch discards everything
    fn reconcile_predictions(&self, vte: &Vte) {
        let imp = self.imp();
        let mut state = imp.prediction.borrow_mut();
        if state.text.is_empty() {
            return;
        }

        let cols = vte.column_count() as i64;
        if cols < 1 {
            return;
        }

        // Read the authoritative screen text from the prediction origin
        // through the rows the prediction spans. Passing origin.0 as
        // start_col (instead of indexing row text) keeps cell/character
        // positions aligned even with wide glyphs left of the origin
        let (_, (_, row_span)) = layout_cells(&state.text, state.origin.0, cols);
        let (screen, _) = vte.text_range_format(
            Format::Text,
            state.origin.1,
            state.origin.0,
            state.origin.1 + row_span,
            cols - 1,
        );
        let screen = screen.map(|text| text.to_string()).unwrap_or_default();

        let confirmed_count = confirmed_graphemes(&screen, &state.text);
        if confirmed_count > 0 {
            let offset = grapheme_offset(&state.text, confirmed_count);
            let confirmed: String = state.text.drain(..offset).collect();
            state.origin = prediction::advance_origin(state.origin, &confirmed, cols);
            // The remote is echoing what we predicted; from now on (until
            // the next discard) predictions may be displayed
            state.display_unlocked = true;
            state.generation += 1;

            let generation = state.generation;
            let pending = !state.text.is_empty();
            drop(state);
            self.queue_prediction_draw();
            if pending {
                self.arm_prediction_timeout(generation);
            }
        } else if vte.cursor_position() != state.origin {
            // The screen changed somewhere else (echo landed elsewhere,
            // full redraw, scroll): our prediction is wrong
            state.discard();
            drop(state);
            self.queue_prediction_draw();
        }
    }

    fn discard_predictions(&self) {
        let imp = self.imp();
        let had_state = {
            let mut state = imp.prediction.borrow_mut();
            let had_state = !state.text.is_empty() || state.display_unlocked;
            state.discard();
            had_state
        };
        if had_state {
            self.queue_prediction_draw();
        }
    }

    /// Discards the prediction if it is still unconfirmed after
    /// PREDICTION_TIMEOUT (e.g. input that produces no echo at all)
    fn arm_prediction_timeout(&self, generation: u64) {
        glib::timeout_add_local_once(
            PREDICTION_TIMEOUT,
            glib::clone!(
                #[weak(rename_to = terminal)]
                self,
                move || {
                    let stale = {
                        let state = terminal.imp().prediction.borrow();
                        state.generation == generation && !state.text.is_empty()
                    };
                    if stale {
                        terminal.discard_predictions();
                    }
                }
            ),
        );
    }

    fn queue_prediction_draw(&self) {
        if let Some(area) = self.imp().prediction_area.borrow().as_ref() {
            area.queue_draw();
        }
    }

    fn draw_predictions(&self, cr: &cairo::Context) {
        let imp = self.imp();
        let state = imp.prediction.borrow();
        // display_unlocked keeps input without any echo (password prompts)
        // off the screen
        if state.text.is_empty() || !state.display_unlocked {
            return;
        }

        let vte = borrow_clone(&imp.vte);
        let Some(adjustment) = vte.vadjustment() else {
            return;
        };
        // While the user looks at the scrollback, the cursor (and therefore
        // the prediction) is off-screen; don't paint over history
        if adjustment.value() + adjustment.page_size() < adjustment.upper() - 0.5 {
            return;
        }

        let char_width = vte.char_width() as f64;
        let char_height = vte.char_height() as f64;
        let cols = vte.column_count() as i64;
        if char_width <= 0.0 || char_height <= 0.0 || cols < 1 {
            return;
        }

        let (fg_r, fg_g, fg_b) = imp.prediction_fg.get();
        let (bg_r, bg_g, bg_b) = imp.prediction_bg.get();
        let base_y = (state.origin.1 as f64 - adjustment.value()) * char_height + VTE_PADDING;

        // Underlined text, same font as the terminal
        let layout = pangocairo::functions::create_layout(cr);
        if let Some(font) = vte.font() {
            let mut font = font.clone();
            if font.size() > 0 {
                font.set_size((font.size() as f64 * vte.font_scale()) as i32);
            }
            layout.set_font_description(Some(&font));
        }
        let attributes = pango::AttrList::new();
        attributes.insert(pango::AttrInt::new_underline(pango::Underline::Single));
        layout.set_attributes(Some(&attributes));

        let (cells, caret) = layout_cells(&state.text, state.origin.0, cols);
        for (grapheme, col, row_delta) in cells {
            let x = col as f64 * char_width + VTE_PADDING;
            let y = base_y + row_delta as f64 * char_height;
            let cell_width = (grapheme.width().max(1)) as f64 * char_width;

            // Fill the cell background first: VTE draws its block cursor in
            // the foreground color at the origin cell, which would swallow
            // a glyph drawn directly on top of it
            cr.set_source_rgb(bg_r, bg_g, bg_b);
            cr.rectangle(x, y, cell_width, char_height);
            let _ = cr.fill();

            cr.set_source_rgb(fg_r, fg_g, fg_b);
            cr.move_to(x, y);
            layout.set_text(grapheme);
            pangocairo::functions::show_layout(cr, &layout);
        }

        // The caret sits at the end of the prediction
        let caret_x = caret.0 as f64 * char_width + VTE_PADDING;
        let caret_y = base_y + caret.1 as f64 * char_height;
        cr.set_source_rgb(fg_r, fg_g, fg_b);
        cr.rectangle(caret_x, caret_y, 2.0, char_height);
        let _ = cr.fill();
    }
}

#[inline]
fn handle_keyboard_event(
    action: KeyboardAction,
    vte: &Vte,
    pane_id: u32,
    top_level: &TmuxTopLevel,
    window: &IvyTmuxWindow,
) {
    match action {
        KeyboardAction::CopySelected => {
            vte.emit_copy_clipboard();
        }
        KeyboardAction::PasteClipboard => {
            // VTE wraps the paste in bracketed paste markers when the pane
            // application enabled them; forwarded via the `commit` signal
            vte.paste_clipboard();
        }
        KeyboardAction::TabRename => {
            top_level.open_rename_modal();
        }
        KeyboardAction::ToggleFullscreen => {
            if window.is_fullscreen() {
                window.unfullscreen();
            } else {
                window.fullscreen();
            }
        }
        KeyboardAction::FontScaleIncrease => {
            window.adjust_font_scale(1);
        }
        KeyboardAction::FontScaleDecrease => {
            window.adjust_font_scale(-1);
        }
        KeyboardAction::FontScaleReset => {
            window.adjust_font_scale(0);
        }
        _ => {
            window.tmux_handle_keybinding(action, pane_id);
        }
    }
}

/// DECSET/DECRST parameters that switch a terminal into mouse tracking
/// (X10, normal, button-event, any-event tracking and the UTF-8/SGR/urxvt/
/// SGR-pixel encodings)
fn is_mouse_tracking_param(param: &[u8]) -> bool {
    matches!(
        param,
        b"9" | b"1000" | b"1001" | b"1002" | b"1003" | b"1005" | b"1006" | b"1015" | b"1016"
    )
}

/// An escape sequence this long is not a DECSET we care about; stop holding
/// it back and pass it through
const MAX_ESCAPE_HOLD: usize = 64;

/// Removes mouse tracking DECSET/DECRST requests (e.g. \x1b[?1002;1006h)
/// from a Tmux %output chunk, keeping all other sequences (?2004 bracketed
/// paste, ?1049 alternate screen, ...) intact. A sequence may be split
/// across chunks; the incomplete tail is stashed in `pending` and processed
/// together with the next chunk.
fn filter_mouse_tracking(pending: &mut Vec<u8>, chunk: &[u8]) -> Vec<u8> {
    let joined: Vec<u8>;
    let input: &[u8] = if pending.is_empty() {
        chunk
    } else {
        let mut data = std::mem::take(pending);
        data.extend_from_slice(chunk);
        joined = data;
        &joined
    };

    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        // Copy everything up to the next escape character verbatim
        match input[i..].iter().position(|byte| *byte == 0x1b) {
            Some(offset) => {
                output.extend_from_slice(&input[i..i + offset]);
                i += offset;
            }
            None => {
                output.extend_from_slice(&input[i..]);
                break;
            }
        }

        let sequence = &input[i..];
        // Not "ESC [ ?": pass the escape through and keep scanning behind it
        if sequence.len() >= 2 && sequence[1] != b'[' {
            output.push(0x1b);
            i += 1;
            continue;
        }
        if sequence.len() >= 3 && sequence[2] != b'?' {
            output.extend_from_slice(&sequence[..2]);
            i += 2;
            continue;
        }

        // "ESC [ ?" so far; find the final byte of the sequence
        let mut end = None;
        for (j, byte) in sequence.iter().enumerate().skip(3) {
            if !byte.is_ascii_digit() && *byte != b';' {
                end = Some(j);
                break;
            }
        }

        match end {
            Some(end) if sequence[end] == b'h' || sequence[end] == b'l' => {
                // A private mode set/reset: filter out the mouse parameters
                let final_byte = sequence[end] as char;
                let kept: Vec<&[u8]> = sequence[3..end]
                    .split(|byte| *byte == b';')
                    .filter(|param| !is_mouse_tracking_param(param))
                    .collect();
                if !kept.is_empty() {
                    output.extend_from_slice(b"\x1b[?");
                    output.extend_from_slice(&kept.join(&b';'));
                    output.push(final_byte as u8);
                }
                i += end + 1;
            }
            Some(end) => {
                // Some other private sequence; pass it through verbatim
                output.extend_from_slice(&sequence[..end + 1]);
                i += end + 1;
            }
            None if sequence.len() < MAX_ESCAPE_HOLD => {
                // The chunk ends in the middle of the sequence (this also
                // covers a trailing lone ESC or "ESC ["); wait for the rest
                pending.extend_from_slice(sequence);
                break;
            }
            None => {
                // Unreasonably long; not something we filter
                output.extend_from_slice(sequence);
                break;
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::filter_mouse_tracking;

    fn filter_all(chunks: &[&[u8]]) -> Vec<u8> {
        let mut pending = Vec::new();
        let mut output = Vec::new();
        for chunk in chunks {
            output.extend_from_slice(&filter_mouse_tracking(&mut pending, chunk));
        }
        // Nothing may be lost: a held-back tail belongs to the next chunk,
        // which in these tests never comes
        output.extend_from_slice(&pending);
        output
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(filter_all(&[b"hello \x1b[1;31mworld\x1b[0m"]), b"hello \x1b[1;31mworld\x1b[0m");
    }

    #[test]
    fn mouse_tracking_requests_are_dropped() {
        assert_eq!(filter_all(&[b"a\x1b[?1002hb"]), b"ab");
        assert_eq!(filter_all(&[b"a\x1b[?1000;1006hb"]), b"ab");
        assert_eq!(filter_all(&[b"a\x1b[?1003;1015lb"]), b"ab");
        assert_eq!(filter_all(&[b"a\x1b[?9hb"]), b"ab");
    }

    #[test]
    fn other_private_modes_are_kept() {
        assert_eq!(filter_all(&[b"\x1b[?2004h"]), b"\x1b[?2004h");
        assert_eq!(filter_all(&[b"\x1b[?1049h"]), b"\x1b[?1049h");
        assert_eq!(filter_all(&[b"\x1b[?25l"]), b"\x1b[?25l");
    }

    #[test]
    fn mixed_parameters_keep_the_non_mouse_ones() {
        assert_eq!(filter_all(&[b"\x1b[?1002;2004h"]), b"\x1b[?2004h");
        assert_eq!(filter_all(&[b"\x1b[?2004;1006;25h"]), b"\x1b[?2004;25h");
    }

    #[test]
    fn sequences_split_across_chunks_are_still_filtered() {
        assert_eq!(filter_all(&[b"a\x1b[?10", b"02hb"]), b"ab");
        assert_eq!(filter_all(&[b"a\x1b", b"[?1006;100", b"0hb"]), b"ab");
        assert_eq!(filter_all(&[b"a\x1b", b"[?2004hb"]), b"a\x1b[?2004hb");
    }

    #[test]
    fn non_decset_escapes_pass_through_unharmed() {
        // Cursor movement, OSC, charset selection
        assert_eq!(filter_all(&[b"\x1b[10;20H"]), b"\x1b[10;20H");
        assert_eq!(filter_all(&[b"\x1b]0;title\x07"]), b"\x1b]0;title\x07");
        assert_eq!(filter_all(&[b"\x1b(B"]), b"\x1b(B");
    }

    #[test]
    fn overlong_private_sequence_is_not_held_forever() {
        let mut data = b"\x1b[?".to_vec();
        data.extend_from_slice(&[b'1'; 100]);
        assert_eq!(filter_all(&[&data]), data);
    }
}
