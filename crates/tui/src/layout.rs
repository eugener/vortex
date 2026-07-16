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

use std::ops::Range;
use std::path::Path;

use ratatui::style::Style;
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use vortex_core::{Selection, Text};

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

/// Byte column within `line` nearest display column `target` - the inverse of
/// [`display_column`], for mapping a pointer's cell back to a caret position. Walks
/// graphemes accumulating cells; when `target` falls on a grapheme, the nearer edge
/// wins (past the midpoint rounds to the following boundary) so a click on the right
/// half of a wide glyph or a tab lands where the pointer visually is. Clamped to the
/// line's content length (a click past the last glyph goes to end-of-line).
pub fn byte_col_at_display(line: &str, target: usize, tab_width: usize) -> usize {
    let mut col = 0;
    for (byte_idx, g) in line.grapheme_indices(true) {
        let w = cells_for(g, col, tab_width);
        if target < col + w {
            return if target - col >= w.div_ceil(2) {
                byte_idx + g.len()
            } else {
                byte_idx
            };
        }
        col += w;
    }
    line.len()
}

/// Buffer byte offset for a pointer at body-relative `(row, col)` cells - the
/// inverse of the paint math, kept here so the pointer->position mapping is
/// unit-testable without a terminal (SPEC §13). `row` is 0-based within the text
/// body (the caller has already subtracted the head bar) and is clamped to the last
/// line; `col` is an absolute body column, so a click in the gutter
/// (`col < gutter_width`) lands at the line start. Both scroll offsets are the
/// frontend's current viewport, so the lookup needs no core round-trip (SPEC §5).
pub fn offset_at_cell(
    text: &Text,
    scroll: usize,
    h_scroll: usize,
    gutter_width: usize,
    tab_width: usize,
    row: usize,
    col: usize,
) -> usize {
    let line = (scroll + row).min(display_line_count(text).saturating_sub(1));
    let line_start = text.byte_of_line(line).unwrap_or(0);
    let byte_col = if col < gutter_width {
        0 // a click in the gutter selects the start of the line
    } else {
        let raw = text.line(line).unwrap_or_default();
        byte_col_at_display(&raw, col - gutter_width + h_scroll, tab_width)
    };
    line_start + byte_col
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

/// Keep index `cursor` visible within a window of `size` starting at `offset`,
/// returning the new offset. Generic 1-D scroll shared by both axes (SPEC §5):
/// pass `(cursor_line, top, rows)` for vertical scroll or
/// `(cursor_col, left, cols)` for horizontal. Scrolls toward the cursor by the
/// minimum needed; a zero-size window never scrolls.
pub fn scroll_to_show(cursor: usize, offset: usize, size: usize) -> usize {
    if size == 0 {
        return offset;
    }
    if cursor < offset {
        cursor
    } else if cursor >= offset + size {
        // Put the cursor on the last visible cell of the window.
        cursor + 1 - size
    } else {
        offset
    }
}

/// Render the tab-expanded `line` into styled spans for the display-column window
/// `[h_scroll, h_scroll + width)` - the frontend's one intra-line styling seam,
/// shared by selection highlighting now and syntax highlighting (M4) later.
///
/// Every cell in the window is emitted: content graphemes, then padding spaces
/// past the line's end, so `base` fills the *whole* width - the mechanism behind
/// the current-line tint. Each `overlay` (a display-column range plus a [`Style`])
/// patches `base` for the cells it covers, later overlays winning; a zero-overlay
/// call is just the clipped line. A wide grapheme (CJK/emoji) straddling either
/// edge is replaced by spaces for its visible cells so columns after it stay
/// aligned with the cursor (SPEC §4: display width != character count). Consecutive
/// equal-style cells coalesce into one span to keep the count low.
pub fn render_line(
    line: &str,
    h_scroll: usize,
    width: usize,
    base: Style,
    overlays: &[(Range<usize>, Style)],
) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let end = h_scroll + width;
    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut col = 0;
    for g in line.graphemes(true) {
        let w = g.width();
        let g_end = col + w;
        if g_end <= h_scroll {
            // Entirely left of the window.
        } else if col >= end {
            break; // entirely right of the window; nothing further can fit
        } else if col >= h_scroll && g_end <= end {
            push_run(&mut runs, g, style_at(col, base, overlays)); // fully inside
        } else {
            // Straddles an edge: one styled space per visible cell so a partially
            // clipped wide glyph does not misalign the columns after it.
            for c in col.max(h_scroll)..g_end.min(end) {
                push_run(&mut runs, " ", style_at(c, base, overlays));
            }
        }
        col = g_end;
    }
    // Pad the window past the line's content so `base` (and any overlay covering
    // these cells - e.g. a selection that consumed the trailing newline) fills the
    // remaining width. An empty range (content already past the window) adds nothing.
    for c in col.max(h_scroll)..end {
        push_run(&mut runs, " ", style_at(c, base, overlays));
    }
    runs.into_iter()
        .map(|(text, style)| Span::styled(text, style))
        .collect()
}

/// Append `s` to the last run when it shares `style`, else start a new run. Keeps
/// [`render_line`] emitting one span per style change rather than one per cell.
fn push_run(runs: &mut Vec<(String, Style)>, s: &str, style: Style) {
    match runs.last_mut() {
        Some((buf, last)) if *last == style => buf.push_str(s),
        _ => runs.push((s.to_string(), style)),
    }
}

/// The style for display column `col`: `base` with every overlay whose range
/// covers `col` patched over it in order (later overlays win).
fn style_at(col: usize, base: Style, overlays: &[(Range<usize>, Style)]) -> Style {
    let mut style = base;
    for (range, overlay) in overlays {
        if range.contains(&col) {
            style = style.patch(*overlay);
        }
    }
    style
}

/// The display-column range a selection covers on one buffer line, or `None` when
/// the selection does not touch the line or is a zero-width cursor here (a cursor
/// renders as the terminal caret, not a highlight). `line` is the line's raw text
/// (tabs intact), `line_start` its first byte offset, and `line_end_excl` the next
/// line's start (or buffer end) so the line's terminator bytes are included.
///
/// When the selection runs through this line's terminator (its end lies past the
/// content), the range gets one extra cell so the consumed line break is visible
/// and blank lines inside a multi-line selection still show a highlight.
pub fn selection_columns(
    line: &str,
    line_start: usize,
    line_end_excl: usize,
    tab_width: usize,
    sel_start: usize,
    sel_end: usize,
) -> Option<Range<usize>> {
    let content_end = line_start + line.len();
    let lo = sel_start.max(line_start);
    let hi = sel_end.min(line_end_excl);
    if lo >= hi {
        return None;
    }
    let lo_col = display_column(line, (lo - line_start).min(line.len()), tab_width);
    let hi_content = hi.min(content_end);
    let hi_col = display_column(line, (hi_content - line_start).min(line.len()), tab_width);
    // A selection reaching past the content consumed this line's newline.
    let end_col = if hi > content_end { hi_col + 1 } else { hi_col };
    (lo_col < end_col).then_some(lo_col..end_col)
}

/// Total grapheme clusters covered by `selections`, for the status readout when a
/// selection is active. Counts user-perceived characters (not bytes), line
/// terminators excluded; zero-width cursors contribute nothing. Bounded by the
/// selected span, materializing only its lines (SPEC §10.4), so an idle cursor
/// costs nothing and only a live selection pays.
pub fn selected_grapheme_count(text: &Text, selections: &[Selection]) -> usize {
    selections
        .iter()
        .map(|s| grapheme_count_in(text, s.start(), s.end()))
        .sum()
}

/// Grapheme clusters in the byte range `[start, end)`, summed line by line so no
/// slice wider than one line is ever materialized.
fn grapheme_count_in(text: &Text, start: usize, end: usize) -> usize {
    if start >= end {
        return 0;
    }
    let mut count = 0;
    for line_idx in text.line_of_byte(start)..=text.line_of_byte(end) {
        let Some(line_start) = text.byte_of_line(line_idx) else {
            break;
        };
        let content = text.line(line_idx).unwrap_or_default();
        let lo = start.max(line_start);
        let hi = end.min(line_start + content.len());
        if lo < hi {
            count += content[lo - line_start..hi - line_start]
                .graphemes(true)
                .count();
        }
    }
    count
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

/// A byte count as a compact human-readable size: plain bytes under 1 KB, then
/// `KB`/`MB`/`GB` (1024-based) with one decimal, so the status bar stays short for
/// large buffers (`12_345_678` -> `11.8MB`). No space before the unit, matching the
/// other status metrics. `GB` is the ceiling - a text buffer never realistically
/// exceeds it, and Tier-3 huge-file handling is future work (SPEC §10.4).
pub fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1}{}", UNITS[unit])
}

/// Status-bar segments `(left, right)` = (cursor position, buffer metrics). When a
/// transient `message` is present (a file open/save result) it replaces the cursor
/// position on the left so the result is visible (SPEC §8). `line`/`col` are
/// 1-based for display; a non-zero `selected` (grapheme count of the active
/// selection) is appended so the size of a selection is visible while it is held.
/// `bytes` is the buffer size (rendered via [`human_size`]) and `version` the
/// document version (SPEC §5), surfaced while the delta/version model is young.
pub fn status_bar(
    line: usize,
    col: usize,
    selected: usize,
    bytes: usize,
    version: u64,
    message: Option<&str>,
) -> (String, String) {
    let left = match message {
        Some(m) => format!(" {m}"),
        None if selected > 0 => format!(" Ln {line}, Col {col}  ({selected} selected)"),
        None => format!(" Ln {line}, Col {col}"),
    };
    let right = format!("{} · v{version} ", human_size(bytes));
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
    fn byte_col_at_display_is_inverse_of_display_column() {
        // Round-trips on the boundaries of a plain ASCII line.
        assert_eq!(byte_col_at_display("hello", 0, 4), 0);
        assert_eq!(byte_col_at_display("hello", 3, 4), 3);
        // Past the last glyph clamps to end-of-line.
        assert_eq!(byte_col_at_display("hello", 99, 4), 5);
    }

    #[test]
    fn byte_col_at_display_rounds_to_nearer_edge_of_a_wide_glyph() {
        // "日本": each glyph is 2 cells. A click on the left cell of "本" (col 2)
        // lands before it (byte 3); the right cell (col 3) lands after it (byte 6).
        assert_eq!(byte_col_at_display("日本", 2, 4), 3);
        assert_eq!(byte_col_at_display("日本", 3, 4), 6);
    }

    #[test]
    fn byte_col_at_display_handles_tabs() {
        // "a\tb": 'a' at col 0, tab spans cols 1..4, 'b' at col 4. A click at col 4
        // rounds onto 'b' (byte 2); a click at col 1 stays before the tab (byte 1).
        assert_eq!(byte_col_at_display("a\tb", 4, 4), 2);
        assert_eq!(byte_col_at_display("a\tb", 1, 4), 1);
    }

    #[test]
    fn offset_at_cell_maps_row_and_column_to_a_buffer_offset() {
        let t = text_of("ab\ncdef");
        // Gutter 4 wide, no scroll. Body row 1 is "cdef" (starts at byte 3); column
        // 4 is the gutter edge -> 'c' (offset 3), column 6 -> 'e' (offset 5).
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 1, 4), 3);
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 1, 6), 5);
    }

    #[test]
    fn offset_at_cell_click_in_gutter_is_line_start() {
        let t = text_of("ab\ncdef");
        // Any column inside the 4-wide gutter maps to the line's first byte.
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 1, 1), 3);
    }

    #[test]
    fn offset_at_cell_accounts_for_scroll() {
        let t = text_of("l0\nl1\nl2\nl3");
        // Scrolled down 2 lines: body row 0 is "l2" (byte 6); its start via a gutter
        // click is offset 6.
        assert_eq!(offset_at_cell(&t, 2, 0, 4, 4, 0, 0), 6);
        // Horizontal scroll of 1 shifts the column mapping: on "l3" (starts byte 9),
        // gutter edge col 4 + h_scroll 1 = display col 1 -> the '3' at offset 10.
        assert_eq!(offset_at_cell(&t, 3, 1, 4, 4, 0, 4), 10);
    }

    #[test]
    fn offset_at_cell_clamps_row_past_the_end() {
        let t = text_of("only");
        // A body row below the content clamps to the last line.
        assert_eq!(offset_at_cell(&t, 0, 0, 4, 4, 50, 4), 0);
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
    fn scroll_to_show_works_for_horizontal_axis() {
        // Same helper drives horizontal scroll: cursor col 20, window of 10 cols
        // from left 0 -> scroll right so col 20 sits on the last cell (left 11).
        assert_eq!(scroll_to_show(20, 0, 10), 11);
        // Cursor col 3 left of a left=5 window -> scroll left to 3.
        assert_eq!(scroll_to_show(3, 5, 10), 3);
    }

    /// Concatenated text of an unstyled [`render_line`] over the window - the
    /// clipping/padding behavior, ignoring styles.
    fn rendered(line: &str, h_scroll: usize, width: usize) -> String {
        render_line(line, h_scroll, width, Style::default(), &[])
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    /// The style [`render_line`] assigned to display column `col`, by walking the
    /// spans and their widths.
    fn style_at_col(spans: &[Span], col: usize) -> Style {
        let mut c = 0;
        for s in spans {
            let w = s.width();
            if col < c + w {
                return s.style;
            }
            c += w;
        }
        Style::default()
    }

    #[test]
    fn render_line_slices_ascii_window() {
        // "abcdefgh", window [2, 2+3) -> "cde".
        assert_eq!(rendered("abcdefgh", 2, 3), "cde");
    }

    #[test]
    fn render_line_from_zero_is_left_aligned_prefix() {
        assert_eq!(rendered("abcdefgh", 0, 4), "abcd");
    }

    #[test]
    fn render_line_pads_short_line_to_width() {
        // The line ends before the window does: the remainder is padded with spaces
        // so a row-wide base style (the current-line tint) fills the whole width.
        assert_eq!(rendered("abc", 0, 5), "abc  ");
    }

    #[test]
    fn render_line_past_end_is_all_padding() {
        // Scrolled entirely past the content: the window is all padding spaces.
        assert_eq!(rendered("abc", 10, 5), "     ");
    }

    #[test]
    fn render_line_zero_width_is_empty() {
        assert!(render_line("abc", 0, 0, Style::default(), &[]).is_empty());
    }

    #[test]
    fn render_line_keeps_wide_char_fully_inside() {
        // "日本語" is 3 chars x 2 cells = 6 cells. Window [2, 2+2) is exactly the
        // second char "本".
        assert_eq!(rendered("日本語", 2, 2), "本");
    }

    #[test]
    fn render_line_replaces_wide_char_straddling_left_edge_with_spaces() {
        // Window starts at col 1, mid-"日" (cols 0..2). The 1 visible cell of that
        // glyph becomes a space so "本" (cols 2..4) still lands at the right place.
        assert_eq!(rendered("日本語", 1, 3), " 本");
    }

    #[test]
    fn render_line_replaces_wide_char_straddling_right_edge_with_spaces() {
        // Window [0, 3): "日" fits, then "本" straddles the right edge -> 1 space.
        assert_eq!(rendered("日本語", 0, 3), "日 ");
    }

    #[test]
    fn render_line_overlay_styles_only_its_columns() {
        use ratatui::style::Color;
        let sel = Style::new().bg(Color::Blue);
        let spans = render_line("hello", 0, 5, Style::default(), &[(1..3, sel)]);
        assert_eq!(style_at_col(&spans, 0).bg, None);
        assert_eq!(style_at_col(&spans, 1).bg, Some(Color::Blue));
        assert_eq!(style_at_col(&spans, 2).bg, Some(Color::Blue));
        assert_eq!(style_at_col(&spans, 3).bg, None);
    }

    #[test]
    fn render_line_base_style_fills_padding() {
        use ratatui::style::Color;
        // The base (current-line tint) reaches the padded cells past the content.
        let base = Style::new().bg(Color::Indexed(236));
        let spans = render_line("ab", 0, 5, base, &[]);
        assert_eq!(style_at_col(&spans, 1).bg, Some(Color::Indexed(236)));
        assert_eq!(style_at_col(&spans, 4).bg, Some(Color::Indexed(236)));
    }

    #[test]
    fn render_line_overlay_patches_over_base() {
        use ratatui::style::Color;
        // Selection over the current-line tint: the overlay bg wins on its columns,
        // the base bg holds elsewhere (the layering used on the cursor's row).
        let base = Style::new().bg(Color::Indexed(236));
        let sel = Style::new().bg(Color::Blue);
        let spans = render_line("abcd", 0, 4, base, &[(1..3, sel)]);
        assert_eq!(style_at_col(&spans, 0).bg, Some(Color::Indexed(236)));
        assert_eq!(style_at_col(&spans, 1).bg, Some(Color::Blue));
        assert_eq!(style_at_col(&spans, 3).bg, Some(Color::Indexed(236)));
    }

    #[test]
    fn selection_columns_partial_within_line() {
        // "hello", select bytes 1..4 ("ell") -> display columns 1..4.
        assert_eq!(selection_columns("hello", 0, 6, 4, 1, 4), Some(1..4));
    }

    #[test]
    fn selection_columns_cursor_is_none() {
        // A zero-width selection highlights nothing (the terminal caret shows it).
        assert_eq!(selection_columns("hello", 0, 6, 4, 2, 2), None);
    }

    #[test]
    fn selection_columns_outside_the_line_is_none() {
        // Selection entirely before this line's byte span.
        assert_eq!(selection_columns("hello", 10, 16, 4, 0, 5), None);
    }

    #[test]
    fn selection_columns_through_newline_adds_a_cell() {
        // "ab" + newline (line span [0, 3)); selecting through the break gives the
        // 2 content columns plus one cell for the consumed newline.
        assert_eq!(selection_columns("ab", 0, 3, 4, 0, 3), Some(0..3));
    }

    #[test]
    fn selection_columns_empty_line_in_selection_shows_one_cell() {
        // An empty line swept by a multi-line selection still shows a 1-cell mark.
        assert_eq!(selection_columns("", 5, 6, 4, 0, 10), Some(0..1));
    }

    #[test]
    fn selection_columns_expands_tabs() {
        // "a\tb": selecting the tab (bytes 1..2) covers columns 1..4 (the tab spans
        // to the next 4-stop), matching the painted glyphs.
        assert_eq!(selection_columns("a\tb", 0, 4, 4, 1, 2), Some(1..4));
    }

    #[test]
    fn selected_grapheme_count_counts_clusters_not_bytes() {
        // "héllo": é is 2 bytes, so 6 bytes but 5 graphemes.
        let t = text_of("héllo");
        assert_eq!(
            selected_grapheme_count(&t, &[vortex_core::Selection::new(0, 6)]),
            5
        );
    }

    #[test]
    fn selected_grapheme_count_cursor_is_zero() {
        let t = text_of("hello");
        assert_eq!(
            selected_grapheme_count(&t, &[vortex_core::Selection::cursor(3)]),
            0
        );
    }

    #[test]
    fn selected_grapheme_count_spans_multiple_lines() {
        // "ab\ncd", select bytes 1..5: "b" + "c" + "d" = 3 graphemes (newline
        // excluded from the count).
        let t = text_of("ab\ncd");
        assert_eq!(
            selected_grapheme_count(&t, &[vortex_core::Selection::new(1, 5)]),
            3
        );
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
        let (left, right) = status_bar(2, 5, 0, 38, 7, None);
        assert_eq!(left, " Ln 2, Col 5");
        assert_eq!(right, "38B · v7 ");
    }

    #[test]
    fn status_bar_appends_selection_count_when_active() {
        // A held selection surfaces its size next to the position; an empty one
        // (count 0) leaves the position untouched.
        let (left, _) = status_bar(2, 5, 12, 38, 7, None);
        assert_eq!(left, " Ln 2, Col 5  (12 selected)");
    }

    #[test]
    fn human_size_scales_at_each_1024_mark() {
        // Whole bytes below 1 KB, then KB/MB/GB with one decimal at each boundary.
        assert_eq!(human_size(0), "0B");
        assert_eq!(human_size(1023), "1023B");
        assert_eq!(human_size(1024), "1.0KB");
        assert_eq!(human_size(1536), "1.5KB");
        assert_eq!(human_size(1024 * 1024 - 1), "1024.0KB"); // just under 1 MB
        assert_eq!(human_size(1024 * 1024), "1.0MB");
        assert_eq!(human_size(3 * 1024 * 1024 + 512 * 1024), "3.5MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0GB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0GB"); // caps at GB
    }

    #[test]
    fn status_bar_renders_large_sizes_in_scaled_units() {
        let (_, right) = status_bar(1, 1, 0, 2 * 1024 * 1024, 3, None);
        assert_eq!(right, "2.0MB · v3 ");
    }

    #[test]
    fn status_bar_message_replaces_cursor_position() {
        // A transient file message takes the left slot so the result is visible;
        // metrics stay on the right (SPEC §8).
        let (left, right) = status_bar(2, 5, 0, 38, 7, Some("Saved f.rs"));
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
