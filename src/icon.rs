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
    // The connection color becomes the terminal background of the logo; the
    // text is drawn on it in a contrasting color so it stays legible
    let bg_hex = rgba_to_hex(&rgba);
    let text_hex = contrasting_text_color(&rgba);
    let name = badge_icon_name(&bg_hex, &text);
    let svg = badge_svg(&bg_hex, text_hex, &text);

    match write_icon(&name, &svg) {
        Ok(()) => Some(name),
        Err(err) => {
            eprintln!("ivyterm: could not install badge icon: {}", err);
            None
        }
    }
}

fn default_badge_color() -> RGBA {
    // The logo's original terminal background (near-black), so a text-only
    // badge keeps the standard look
    RGBA::parse("#1a1a1a").unwrap()
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

/// The ivyTerm logo, embedded so badges are self-contained
const LOGO_SVG: &str = include_str!("../data/com.tomiyou.ivyTerm.svg");

/// Drawable content of the logo (children of its root `<svg>`), so it can be
/// nested inside the generated badge SVG
fn logo_inner() -> &'static str {
    let start = match LOGO_SVG.find("<svg") {
        Some(i) => match LOGO_SVG[i..].find('>') {
            Some(j) => i + j + 1,
            None => return "",
        },
        None => return "",
    };
    let end = LOGO_SVG.rfind("</svg>").unwrap_or(LOGO_SVG.len());
    &LOGO_SVG[start..end]
}

/// The logo fill representing the terminal background (a near-black
/// rounded rect); this is what --badge-color recolors
const TERMINAL_BG_FILL: &str = "fill:#1a1a1a";

/// The logo fill of the prompt glyphs (`>` and cursor `_`); hidden when a
/// session label takes their place
const PROMPT_FILL: &str = "fill:#ffffff";

/// Recolors only the terminal-background fill of the logo, leaving the rest
/// (the prompt, the ivy) in its original colors
fn recolor_terminal_bg(svg: &str, color_hex: &str) -> String {
    svg.replace(TERMINAL_BG_FILL, &format!("fill:{}", color_hex))
}

/// SVG for the badge: the ivyTerm logo with its terminal background recolored
/// to the connection color. With a session label, the prompt glyphs are
/// hidden and the label is drawn in the terminal screen in their place.
pub fn badge_svg(color_hex: &str, text_color_hex: &str, text: &str) -> String {
    // Logo native viewBox is 4096; 0.03125 * 4096 = 128, so it fills the icon
    let mut logo = recolor_terminal_bg(logo_inner(), color_hex);

    let text_element = if text.is_empty() {
        String::new()
    } else {
        // The label sits where the prompt was, so hide the prompt glyphs
        logo = logo.replace(PROMPT_FILL, "fill:none");
        let font_size = match text.chars().count() {
            1 => 56,
            2 => 46,
            _ => 38,
        };
        format!(
            r#"<text x="60" y="52" font-family="monospace" font-weight="bold" font-size="{size}" fill="{text_color}" text-anchor="middle" dominant-baseline="central">{text}</text>"#,
            size = font_size,
            text_color = text_color_hex,
            text = escape_xml(text),
        )
    };

    // The inlined logo carries sodipodi:/inkscape: prefixed nodes; declare
    // those namespaces on the root so the generated SVG is valid XML
    format!(
        r##"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.0.dtd" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" width="128" height="128" viewBox="0 0 128 128">
  <g transform="scale(0.03125)">{logo}</g>
  {text_element}
</svg>
"##,
        logo = logo,
        text_element = text_element,
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
        let svg = badge_svg("#cc3333", "#ffffff", "a<b");
        assert!(svg.contains("a&lt;b"));
        assert!(!svg.contains("a<b"));
        assert!(svg.contains("<svg"));
        assert!(svg.contains("fill=\"#ffffff\""));
    }

    #[test]
    fn only_terminal_background_is_recolored() {
        let svg = badge_svg("#cc3333", "#ffffff", "ML1");
        // The logo is embedded
        assert!(svg.contains("<path"));
        // The terminal background is recolored...
        assert!(svg.contains("fill:#cc3333"));
        assert!(!svg.contains("fill:#1a1a1a"));
        // ...but the ivy (green) and prompt stay original
        assert!(svg.contains("#5e9445"));
        assert!(svg.contains("#cbe3ac"));
    }

    #[test]
    fn recolor_only_touches_the_terminal_bg_fill() {
        let input = "a fill:#1a1a1a b fill:#5e9445 c";
        assert_eq!(
            recolor_terminal_bg(input, "#cc3333"),
            "a fill:#cc3333 b fill:#5e9445 c"
        );
    }
}
