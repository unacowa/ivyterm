use gtk4::gdk::RGBA;
use serde::{Deserialize, Serialize};
use vte4::{Terminal as Vte, TerminalExt, TerminalExtManual};

use super::{IvyColor, IvyFont};

/// When to display mosh-style predictive local echo in Tmux terminals
#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum PredictiveEchoMode {
    /// Never predict
    Off,
    /// Predict when the measured transport RTT is high
    #[default]
    Auto,
    /// Always predict
    Always,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct TerminalConfig {
    #[serde(default = "default_font")]
    pub font: IvyFont,
    #[serde(default = "default_scrollback_lines")]
    pub scrollback_lines: u32,
    /// Name of a built-in color scheme (see [`crate::config::THEME_NAMES`]).
    /// When set to a known theme it fully determines the terminal colors; the
    /// individual color fields below are used only when this is `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default = "default_foreground")]
    pub foreground: IvyColor,
    #[serde(default = "default_background")]
    pub background: IvyColor,
    #[serde(default = "default_standard_colors")]
    pub standard_colors: [IvyColor; 8],
    #[serde(default = "default_bright_colors")]
    pub bright_colors: [IvyColor; 8],
    /// Optional cursor / selection colors. Themes provide these; for a custom
    /// (no-theme) scheme they are only applied when explicitly configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<IvyColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_foreground: Option<IvyColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<IvyColor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_foreground: Option<IvyColor>,
    #[serde(default = "default_split_handle_color")]
    pub split_handle_color: IvyColor,
    #[serde(default)]
    pub terminal_bell: bool,
    #[serde(default)]
    pub predictive_echo: PredictiveEchoMode,
}

impl TerminalConfig {
    /// The background color actually shown in terminals, honoring a named
    /// [`TerminalConfig::theme`]. Used for the window/separator CSS so the gaps
    /// between split panes match the real terminal background.
    pub fn effective_background(&self) -> IvyColor {
        if let Some(name) = &self.theme {
            if let Some(theme) = super::themes::by_name(name) {
                return theme.background;
            }
        }
        self.background.clone()
    }
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font: default_font(),
            scrollback_lines: default_scrollback_lines(),
            theme: None,
            foreground: default_foreground(),
            background: default_background(),
            standard_colors: default_standard_colors(),
            bright_colors: default_bright_colors(),
            cursor: None,
            cursor_foreground: None,
            selection: None,
            selection_foreground: None,
            split_handle_color: default_split_handle_color(),
            terminal_bell: false,
            predictive_echo: PredictiveEchoMode::default(),
        }
    }
}

/// The fully-resolved set of colors applied to a VTE terminal. Resolution
/// happens in [`ColorScheme::new`]: a named [`TerminalConfig::theme`] wins when
/// present and valid, otherwise the individually-configured colors are used.
pub struct ColorScheme {
    foreground: RGBA,
    background: RGBA,
    palette: [RGBA; 16],
    cursor: Option<RGBA>,
    cursor_foreground: Option<RGBA>,
    selection: Option<RGBA>,
    selection_foreground: Option<RGBA>,
}

impl ColorScheme {
    pub fn new(config: &TerminalConfig) -> Self {
        if let Some(name) = &config.theme {
            if let Some(theme) = super::themes::by_name(name) {
                return Self::from_theme(&theme);
            }
            eprintln!(
                "Unknown terminal theme '{}', falling back to configured colors",
                name
            );
        }
        Self::from_config(config)
    }

    fn from_theme(theme: &super::themes::Theme) -> Self {
        Self {
            foreground: theme.foreground.clone().into(),
            background: theme.background.clone().into(),
            palette: palette_from(&theme.normal, &theme.bright),
            cursor: theme.cursor.clone().map(Into::into),
            cursor_foreground: theme.cursor_foreground.clone().map(Into::into),
            selection: theme.selection.clone().map(Into::into),
            selection_foreground: theme.selection_foreground.clone().map(Into::into),
        }
    }

    fn from_config(config: &TerminalConfig) -> Self {
        Self {
            foreground: config.foreground.clone().into(),
            background: config.background.clone().into(),
            palette: palette_from(&config.standard_colors, &config.bright_colors),
            cursor: config.cursor.clone().map(Into::into),
            cursor_foreground: config.cursor_foreground.clone().map(Into::into),
            selection: config.selection.clone().map(Into::into),
            selection_foreground: config.selection_foreground.clone().map(Into::into),
        }
    }

    /// Apply every resolved color to a VTE terminal in one place, so the create
    /// and live-update paths (Tmux and normal) can never drift apart.
    pub fn apply(&self, vte: &Vte) {
        let palette: Vec<&RGBA> = self.palette.iter().collect();
        vte.set_colors(Some(&self.foreground), Some(&self.background), &palette);
        vte.set_color_cursor(self.cursor.as_ref());
        vte.set_color_cursor_foreground(self.cursor_foreground.as_ref());
        vte.set_color_highlight(self.selection.as_ref());
        vte.set_color_highlight_foreground(self.selection_foreground.as_ref());
    }
}

fn palette_from(normal: &[IvyColor; 8], bright: &[IvyColor; 8]) -> [RGBA; 16] {
    let mut palette: Vec<RGBA> = Vec::with_capacity(16);
    for color in normal {
        palette.push(color.clone().into());
    }
    for color in bright {
        palette.push(color.clone().into());
    }
    palette.try_into().unwrap()
}

pub fn default_font() -> IvyFont {
    IvyFont::new("Monospace 12")
}

pub fn default_scrollback_lines() -> u32 {
    500
}

pub fn default_foreground() -> IvyColor {
    let rgba = RGBA::parse("#ffffff").unwrap();
    IvyColor(rgba)
}

pub fn default_background() -> IvyColor {
    let rgba = RGBA::parse("#000000").unwrap();
    IvyColor(rgba)
}

pub fn default_split_handle_color() -> IvyColor {
    let rgba = RGBA::parse("#ffffff").unwrap();
    IvyColor(rgba)
}

pub fn default_standard_colors() -> [IvyColor; 8] {
    [
        IvyColor(RGBA::parse("#2e3436").unwrap()),
        IvyColor(RGBA::parse("#cc0000").unwrap()),
        IvyColor(RGBA::parse("#4e9a06").unwrap()),
        IvyColor(RGBA::parse("#c4a000").unwrap()),
        IvyColor(RGBA::parse("#3465a4").unwrap()),
        IvyColor(RGBA::parse("#75507b").unwrap()),
        IvyColor(RGBA::parse("#06989a").unwrap()),
        IvyColor(RGBA::parse("#d3d7cf").unwrap()),
    ]
}

pub fn default_bright_colors() -> [IvyColor; 8] {
    [
        IvyColor(RGBA::parse("#555753").unwrap()),
        IvyColor(RGBA::parse("#ef2929").unwrap()),
        IvyColor(RGBA::parse("#8ae234").unwrap()),
        IvyColor(RGBA::parse("#fce94f").unwrap()),
        IvyColor(RGBA::parse("#729fcf").unwrap()),
        IvyColor(RGBA::parse("#ad7fa8").unwrap()),
        IvyColor(RGBA::parse("#34e2e2").unwrap()),
        IvyColor(RGBA::parse("#eeeeec").unwrap()),
    ]
}
