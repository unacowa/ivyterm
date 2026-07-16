use std::{
    cell::RefCell,
    process::{Command, Stdio},
};

use const_format::concatcp;
use gtk4::prelude::*;

/// Applies a composed badge texture to a window's Wayland toplevel via
/// gdk_toplevel_set_icon_list (per-window, pixel-based — bypasses the icon
/// theme/name lookup, which a running compositor caches unreliably). The
/// toplevel surface only exists once the window is realized, so this defers
/// to the realize signal. None leaves the window on the base application icon.
pub fn apply_window_icon(window: &impl IsA<gtk4::Window>, texture: Option<gtk4::gdk::Texture>) {
    let Some(texture) = texture else {
        return;
    };
    let window: gtk4::Window = window.clone().upcast();
    window.connect_realize(move |window| {
        if let Some(surface) = window.surface() {
            if let Ok(toplevel) = surface.downcast::<gtk4::gdk::Toplevel>() {
                toplevel.set_icon_list(std::slice::from_ref(&texture));
            }
        }
    });
}

#[derive(thiserror::Error, Debug)]
pub enum IvyError {
    #[error("executing remote Tmux command failed")]
    TmuxSpawnFailed = 0,
}

#[derive(Debug)]
pub enum TmuxError {
    EventChannelClosed,
    ExitEventReceived,
    ErrorParsingUTF8,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WithId<T> {
    pub id: u32,
    pub terminal: T,
}

impl<T: PartialEq + Eq> PartialOrd for WithId<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Eq> Ord for WithId<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

pub struct SortedVec<T> {
    terminals: Vec<WithId<T>>,
}

impl<T> Default for SortedVec<T> {
    fn default() -> Self {
        Self { terminals: vec![] }
    }
}

impl<T: Eq + Clone> SortedVec<T> {
    pub fn insert(&mut self, id: u32, terminal: &T) -> usize {
        let terminal = WithId {
            id: id,
            terminal: terminal.clone(),
        };

        let insert_at = match self.terminals.binary_search(&terminal) {
            Ok(insert_at) | Err(insert_at) => insert_at,
        };
        self.terminals.insert(insert_at, terminal);
        insert_at
    }

    pub fn push(&mut self, id: u32, terminal: &T) -> usize {
        let sorted_terminal = WithId {
            id: id,
            terminal: terminal.clone(),
        };

        if let Some(last) = self.terminals.last() {
            let cmp = sorted_terminal.cmp(last);
            if cmp == std::cmp::Ordering::Greater || cmp == std::cmp::Ordering::Equal {
                // The new element is greater than or equal to the current last element,
                // so we can simply push it onto the vec.
                self.terminals.push(sorted_terminal);
                self.terminals.len() - 1
            } else {
                // The new element is less than the last element in the container, so we
                // cannot simply push. We will fall back on the normal insert behavior.
                self.insert(id, terminal)
            }
        } else {
            // If there is no last element then the container must be empty, so we
            // can simply push the element and return its index, which must be 0.
            self.terminals.push(sorted_terminal);
            0
        }
    }

    pub fn remove(&mut self, id: u32) -> Option<T> {
        match self
            .terminals
            .binary_search_by(|terminal| terminal.id.cmp(&id))
        {
            Ok(index) => Some(self.terminals.remove(index).terminal),
            Err(_) => None,
        }
    }

    pub fn get(&self, id: u32) -> Option<T> {
        match self
            .terminals
            .binary_search_by(|terminal| terminal.id.cmp(&id))
        {
            Ok(index) => Some(self.terminals[index].terminal.clone()),
            Err(_) => None,
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, WithId<T>> {
        self.terminals.iter()
    }

    pub fn len(&self) -> usize {
        self.terminals.len()
    }

    pub fn retain<F>(&mut self, f: F)
    where
        F: FnMut(&WithId<T>) -> bool,
    {
        self.terminals.retain(f);
    }

    pub fn clear(&mut self) {
        self.terminals.clear();
    }
}

pub fn open_editor(path: &str) {
    if path.is_empty() {
        return;
    }

    println!("Opening editor in path: {}", path);

    let mut command = Command::new("code");
    // Redirect stdin/stdout/stderr to /dev/null (we don't care about it)
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    // The pane path comes from Tmux (possibly a remote session); "--" keeps
    // a path that looks like an option (e.g. "--folder-uri=...") positional
    command.arg("--").arg(path);

    // Spawn editor
    match command.spawn() {
        Err(err) => {
            eprintln!("Error opening editor: {}", err);
        }
        _ => {}
    }
}

const USERCHARS: &str = "-[:alnum:]";
const USERCHARS_CLASS: &str = concatcp!("[", USERCHARS, "]");
const PASSCHARS_CLASS: &str = "[-[:alnum:]\\Q,?;.:/!%$^*&~\"#'\\E]";
const HOSTCHARS_CLASS: &str = "[-[:alnum:]]";
const HOST: &str = concatcp!(HOSTCHARS_CLASS, "+(\\.", HOSTCHARS_CLASS, "+)*");
const PORT: &str = "(?:\\:[[:digit:]]{1,5})?";
const PATHCHARS_CLASS: &str = "[-[:alnum:]\\Q_$.+!*,;:@&=?/~#%\\E]";
const PATHTERM_CLASS: &str = "[^\\Q]'.}>) \t\r\n,\"\\E]";
const SCHEME: &str = concatcp!(
    "(?:news:|telnet:|nntp:|file:\\/|https?:|ftps?:|sftp:|webcal:",
    "|irc:|sftp:|ldaps?:|nfs:|smb:|rsync:|ssh:|rlogin:|telnet:|git:",
    "|git\\+ssh:|bzr:|bzr\\+ssh:|svn:|svn\\+ssh:|hg:|mailto:|magnet:)"
);

const USERPASS: &str = concatcp!(USERCHARS_CLASS, "+(?:", PASSCHARS_CLASS, "+)?");
const URLPATH: &str = concatcp!(
    "(?:(/",
    PATHCHARS_CLASS,
    "+(?:[(]",
    PATHCHARS_CLASS,
    "*[)])*",
    PATHCHARS_CLASS,
    "*)*",
    PATHTERM_CLASS,
    ")?"
);

pub const URL_REGEX_STRINGS: [&str; 5] = [
    concatcp!(SCHEME, "//(?:", USERPASS, "\\@)?", HOST, PORT, URLPATH),
    concatcp!("(?:www|ftp)", HOSTCHARS_CLASS, "*\\.", HOST, PORT, URLPATH),
    concatcp!(
        "(?:callto:|h323:|sip:)",
        USERCHARS_CLASS,
        "[",
        USERCHARS,
        ".]*(?:",
        PORT,
        "/[a-z0-9]+)?\\@",
        HOST
    ),
    concatcp!(
        "(?:mailto:)?",
        USERCHARS_CLASS,
        "[",
        USERCHARS,
        ".]*\\@",
        HOSTCHARS_CLASS,
        "+\\.",
        HOST
    ),
    concatcp!("(?:news:|man:|info:)[[:alnum:]\\Q^_{|}~!\"#$%&'()*+,./;:=?`\\E]+"),
];

pub const PCRE2_MULTILINE: u32 = 0x00000400;

#[macro_export]
macro_rules! unwrap_or_return {
    ( $e:expr ) => {
        match $e {
            Some(x) => x,
            None => return,
        }
    };
}

#[inline]
pub fn borrow_clone<T>(cell: &RefCell<Option<T>>) -> T
where
    T: Clone,
{
    cell.borrow().clone().unwrap()
}

/// One font zoom step (Ctrl+plus/minus/0). A positive delta zooms in, a
/// negative one zooms out and 0 resets to the configured font size.
pub fn adjusted_font_scale(current: f64, delta: i32) -> f64 {
    const STEP: f64 = 1.2;
    const MIN_SCALE: f64 = 0.25;
    const MAX_SCALE: f64 = 4.0;

    match delta {
        0 => 1.0,
        delta if delta > 0 => (current * STEP).min(MAX_SCALE),
        _ => (current / STEP).max(MIN_SCALE),
    }
}

#[cfg(test)]
mod tests {
    use super::adjusted_font_scale;

    #[test]
    fn font_scale_steps_and_clamps() {
        assert_eq!(adjusted_font_scale(1.0, 1), 1.2);
        assert_eq!(adjusted_font_scale(1.2, -1), 1.0);
        assert_eq!(adjusted_font_scale(2.5, 0), 1.0);
        // Clamped at both ends
        assert_eq!(adjusted_font_scale(4.0, 1), 4.0);
        assert_eq!(adjusted_font_scale(0.25, -1), 0.25);
    }
}
