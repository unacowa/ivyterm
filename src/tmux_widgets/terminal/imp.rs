use std::cell::{Cell, RefCell};

use gtk4::DrawingArea;
use libadwaita::{glib, prelude::*, subclass::prelude::*};
use vte4::Terminal as Vte;

use crate::config::PredictiveEchoMode;

use super::prediction::PredictionState;

// Object holding the state
#[derive(Default)]
pub struct TerminalPriv {
    pub vte: RefCell<Option<Vte>>,
    pub id: Cell<u32>,
    initial_output: Cell<bool>,
    /// True while a debounced selection -> Tmux buffer sync is scheduled
    /// (`selection-changed` fires on every pointer motion during a drag)
    pub selection_sync_scheduled: Cell<bool>,
    /// Escape sequence split across %output chunks, held back until the
    /// rest arrives (see filter_mouse_tracking)
    pub pending_escape: RefCell<Vec<u8>>,
    /// Predictive echo state (mosh-style local echo)
    pub prediction: RefCell<PredictionState>,
    /// Overlay the predictions are drawn on
    pub prediction_area: RefCell<Option<DrawingArea>>,
    pub predictive_mode: Cell<PredictiveEchoMode>,
    /// Foreground/background colors for drawing predictions (from config)
    pub prediction_fg: Cell<(f64, f64, f64)>,
    pub prediction_bg: Cell<(f64, f64, f64)>,
}

// The central trait for subclassing a GObject
#[glib::object_subclass]
impl ObjectSubclass for TerminalPriv {
    const NAME: &'static str = "ivytermTmuxTerminal";
    type Type = super::TmuxTerminal;
    type ParentType = libadwaita::Bin;
}

// Trait shared by all GObjects
impl ObjectImpl for TerminalPriv {
    fn dispose(&self) {
        self.vte.take();
        self.prediction_area.take();
    }
}

// Trait shared by all widgets
impl WidgetImpl for TerminalPriv {
    fn grab_focus(&self) -> bool {
        self.parent_grab_focus();

        self.vte.borrow().as_ref().unwrap().grab_focus()
    }
}

// Trait shared by all buttons
impl BinImpl for TerminalPriv {}

impl TerminalPriv {
    pub fn init_values(&self, id: u32, terminal: &Vte) {
        self.id.replace(id);
        self.vte.borrow_mut().replace(terminal.clone());
    }

    pub fn is_synced(&self) -> bool {
        self.initial_output.get()
    }

    pub fn set_synced(&self) {
        self.initial_output.replace(true);
    }
}
