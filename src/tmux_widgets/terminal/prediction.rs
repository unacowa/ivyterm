//! Pure logic for mosh-style predictive echo: classifying input, matching
//! predictions against the authoritative screen and laying predicted
//! graphemes out on the cell grid. The widget/drawing side lives in the
//! TmuxTerminal implementation.

use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Unconfirmed predictions are discarded after this long
pub const PREDICTION_TIMEOUT: Duration = Duration::from_secs(2);

/// A commit with more graphemes than this is a paste, not typing
const MAX_COMMIT_GRAPHEMES: usize = 64;

/// State of the predictive echo of one terminal
#[derive(Default)]
pub struct PredictionState {
    /// Predicted text which the remote echo has not confirmed yet
    pub text: String,
    /// Cell the prediction starts at: (column, absolute row)
    pub origin: (i64, i64),
    /// Predictions are only displayed once the remote confirmed at least
    /// one grapheme since the last discard; this keeps no-echo input
    /// (password prompts) invisible
    pub display_unlocked: bool,
    /// Bumped on every state change; lets pending timeouts detect that
    /// they are stale
    pub generation: u64,
}

impl PredictionState {
    pub fn discard(&mut self) {
        self.text.clear();
        self.display_unlocked = false;
        self.generation += 1;
    }
}

/// What one `commit` chunk means for the prediction
#[derive(Debug, PartialEq)]
pub enum CommitKind {
    /// Printable text: append to the prediction
    Append,
    /// Backspace: erase the last predicted grapheme
    Backspace,
    /// Anything else (control keys, escape sequences, pastes): the effect
    /// on the screen is unpredictable, discard
    Control,
}

pub fn classify_commit(text: &str) -> CommitKind {
    if text == "\x7f" || text == "\x08" {
        return CommitKind::Backspace;
    }
    if text.is_empty() || text.chars().any(|char| char.is_control()) {
        return CommitKind::Control;
    }
    if text.graphemes(true).count() > MAX_COMMIT_GRAPHEMES {
        return CommitKind::Control;
    }
    CommitKind::Append
}

/// Removes the last grapheme; returns false when there was nothing to remove
pub fn pop_grapheme(text: &mut String) -> bool {
    match text.grapheme_indices(true).last() {
        Some((offset, _)) => {
            text.truncate(offset);
            true
        }
        None => false,
    }
}

/// How many leading graphemes of `prediction` the actual screen content
/// (read starting at the prediction origin) confirms
pub fn confirmed_graphemes(screen: &str, prediction: &str) -> usize {
    // The screen text may contain newlines where the range spans rows;
    // predictions never contain them (classified Control), so drop them
    // before comparing
    let mut screen_graphemes = screen
        .graphemes(true)
        .filter(|grapheme| *grapheme != "\n" && *grapheme != "\r");

    let mut confirmed = 0;
    for grapheme in prediction.graphemes(true) {
        if screen_graphemes.next() != Some(grapheme) {
            break;
        }
        confirmed += 1;
    }
    confirmed
}

/// Byte offset of the end of the first `count` graphemes
pub fn grapheme_offset(text: &str, count: usize) -> usize {
    match text.grapheme_indices(true).nth(count) {
        Some((offset, _)) => offset,
        None => text.len(),
    }
}

/// Cell width of a grapheme cluster (at least 1, so zero-width input still
/// occupies a cell rather than corrupting the layout)
fn grapheme_cell_width(grapheme: &str) -> i64 {
    (grapheme.width() as i64).max(1)
}

/// Lays `text` out on the cell grid starting at `origin_col`, wrapping at
/// `cols`. Returns the cell of every grapheme as (grapheme, col, row delta)
/// plus the caret cell one past the end.
pub fn layout_cells<'a>(
    text: &'a str,
    origin_col: i64,
    cols: i64,
) -> (Vec<(&'a str, i64, i64)>, (i64, i64)) {
    let mut cells = Vec::new();
    let mut col = origin_col;
    let mut row = 0;

    for grapheme in text.graphemes(true) {
        let width = grapheme_cell_width(grapheme);
        if col + width > cols {
            col = 0;
            row += 1;
        }
        cells.push((grapheme, col, row));
        col += width;
    }

    if col >= cols {
        col = 0;
        row += 1;
    }
    (cells, (col, row))
}

/// Advances the prediction origin past `confirmed` (the graphemes the
/// remote echoed), following the same wrapping rule as layout_cells
pub fn advance_origin(origin: (i64, i64), confirmed: &str, cols: i64) -> (i64, i64) {
    let (_, (col, row_delta)) = layout_cells(confirmed, origin.0, cols);
    (col, origin.1 + row_delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_printable_and_japanese_as_append() {
        assert_eq!(classify_commit("a"), CommitKind::Append);
        assert_eq!(classify_commit("hello world"), CommitKind::Append);
        assert_eq!(classify_commit("こんにちは"), CommitKind::Append);
    }

    #[test]
    fn classify_backspace() {
        assert_eq!(classify_commit("\x7f"), CommitKind::Backspace);
        assert_eq!(classify_commit("\x08"), CommitKind::Backspace);
    }

    #[test]
    fn classify_control_input() {
        assert_eq!(classify_commit("\r"), CommitKind::Control);
        assert_eq!(classify_commit("\x1b[A"), CommitKind::Control);
        assert_eq!(classify_commit("\x1b\x7f"), CommitKind::Control);
        assert_eq!(classify_commit(""), CommitKind::Control);
        // A paste-sized commit is not typing
        assert_eq!(classify_commit(&"x".repeat(65)), CommitKind::Control);
    }

    #[test]
    fn pop_grapheme_handles_multibyte() {
        let mut text = String::from("aあ");
        assert!(pop_grapheme(&mut text));
        assert_eq!(text, "a");
        assert!(pop_grapheme(&mut text));
        assert_eq!(text, "");
        assert!(!pop_grapheme(&mut text));
    }

    #[test]
    fn confirmation_is_prefix_based() {
        assert_eq!(confirmed_graphemes("abc   ", "abc"), 3);
        assert_eq!(confirmed_graphemes("abx", "abc"), 2);
        assert_eq!(confirmed_graphemes("", "abc"), 0);
        assert_eq!(confirmed_graphemes("xbc", "abc"), 0);
        // Wide characters compare as graphemes, not cells
        assert_eq!(confirmed_graphemes("日本語です", "日本語"), 3);
        // Newlines from a wrapped row range are transparent
        assert_eq!(confirmed_graphemes("ab\ncd", "abcd"), 4);
    }

    #[test]
    fn grapheme_offset_maps_to_bytes() {
        assert_eq!(grapheme_offset("abc", 2), 2);
        assert_eq!(grapheme_offset("あいう", 1), 3);
        assert_eq!(grapheme_offset("abc", 10), 3);
    }

    #[test]
    fn layout_wraps_wide_characters() {
        // 4 columns: "aあi" -> a at col 0, あ (width 2) at col 1, i at col 3
        let (cells, caret) = layout_cells("aあi", 0, 4);
        assert_eq!(cells, vec![("a", 0, 0), ("あ", 1, 0), ("i", 3, 0)]);
        assert_eq!(caret, (0, 1), "caret wraps to the next row");

        // A wide character that does not fit at the right edge wraps whole
        let (cells, caret) = layout_cells("aあ", 3, 4);
        assert_eq!(cells, vec![("a", 3, 0), ("あ", 0, 1)]);
        assert_eq!(caret, (2, 1));
    }

    #[test]
    fn origin_advances_with_wrapping() {
        assert_eq!(advance_origin((2, 10), "ab", 80), (4, 10));
        assert_eq!(advance_origin((78, 10), "abc", 80), (1, 11));
        assert_eq!(advance_origin((0, 5), "日本", 80), (4, 5));
    }
}
