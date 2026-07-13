mod keybindings;


pub use keybindings::{check_keybinding_match, Keybinding, Keybindings};

#[derive(Clone, PartialEq, Debug, Copy)]
pub enum Direction {
    Left,
    Up,
    Right,
    Down,
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum KeyboardAction {
    TabNew,
    TabClose,
    TabRename,
    PaneSplit(bool),
    PaneClose,
    // TODO: Correct naming
    MoveFocus(Direction),
    ToggleZoom,
    ToggleFullscreen,
    CopySelected,
    PasteClipboard,
    OpenEditorCwd,
    ClearScrollback,
    FontScaleIncrease,
    FontScaleDecrease,
    FontScaleReset,
}

