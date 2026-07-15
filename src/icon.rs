//! Per-launch composed window icons.
//!
//! GTK4 exposes the window icon only by name, but on a compositor that
//! supports the `xdg-toplevel-icon-v1` protocol (KWin >= Plasma 6.3)
//! `gtk_window_set_icon_name` forwards that name and the compositor resolves
//! it against the icon theme. So a custom "badge" icon (a background color
//! identifying the connection target plus a short text identifying the
//! session) is realized by composing an SVG into the user's icon theme and
//! pointing the window at it by name. No `.desktop` file or per-launch
//! application id is needed; on compositors without the protocol the call is
//! simply ignored and the base application icon shows instead.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use gtk4::gdk::RGBA;

/// The application id, which also names the base (unbadged) icon
pub const BASE_APP_ID: &str = "com.tomiyou.ivyTerm";

/// Longest badge text that stays legible in the icon
const MAX_BADGE_CHARS: usize = 3;

/// Composes the badge icon from the optional CLI values, writes it into the
/// user's icon theme and returns the icon name to pass to
/// `Window::set_icon_name`. Returns None (use the base icon) when neither
/// argument was given or the icon could not be written.
pub fn install_badge(color: Option<&str>, text: Option<&str>) -> Option<String> {
    if color.is_none() && text.is_none() {
        return None;
    }

    let rgba = color
        .and_then(|value| match RGBA::parse(value) {
            Ok(rgba) => Some(rgba),
            Err(_) => {
                eprintln!("ivyterm: invalid --badge-color '{}', using default", value);
                None
            }
        })
        .unwrap_or_else(default_badge_color);

    let text = trim_badge_text(text.unwrap_or(""));
    let bg_hex = rgba_to_hex(&rgba);
    let fg_hex = contrasting_text_color(&rgba);
    let name = badge_icon_name(&bg_hex, &text);
    let svg = badge_svg(&bg_hex, fg_hex, &text);

    match write_icon(&name, &svg) {
        Ok(()) => Some(name),
        Err(err) => {
            eprintln!("ivyterm: could not install badge icon: {}", err);
            None
        }
    }
}

fn default_badge_color() -> RGBA {
    RGBA::parse("#3465a4").unwrap()
}

/// Keeps at most MAX_BADGE_CHARS characters (counted as Unicode scalar
/// values, which is adequate for the short labels this targets)
fn trim_badge_text(text: &str) -> String {
    text.chars().take(MAX_BADGE_CHARS).collect()
}

fn rgba_to_hex(rgba: &RGBA) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        channel(rgba.red()),
        channel(rgba.green()),
        channel(rgba.blue())
    )
}

/// Black on light backgrounds, white on dark ones (WCAG relative luminance)
fn contrasting_text_color(rgba: &RGBA) -> &'static str {
    let luminance =
        0.2126 * rgba.red() as f64 + 0.7152 * rgba.green() as f64 + 0.0722 * rgba.blue() as f64;
    if luminance > 0.5 {
        "#000000"
    } else {
        "#ffffff"
    }
}

/// Deterministic icon name for a badge, so the SVG is reused across launches
/// of the same color+text
fn badge_icon_name(bg_hex: &str, text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    bg_hex.hash(&mut hasher);
    "\0".hash(&mut hasher);
    text.hash(&mut hasher);
    format!("{}.badge_{:08x}", BASE_APP_ID, hasher.finish() as u32)
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// SVG for the badge: a rounded square in the badge color with the text
/// centered on top
pub fn badge_svg(bg_hex: &str, fg_hex: &str, text: &str) -> String {
    let font_size = match text.chars().count() {
        0 | 1 => 64,
        2 => 52,
        _ => 40,
    };
    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <rect x="8" y="8" width="112" height="112" rx="24" ry="24" fill="{bg}"/>
  <text x="64" y="64" font-family="monospace" font-weight="bold" font-size="{size}" fill="{fg}" text-anchor="middle" dominant-baseline="central">{text}</text>
</svg>
"##,
        bg = bg_hex,
        fg = fg_hex,
        size = font_size,
        text = escape_xml(text),
    )
}

fn xdg_data_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
}

/// Writes the SVG into the scalable hicolor apps dir (idempotent) so the
/// compositor can resolve the icon name
fn write_icon(name: &str, svg: &str) -> std::io::Result<()> {
    let data_home = xdg_data_home()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no XDG data dir"))?;
    let icon_dir = data_home.join("icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&icon_dir)?;
    std::fs::write(icon_dir.join(format!("{}.svg", name)), svg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_trimmed_to_three_chars() {
        assert_eq!(trim_badge_text("ML1"), "ML1");
        assert_eq!(trim_badge_text("LONGER"), "LON");
        assert_eq!(trim_badge_text(""), "");
    }

    #[test]
    fn icon_name_is_deterministic_and_distinct() {
        let a = badge_icon_name("#c33", "ML1");
        assert_eq!(a, badge_icon_name("#c33", "ML1"));
        assert_ne!(badge_icon_name("#c33", "ML1"), badge_icon_name("#c33", "ML2"));
        assert_ne!(badge_icon_name("#c33", "ML1"), badge_icon_name("#d44", "ML1"));
        assert!(a.starts_with("com.tomiyou.ivyTerm.badge_"));
    }

    #[test]
    fn hex_conversion_rounds_channels() {
        assert_eq!(rgba_to_hex(&RGBA::parse("#ff0000").unwrap()), "#ff0000");
        assert_eq!(rgba_to_hex(&RGBA::parse("#000000").unwrap()), "#000000");
        assert_eq!(rgba_to_hex(&RGBA::parse("white").unwrap()), "#ffffff");
    }

    #[test]
    fn text_color_contrasts_with_background() {
        assert_eq!(contrasting_text_color(&RGBA::parse("#ffffff").unwrap()), "#000000");
        assert_eq!(contrasting_text_color(&RGBA::parse("#000000").unwrap()), "#ffffff");
        assert_eq!(contrasting_text_color(&RGBA::parse("#101010").unwrap()), "#ffffff");
    }

    #[test]
    fn svg_escapes_text_and_is_well_formed() {
        let svg = badge_svg("#c33", "#fff", "a<b");
        assert!(svg.contains("a&lt;b"));
        assert!(!svg.contains("a<b"));
        assert!(svg.contains("<svg"));
        assert!(svg.contains("fill=\"#c33\""));
    }
}
