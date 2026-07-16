//! Per-launch composed window icons.
//!
//! A custom "badge" icon (the ivyTerm logo with its terminal background
//! recolored to identify the connection target, and a short session label)
//! is composed as an SVG, rasterized with resvg and handed to the window as
//! a texture via `gdk_toplevel_set_icon_list`. On a compositor supporting
//! the `xdg-toplevel-icon` protocol (KWin >= Plasma 6.3) the pixels are sent
//! per window directly. This deliberately avoids the icon-theme/name route:
//! a running compositor caches icon-name lookups, so dynamically created
//! named icons are unreliable. Sending pixels also needs no on-disk file.

use std::sync::Arc;

use gtk4::gdk::{self, RGBA};
use gtk4::glib;
use gtk4::prelude::*;
use resvg::{tiny_skia, usvg};

/// The application id, which also names the base (unbadged) icon
pub const BASE_APP_ID: &str = "com.tomiyou.ivyTerm";

/// Longest badge text that stays legible in the icon
const MAX_BADGE_CHARS: usize = 3;

/// Rendered icon resolution (the SVG viewBox is 128)
const RENDER_SIZE: u32 = 256;

/// Composes the badge from the optional CLI values and rasterizes it to a
/// texture. Returns None (use the base icon) when neither argument was given
/// or rendering failed.
pub fn render_badge_texture(color: Option<&str>, text: Option<&str>) -> Option<gdk::Texture> {
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
    let svg = badge_svg(&bg_hex, text_hex, &text);

    render_svg_to_texture(&svg)
}

/// Rasterizes an SVG string to a GDK texture with resvg (pure Rust, no
/// system rasterizer dependency)
fn render_svg_to_texture(svg: &str) -> Option<gdk::Texture> {
    let mut fontdb = usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    let options = usvg::Options {
        fontdb: Arc::new(fontdb),
        ..Default::default()
    };

    let tree = match usvg::Tree::from_str(svg, &options) {
        Ok(tree) => tree,
        Err(err) => {
            eprintln!("ivyterm: could not parse badge SVG: {}", err);
            return None;
        }
    };

    let mut pixmap = tiny_skia::Pixmap::new(RENDER_SIZE, RENDER_SIZE)?;
    let scale = RENDER_SIZE as f32 / 128.0;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny_skia is RGBA8 premultiplied, matching R8g8b8a8Premultiplied
    let bytes = glib::Bytes::from(pixmap.data());
    let texture = gdk::MemoryTexture::new(
        RENDER_SIZE as i32,
        RENDER_SIZE as i32,
        gdk::MemoryFormat::R8g8b8a8Premultiplied,
        &bytes,
        (RENDER_SIZE * 4) as usize,
    );
    Some(texture.upcast())
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
