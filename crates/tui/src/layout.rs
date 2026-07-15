//! Viewport math and display-column layout - the frontend's coordinate work
//! (SPEC §4, §5).
//!
//! The core deals in byte/grapheme/line-column spaces; **display columns**
//! (terminal cells, with tab expansion and wide-character width) are the
//! frontend's job (SPEC §4). The core never assumes 1 char = 1 cell. These are
//! pure functions so they are unit-testable without a terminal (SPEC §13).
//!
//! The frontend owns the viewport: which lines are visible and the scroll offset
//! are computed here from the primary cursor and the terminal size, with **zero
//! round-trips to the core** (the anti-Xi rule, SPEC §5).

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vortex_core::Text;

/// The primary cursor's on-screen position, derived from the buffer text and the
/// primary caret's byte offset. Returns `(line, byte_col_within_line, line_text)`.
///
/// Defensive (SPEC §8): an out-of-range `head` is clamped to the buffer end and an
/// empty buffer yields line 0 / column 0 / empty text, so a bad offset renders in
/// the corner rather than panicking. Kept here (not in the I/O shell) so it is
/// unit-testable without a terminal (SPEC §13).
pub fn cursor_line_col(text: &Text, head: usize) -> (usize, usize, String) {
    let head = head.min(text.byte_len());
    let line = text.line_of_byte(head);
    let line_start = text.byte_of_line(line).unwrap_or(0);
    let line_text = text.line(line).unwrap_or_default();
    (line, head - line_start, line_text)
}

/// Cells one grapheme occupies starting from display column `col`: a tab advances
/// to the next `tab_width` stop; any other grapheme takes its `unicode-width`.
/// The single source of truth for tab-stop semantics, shared by [`display_column`]
/// and [`expand_tabs`] so the cursor column and painted glyphs can never drift.
fn cells_for(grapheme: &str, col: usize, tab_width: usize) -> usize {
    if grapheme == "\t" {
        tab_width - (col % tab_width) // to the next multiple, at least one cell
    } else {
        grapheme.width()
    }
}

/// Display width (terminal cells) of the prefix of `line` up to `byte_col`.
///
/// Tabs expand to the next `tab_width` stop; wide characters (CJK, emoji) count
/// as their `unicode-width`. `byte_col` must be a grapheme boundary within the
/// line; it is clamped to the line length defensively. This maps the core's
/// byte/grapheme column to the cell the cursor should paint in (SPEC §4).
pub fn display_column(line: &str, byte_col: usize, tab_width: usize) -> usize {
    let end = byte_col.min(line.len());
    line[..end]
        .graphemes(true)
        .fold(0, |col, g| col + cells_for(g, col, tab_width))
}

/// Expand tabs in `line` to spaces at `tab_width` stops, so the painted glyphs
/// occupy the same cells [`display_column`] computes for the cursor. Without this
/// the terminal advances tabs to *its own* stops while the cursor uses ours, and
/// the two drift apart (a real "cursor off after a tab" bug). Non-tab graphemes
/// are copied verbatim; wide chars keep their width because only tabs are
/// rewritten.
pub fn expand_tabs(line: &str, tab_width: usize) -> String {
    if !line.contains('\t') {
        return line.to_string(); // fast path: most lines have no tabs
    }
    let mut out = String::with_capacity(line.len());
    let mut col = 0;
    for g in line.graphemes(true) {
        if g == "\t" {
            // Fill with the same cell count display_column charges for this tab.
            let fill = cells_for(g, col, tab_width);
            out.extend(std::iter::repeat_n(' ', fill));
            col += fill;
        } else {
            out.push_str(g);
            col += cells_for(g, col, tab_width);
        }
    }
    out
}

/// The visible lines to paint: the `height` rows starting at `scroll`, each with
/// tabs expanded to `tab_width` stops so glyphs align with the cursor column.
/// Stops at the end of the buffer (no blank padding - the terminal backend clears
/// unused rows). Pure and line-bounded (SPEC §10.4), so the viewport slice is
/// unit-testable rather than buried in the draw closure (SPEC §13).
pub fn visible_lines(text: &Text, scroll: usize, height: usize, tab_width: usize) -> Vec<String> {
    (scroll..text.line_count())
        .take(height)
        .map(|i| {
            let raw = text.line(i).unwrap_or_default();
            // Only tab-bearing lines need a rewrite; move the rest as-is to avoid
            // a second full-line copy per row.
            if raw.contains('\t') {
                expand_tabs(&raw, tab_width)
            } else {
                raw
            }
        })
        .collect()
}

/// Scroll the vertical viewport so `cursor_line` stays visible within a window of
/// `height` rows, given the current top line `scroll`. Returns the new top line.
///
/// Keeps the cursor inside the window with minimal movement: scroll up if the
/// cursor is above the top, down if it is below the bottom, else stay. This is
/// the whole "scroll = read a different range from the same snapshot, no core
/// message" mechanism (SPEC §5) - the frontend adjusts `scroll` locally.
pub fn scroll_to_show(cursor_line: usize, scroll: usize, height: usize) -> usize {
    if height == 0 {
        return scroll;
    }
    if cursor_line < scroll {
        cursor_line
    } else if cursor_line >= scroll + height {
        // Put the cursor on the last visible row.
        cursor_line + 1 - height
    } else {
        scroll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_display_column_is_byte_column() {
        assert_eq!(display_column("hello", 0, 4), 0);
        assert_eq!(display_column("hello", 3, 4), 3);
        assert_eq!(display_column("hello", 5, 4), 5);
    }

    #[test]
    fn tab_expands_to_next_stop() {
        // At col 0 a tab jumps to 4 (tab_width). "a\t" -> 'a' at 0, tab fills to 4.
        assert_eq!(display_column("\t", 1, 4), 4);
        assert_eq!(display_column("a\t", 2, 4), 4);
        assert_eq!(display_column("ab\t", 3, 4), 4);
        assert_eq!(display_column("abcd\t", 5, 4), 8); // already at a stop -> +4
    }

    #[test]
    fn wide_chars_take_two_cells() {
        // Each CJK char is 2 cells wide. "日本" prefix of 3 bytes = one char = 2.
        assert_eq!(display_column("日本", 3, 4), 2);
        assert_eq!(display_column("日本", 6, 4), 4);
    }

    #[test]
    fn zwj_emoji_is_one_grapheme() {
        // A ZWJ family renders as a single (wide) grapheme; the whole cluster is
        // measured as one unit, not per code point.
        let family = "👨‍👩‍👧";
        // width of the cluster is implementation-defined but stable and > 0;
        // the point is it is measured once, and the byte length maps to it.
        let w = display_column(family, family.len(), 4);
        assert!(
            w >= 2,
            "emoji cluster should occupy at least 2 cells, got {w}"
        );
    }

    #[test]
    fn byte_col_clamped_to_line_length() {
        assert_eq!(display_column("hi", 99, 4), 2);
    }

    #[test]
    fn scroll_stays_when_cursor_visible() {
        assert_eq!(scroll_to_show(5, 3, 10), 3); // 5 within [3, 13)
    }

    #[test]
    fn scroll_up_when_cursor_above_top() {
        assert_eq!(scroll_to_show(2, 5, 10), 2);
    }

    #[test]
    fn scroll_down_when_cursor_below_bottom() {
        // window height 10, top 0 shows lines 0..9; cursor at 12 -> top 3 (3..12).
        assert_eq!(scroll_to_show(12, 0, 10), 3);
    }

    #[test]
    fn scroll_cursor_on_last_visible_row_is_stable() {
        // Cursor exactly on the bottom row stays put.
        assert_eq!(scroll_to_show(9, 0, 10), 0);
    }

    #[test]
    fn scroll_zero_height_is_noop() {
        assert_eq!(scroll_to_show(5, 2, 0), 2);
    }

    #[test]
    fn expand_tabs_no_tabs_is_identity() {
        assert_eq!(expand_tabs("hello", 4), "hello");
        assert_eq!(expand_tabs("日本", 4), "日本");
    }

    #[test]
    fn expand_tabs_fills_to_stop() {
        // Leading tab -> 4 spaces; tab after "ab" -> 2 spaces (to col 4).
        assert_eq!(expand_tabs("\t", 4), "    ");
        assert_eq!(expand_tabs("ab\t", 4), "ab  ");
        assert_eq!(expand_tabs("abcd\tx", 4), "abcd    x");
    }

    #[test]
    fn expand_tabs_matches_display_column() {
        // The whole point: expanded text length in cells == display_column of the
        // original at its end, so cursor and glyphs never drift.
        let line = "a\tbc\td";
        let expanded = expand_tabs(line, 4);
        assert_eq!(
            expanded.chars().count(),
            display_column(line, line.len(), 4)
        );
    }

    #[test]
    fn expand_tabs_with_wide_char_before_tab() {
        // "日" is 2 cells, so the following tab fills 2 to reach col 4.
        assert_eq!(expand_tabs("日\t", 4), "日  ");
    }

    fn text_of(s: &str) -> Text {
        use vortex_core::{Buffer, RopeBuffer};
        RopeBuffer::from(s).text()
    }

    #[test]
    fn cursor_line_col_on_first_line() {
        let (line, col, text) = cursor_line_col(&text_of("hello\nworld"), 3);
        assert_eq!((line, col), (0, 3));
        assert_eq!(text, "hello");
    }

    #[test]
    fn cursor_line_col_on_second_line() {
        // Offset 8 is 'r' in "world" (line 1 starts at byte 6).
        let (line, col, text) = cursor_line_col(&text_of("hello\nworld"), 8);
        assert_eq!((line, col), (1, 2));
        assert_eq!(text, "world");
    }

    #[test]
    fn cursor_line_col_clamps_out_of_range_head() {
        let (line, col, _) = cursor_line_col(&text_of("ab"), 999);
        assert_eq!((line, col), (0, 2));
    }

    #[test]
    fn cursor_line_col_empty_buffer() {
        let (line, col, text) = cursor_line_col(&text_of(""), 0);
        assert_eq!((line, col), (0, 0));
        assert_eq!(text, "");
    }

    #[test]
    fn visible_lines_window_from_scroll() {
        let t = text_of("l0\nl1\nl2\nl3\nl4");
        // Window of 2 rows starting at line 1.
        assert_eq!(visible_lines(&t, 1, 2, 4), vec!["l1", "l2"]);
    }

    #[test]
    fn visible_lines_stops_at_buffer_end() {
        let t = text_of("a\nb");
        // Height exceeds the buffer: only the two real lines, no blank padding.
        assert_eq!(visible_lines(&t, 0, 10, 4), vec!["a", "b"]);
    }

    #[test]
    fn visible_lines_expands_tabs() {
        let t = text_of("a\tb\nplain");
        assert_eq!(visible_lines(&t, 0, 2, 4), vec!["a   b", "plain"]);
    }

    #[test]
    fn visible_lines_scroll_past_end_is_empty() {
        let t = text_of("only");
        assert!(visible_lines(&t, 5, 3, 4).is_empty());
    }
}
