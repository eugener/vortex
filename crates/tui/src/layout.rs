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

use std::path::Path;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vortex_core::Text;

/// Shown in the head bar when the buffer has no bound file (SPEC §10 lifecycle).
pub const NO_NAME: &str = "[No Name]";

/// Minimum digit field for the line-number gutter, so even a short file gets a
/// tidy left margin instead of a cramped single column.
const MIN_GUTTER_DIGITS: usize = 3;

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

/// Number of display lines - every line the cursor can reach, each of which gets
/// a row and a gutter number. This includes the empty line after a trailing
/// newline (press Enter at end of file) and the sole line of an empty buffer.
///
/// crop's [`Text::line_count`] omits both: it counts `""` as 0 lines and does not
/// count the empty line following a final `"\n"` (a trailing newline is a
/// *terminator*, not a new line). So `"a\nb\n"` is 2 to the rope but 3 lines on
/// screen. We derive the count from the last reachable byte's line index instead,
/// which is exactly "how many lines a cursor can be on" (SPEC §4).
pub fn display_line_count(text: &Text) -> usize {
    text.line_of_byte(text.byte_len()) + 1
}

/// The visible lines to paint: the `height` rows starting at `scroll`, each with
/// tabs expanded to `tab_width` stops so glyphs align with the cursor column.
/// Bounded by [`display_line_count`] so a trailing empty line (and the empty
/// buffer's sole line) still gets a row; a line past the rope's content resolves
/// to `""`. Stops at the last display line (no blank padding - the terminal
/// backend clears unused rows). Pure and line-bounded (SPEC §10.4), so the
/// viewport slice is unit-testable rather than buried in the draw closure (§13).
pub fn visible_lines(text: &Text, scroll: usize, height: usize, tab_width: usize) -> Vec<String> {
    (scroll..display_line_count(text))
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

/// Columns the line-number gutter occupies: a right-aligned digit field (at least
/// [`MIN_GUTTER_DIGITS`] wide, widening for larger files) plus one space
/// separating the numbers from the text. Sized from the largest line number so
/// the gutter width never jitters as the cursor moves within a file. `line_count`
/// is the display count (see [`display_line_count`]), always >= 1.
pub fn gutter_width(line_count: usize) -> usize {
    digit_count(line_count.max(1)).max(MIN_GUTTER_DIGITS) + 1
}

/// Base-10 digit count of `n` (n >= 1), without floating-point `log10`.
fn digit_count(n: usize) -> usize {
    let mut n = n;
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// The gutter text for the buffer line at 0-based `line_index`: its 1-based
/// number, right-aligned in `gutter_width` columns with the trailing separator
/// space (absolute numbering). `gutter_width` includes that space, so the digit
/// field is one narrower.
pub fn gutter_label(line_index: usize, gutter_width: usize) -> String {
    let field = gutter_width.saturating_sub(1);
    format!("{:>field$} ", line_index + 1)
}

/// 1-based grapheme column of `byte_col` within `line`, for the status readout.
/// Columns count grapheme clusters (user-perceived characters), not bytes, so a
/// multi-byte character advances the count by one, not by its byte length
/// (SPEC §4). `byte_col` is clamped to the line length defensively.
pub fn grapheme_column(line: &str, byte_col: usize) -> usize {
    let end = byte_col.min(line.len());
    line[..end].graphemes(true).count() + 1
}

/// The buffer's display name for the head bar: the file name of `path` (not the
/// full path, to keep the bar short), or [`NO_NAME`] when unnamed. A modified
/// buffer is prefixed with `● ` so unsaved work is visible at a glance (SPEC §8,
/// §10). A path ending in `..`/`/` (no file name component) falls back to its
/// lossy full form rather than the placeholder.
pub fn buffer_display_name(path: Option<&Path>, modified: bool) -> String {
    let name = match path {
        None => NO_NAME.to_string(),
        Some(p) => p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.to_string_lossy().into_owned()),
    };
    if modified {
        format!("● {name}")
    } else {
        name
    }
}

/// Head-bar segments `(left, right)` = (buffer display name, line count). Composed
/// to full width by [`fit_bar`] at paint time. `name` is [`buffer_display_name`]
/// (already carries any modified marker); `line_count` is the display count (see
/// [`display_line_count`], always >= 1); the `.max(1)` is a defensive floor so a
/// stray 0 still reads "1 line" rather than an empty count.
pub fn head_bar(name: &str, line_count: usize) -> (String, String) {
    let left = format!(" {name}");
    let right = match line_count.max(1) {
        1 => "1 line ".to_string(),
        n => format!("{n} lines "),
    };
    (left, right)
}

/// A short, human-readable status line for a file-lifecycle notification, or
/// `None` for notifications the status bar does not surface (e.g. `ShuttingDown`).
/// The frontend shows this transiently in place of the cursor position (SPEC §8:
/// a save result - especially a failure - must be visible, not silent).
pub fn notification_message(note: &vortex_core::Notification) -> Option<String> {
    use vortex_core::Notification::*;
    match note {
        FileOpened { path, existed, .. } => {
            let name = buffer_display_name(Some(path), false);
            Some(if *existed {
                format!("Opened {name}")
            } else {
                format!("{name} [New File]")
            })
        }
        FileSaved { path, .. } => Some(format!("Saved {}", buffer_display_name(Some(path), false))),
        FileError { message, .. } => Some(format!("Error: {message}")),
        EditRejected { message, .. } => Some(format!("Edit rejected: {message}")),
        // Non-exhaustive: unknown/silent notifications do not occupy the bar.
        _ => None,
    }
}

/// Status-bar segments `(left, right)` = (cursor position, buffer metrics). When a
/// transient `message` is present (a file open/save result) it replaces the cursor
/// position on the left so the result is visible (SPEC §8). `line`/`col` are
/// 1-based for display; `bytes` is the buffer size and `version` the document
/// version (SPEC §5), surfaced while the delta/version model is young.
pub fn status_bar(
    line: usize,
    col: usize,
    bytes: usize,
    version: u64,
    message: Option<&str>,
) -> (String, String) {
    let left = match message {
        Some(m) => format!(" {m}"),
        None => format!(" Ln {line}, Col {col}"),
    };
    let right = format!("{bytes}B · v{version} ");
    (left, right)
}

/// Compose a bar of exactly `width` display cells: `left` flush to the start,
/// `right` flush to the end, spaces between. Returning the full-width string means
/// the caller's background fill covers every cell with no gaps.
///
/// When the two cannot both fit with a one-space gap, the right segment is dropped
/// and the left is truncated to `width` - the left half (name / cursor position)
/// is the more important one to keep. Truncation is grapheme-aware so a multi-byte
/// cluster is never split (SPEC §4).
pub fn fit_bar(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let (left_text, left_cells) = truncate_to_cells(left, width);
    let right_cells = right.width();
    // Room for both plus at least one separating space?
    if left_cells + 1 + right_cells <= width {
        let gap = width - left_cells - right_cells;
        let mut out = String::with_capacity(left_text.len() + gap + right.len());
        out.push_str(&left_text);
        out.extend(std::iter::repeat_n(' ', gap));
        out.push_str(right);
        out
    } else {
        // Right cannot fit: pad the (possibly truncated) left to full width.
        let mut out = left_text;
        out.extend(std::iter::repeat_n(' ', width - left_cells));
        out
    }
}

/// Longest grapheme-boundary prefix of `s` fitting in `max_cells` display columns,
/// with its actual cell width. Never splits a cluster (SPEC §4).
fn truncate_to_cells(s: &str, max_cells: usize) -> (String, usize) {
    let mut out = String::new();
    let mut cells = 0;
    for g in s.graphemes(true) {
        let w = g.width();
        if cells + w > max_cells {
            break;
        }
        out.push_str(g);
        cells += w;
    }
    (out, cells)
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

    #[test]
    fn display_line_count_includes_reachable_empty_lines() {
        // The empty buffer still has one line the cursor sits on.
        assert_eq!(display_line_count(&text_of("")), 1);
        assert_eq!(display_line_count(&text_of("hello")), 1);
        // A trailing newline opens a new (empty) line the cursor can reach - crop's
        // line_count() reports 1 here, but the screen shows 2.
        assert_eq!(display_line_count(&text_of("hello\n")), 2);
        assert_eq!(display_line_count(&text_of("a\nb")), 2);
        assert_eq!(display_line_count(&text_of("a\nb\n")), 3);
    }

    #[test]
    fn visible_lines_renders_empty_buffer_as_one_blank_row() {
        // Regression: an empty buffer must still paint one (blank) row so its
        // gutter number "1" shows, rather than zero rows.
        assert_eq!(visible_lines(&text_of(""), 0, 10, 4), vec![""]);
    }

    #[test]
    fn visible_lines_renders_trailing_empty_line() {
        // Regression: pressing Enter at end of file ("a\n") must show line 2 as a
        // blank row so it gets a gutter number, not be swallowed as a terminator.
        assert_eq!(visible_lines(&text_of("a\n"), 0, 10, 4), vec!["a", ""]);
    }

    #[test]
    fn gutter_width_has_minimum_then_widens_with_digits() {
        // 3-digit minimum field + 1 separator space, until the file needs more.
        assert_eq!(gutter_width(1), 4);
        assert_eq!(gutter_width(999), 4);
        assert_eq!(gutter_width(1000), 5); // 4 digits + space
        assert_eq!(gutter_width(0), 4); // defensive floor: still sizes for line "1"
    }

    #[test]
    fn digit_count_counts_base_ten_digits() {
        assert_eq!(digit_count(1), 1);
        assert_eq!(digit_count(9), 1);
        assert_eq!(digit_count(10), 2);
        assert_eq!(digit_count(99), 2);
        assert_eq!(digit_count(100), 3);
    }

    #[test]
    fn gutter_label_is_one_based_and_right_aligned() {
        // width 4 = 3-digit field + trailing space.
        assert_eq!(gutter_label(0, 4), "  1 ");
        assert_eq!(gutter_label(41, 4), " 42 ");
        assert_eq!(gutter_label(998, 4), "999 ");
    }

    #[test]
    fn grapheme_column_is_one_based_and_counts_clusters() {
        assert_eq!(grapheme_column("hello", 0), 1); // start of line
        assert_eq!(grapheme_column("hello", 3), 4);
        // "日本": each char is 3 bytes; byte_col 3 is after one cluster -> col 2.
        assert_eq!(grapheme_column("日本", 3), 2);
        assert_eq!(grapheme_column("日本", 6), 3);
    }

    #[test]
    fn grapheme_column_counts_zwj_cluster_once() {
        let family = "👨‍👩‍👧";
        // The whole cluster is one column: past it is column 2, not column 8.
        assert_eq!(grapheme_column(family, family.len()), 2);
    }

    #[test]
    fn grapheme_column_clamps_out_of_range_byte_col() {
        assert_eq!(grapheme_column("hi", 99), 3);
    }

    #[test]
    fn head_bar_pluralizes_line_count() {
        assert_eq!(
            head_bar("[No Name]", 1),
            (" [No Name]".into(), "1 line ".into())
        );
        assert_eq!(head_bar("f.rs", 4), (" f.rs".into(), "4 lines ".into()));
        // Empty buffer reads as one line, matching the single rendered row.
        assert_eq!(head_bar("x", 0).1, "1 line ");
    }

    #[test]
    fn status_bar_composes_position_and_metrics() {
        let (left, right) = status_bar(2, 5, 38, 7, None);
        assert_eq!(left, " Ln 2, Col 5");
        assert_eq!(right, "38B · v7 ");
    }

    #[test]
    fn status_bar_message_replaces_cursor_position() {
        // A transient file message takes the left slot so the result is visible;
        // metrics stay on the right (SPEC §8).
        let (left, right) = status_bar(2, 5, 38, 7, Some("Saved f.rs"));
        assert_eq!(left, " Saved f.rs");
        assert_eq!(right, "38B · v7 ");
    }

    #[test]
    fn buffer_display_name_uses_file_name_not_full_path() {
        assert_eq!(
            buffer_display_name(Some(Path::new("/home/user/src/main.rs")), false),
            "main.rs"
        );
    }

    #[test]
    fn buffer_display_name_unnamed_buffer_is_placeholder() {
        assert_eq!(buffer_display_name(None, false), NO_NAME);
    }

    #[test]
    fn buffer_display_name_marks_modified_with_dot() {
        assert_eq!(
            buffer_display_name(Some(Path::new("a.txt")), true),
            "● a.txt"
        );
        assert_eq!(buffer_display_name(None, true), "● [No Name]");
    }

    #[test]
    fn buffer_display_name_falls_back_when_no_file_name_component() {
        // A path ending in "/" or ".." has no file_name; use the lossy full form
        // rather than the unnamed placeholder.
        assert_eq!(buffer_display_name(Some(Path::new("..")), false), "..");
    }

    #[test]
    fn notification_message_renders_file_events() {
        use std::path::PathBuf;
        use vortex_core::{BufferId, Notification};
        let id = BufferId(0);
        assert_eq!(
            notification_message(&Notification::FileOpened {
                buffer_id: id,
                path: PathBuf::from("dir/a.rs"),
                existed: true,
            })
            .as_deref(),
            Some("Opened a.rs")
        );
        assert_eq!(
            notification_message(&Notification::FileOpened {
                buffer_id: id,
                path: PathBuf::from("new.rs"),
                existed: false,
            })
            .as_deref(),
            Some("new.rs [New File]")
        );
        assert_eq!(
            notification_message(&Notification::FileSaved {
                buffer_id: id,
                path: PathBuf::from("dir/a.rs"),
            })
            .as_deref(),
            Some("Saved a.rs")
        );
        assert_eq!(
            notification_message(&Notification::FileError {
                buffer_id: id,
                path: None,
                message: "disk full".into(),
            })
            .as_deref(),
            Some("Error: disk full")
        );
    }

    #[test]
    fn notification_message_none_for_shutting_down() {
        use vortex_core::Notification;
        assert_eq!(notification_message(&Notification::ShuttingDown), None);
    }

    #[test]
    fn fit_bar_pushes_segments_to_each_edge() {
        // width 20: "ab" (2) + gap + "cd" (2) -> gap of 16.
        let bar = fit_bar("ab", "cd", 20);
        assert_eq!(bar, "ab".to_string() + &" ".repeat(16) + "cd");
        assert_eq!(bar.width(), 20);
    }

    #[test]
    fn fit_bar_exact_fit_keeps_single_space() {
        // "ab" + 1 space + "cd" = 5 cells exactly.
        assert_eq!(fit_bar("ab", "cd", 5), "ab cd");
    }

    #[test]
    fn fit_bar_drops_right_and_pads_left_when_tight() {
        // width 4 can't hold "ab" + space + "cd" (needs 5): keep left, pad.
        assert_eq!(fit_bar("ab", "cd", 4), "ab  ");
    }

    #[test]
    fn fit_bar_truncates_left_when_wider_than_bar() {
        // Left alone exceeds width: truncate it, drop right, fill exactly.
        assert_eq!(fit_bar("abcdef", "xy", 4), "abcd");
        assert_eq!(fit_bar("abcdef", "xy", 4).width(), 4);
    }

    #[test]
    fn fit_bar_zero_width_is_empty() {
        assert_eq!(fit_bar("ab", "cd", 0), "");
    }

    #[test]
    fn fit_bar_never_splits_a_wide_cluster() {
        // "日" is 2 cells. width 3 fits one (2 cells) then can't fit the second;
        // the padded result is 3 cells with the cluster intact.
        let bar = fit_bar("日本", "", 3);
        assert_eq!(bar, "日 ");
        assert_eq!(bar.width(), 3);
    }
}
