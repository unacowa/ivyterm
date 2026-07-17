//! Built-in terminal color schemes ("themes"), bundled at compile time.
//!
//! Each theme is a small TOML file under `themes/` with the fields:
//! `foreground`, `background`, optional `cursor` / `cursor_foreground` /
//! `selection` / `selection_foreground`, and the 8-entry `normal` / `bright`
//! palettes. The format is deliberately close to Alacritty/Ghostty color
//! schemes so community themes can be dropped in with minimal editing.

use include_dir::{include_dir, Dir};
use serde::Deserialize;

use super::IvyColor;

/// All bundled theme files, embedded at compile time. Adding a `*.toml` file
/// to `src/config/themes/` is all it takes to ship a new theme — it shows up
/// in [`theme_names`] and resolves via [`by_name`] automatically.
static THEMES_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/config/themes");

#[derive(Deserialize, Clone)]
pub struct Theme {
    pub foreground: IvyColor,
    pub background: IvyColor,
    #[serde(default)]
    pub cursor: Option<IvyColor>,
    #[serde(default)]
    pub cursor_foreground: Option<IvyColor>,
    #[serde(default)]
    pub selection: Option<IvyColor>,
    #[serde(default)]
    pub selection_foreground: Option<IvyColor>,
    pub normal: [IvyColor; 8],
    pub bright: [IvyColor; 8],
}

/// Names of all bundled themes, sorted alphabetically (as shown in the
/// settings UI). Derived from the embedded theme files.
pub fn theme_names() -> Vec<String> {
    let mut names: Vec<String> = THEMES_DIR
        .files()
        .filter_map(|file| {
            file.path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// Resolve a bundled theme by name. Returns `None` for an unknown name; the
/// caller then falls back to the individually-configured colors.
pub fn by_name(name: &str) -> Option<Theme> {
    let file = THEMES_DIR.get_file(format!("{}.toml", name))?;
    let toml = file.contents_utf8()?;

    // The bundled files are validated by a unit test, so a parse failure here
    // would be a build-time authoring bug rather than a user error.
    match toml::from_str(toml) {
        Ok(theme) => Some(theme),
        Err(err) => {
            eprintln!("Built-in theme '{}' failed to parse: {}", name, err);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_themes_parse() {
        let names = theme_names();
        assert!(names.len() > 50, "expected the bundled theme pack, got {}", names.len());
        for name in &names {
            assert!(
                by_name(name).is_some(),
                "bundled theme '{}' must parse",
                name
            );
        }
    }

    #[test]
    fn unknown_theme_is_none() {
        assert!(by_name("does-not-exist").is_none());
    }
}
