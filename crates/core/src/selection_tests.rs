use super::*;
use crate::buffer::Buffer;
use crate::buffer::RopeBuffer;

fn text(s: &str) -> Text {
    RopeBuffer::from(s).text()
}

#[test]
fn cursor_is_zero_width() {
    let c = Selection::cursor(5);
    assert!(c.is_cursor());
    assert_eq!(c.start(), 5);
    assert_eq!(c.end(), 5);
}

#[test]
fn selection_span_regardless_of_direction() {
    let forward = Selection::new(2, 7);
    let backward = Selection::new(7, 2);
    assert_eq!((forward.start(), forward.end()), (2, 7));
    assert_eq!((backward.start(), backward.end()), (2, 7));
    assert!(!forward.is_cursor());
}

#[test]
fn equality_ignores_goal_column() {
    let mut a = Selection::cursor(3);
    a.goal_column = Some(9);
    let b = Selection::cursor(3);
    assert_eq!(a, b);
}

#[test]
fn set_is_never_empty() {
    let s = SelectionSet::at_origin();
    assert_eq!(s.len(), 1);
    assert!(!s.is_empty());
    assert_eq!(*s.primary(), Selection::cursor(0));
}

#[test]
fn move_right_over_ascii() {
    let t = text("hello");
    let mut s = SelectionSet::at_origin();
    s.move_all(&t, Motion::Right, false);
    assert_eq!(s.primary().head, 1);
    assert!(s.primary().is_cursor()); // non-extend collapses
}

#[test]
fn move_right_over_multibyte_grapheme() {
    // A ZWJ family cluster is 25 bytes / several code points but ONE grapheme:
    // moving right must land past the whole cluster, not mid-codepoint (§4).
    let family = "👨‍👩‍👧";
    let t = text(family);
    let mut s = SelectionSet::at_origin();
    s.move_all(&t, Motion::Right, false);
    assert_eq!(s.primary().head, family.len());
}

#[test]
fn move_left_over_multibyte_grapheme() {
    let family = "👨‍👩‍👧";
    let t = text(family);
    let mut s = SelectionSet::single(Selection::cursor(family.len()));
    s.move_all(&t, Motion::Left, false);
    assert_eq!(s.primary().head, 0);
}

#[test]
fn move_right_wraps_to_next_line() {
    let t = text("ab\ncd");
    let mut s = SelectionSet::single(Selection::cursor(2)); // end of "ab"
    s.move_all(&t, Motion::Right, false);
    assert_eq!(s.primary().head, 3); // start of "cd"
}

#[test]
fn move_left_wraps_to_previous_line_end() {
    let t = text("ab\ncd");
    let mut s = SelectionSet::single(Selection::cursor(3)); // start of "cd"
    s.move_all(&t, Motion::Left, false);
    assert_eq!(s.primary().head, 2); // end of "ab" content
}

#[test]
fn move_left_at_buffer_start_stays() {
    let t = text("abc");
    let mut s = SelectionSet::at_origin();
    s.move_all(&t, Motion::Left, false);
    assert_eq!(s.primary().head, 0);
}

#[test]
fn move_right_at_buffer_end_stays() {
    let t = text("abc");
    let mut s = SelectionSet::single(Selection::cursor(3));
    s.move_all(&t, Motion::Right, false);
    assert_eq!(s.primary().head, 3);
}

#[test]
fn extend_keeps_anchor() {
    let t = text("hello");
    let mut s = SelectionSet::at_origin();
    s.move_all(&t, Motion::Right, true);
    s.move_all(&t, Motion::Right, true);
    assert_eq!(s.primary().anchor, 0);
    assert_eq!(s.primary().head, 2);
    assert!(!s.primary().is_cursor());
}

#[test]
fn line_start_and_end() {
    let t = text("ab\ncdef\ng");
    let mut s = SelectionSet::single(Selection::cursor(5)); // inside "cdef"
    s.move_all(&t, Motion::LineStart, false);
    assert_eq!(s.primary().head, 3); // start of "cdef"
    s.move_all(&t, Motion::LineEnd, false);
    assert_eq!(s.primary().head, 7); // end of "cdef" content
}

#[test]
fn buffer_start_and_end() {
    let t = text("ab\ncd");
    let mut s = SelectionSet::single(Selection::cursor(2));
    s.move_all(&t, Motion::BufferEnd, false);
    assert_eq!(s.primary().head, 5);
    s.move_all(&t, Motion::BufferStart, false);
    assert_eq!(s.primary().head, 0);
}

#[test]
fn vertical_preserves_goal_column_through_short_line() {
    // col 4 on line 0; line 1 is shorter (2 chars) so Down clamps to col 2;
    // Down again must return to col 4 on line 2 using the retained goal.
    let t = text("abcde\nxy\nfghij");
    let mut s = SelectionSet::single(Selection::cursor(4)); // "abcd|e"
    s.move_all(&t, Motion::Down, false);
    // line 1 "xy" clamps to its end, byte offset 6+2 = 8.
    assert_eq!(s.primary().head, 8);
    s.move_all(&t, Motion::Down, false);
    // line 2 "fghij" starts at byte 9; goal col 4 => byte 13.
    assert_eq!(s.primary().head, 13);
}

#[test]
fn vertical_goal_resets_after_horizontal_motion() {
    let t = text("abcde\nxy\nfghij");
    let mut s = SelectionSet::single(Selection::cursor(4));
    s.move_all(&t, Motion::Down, false); // to "xy" end (col 2, byte 8)
    s.move_all(&t, Motion::Left, false); // horizontal: clears goal, col now 1
    s.move_all(&t, Motion::Down, false); // goal recomputed from col 1
    // line 2 "fghij" start 9; col 1 => byte 10.
    assert_eq!(s.primary().head, 10);
}

#[test]
fn vertical_up_at_top_and_down_at_bottom_stay() {
    let t = text("ab\ncd");
    let mut top = SelectionSet::single(Selection::cursor(1));
    top.move_all(&t, Motion::Up, false);
    assert_eq!(top.primary().head, 1);

    let mut bottom = SelectionSet::single(Selection::cursor(4));
    bottom.move_all(&t, Motion::Down, false);
    assert_eq!(bottom.primary().head, 4);
}

#[test]
fn vertical_moves_up_one_line() {
    let t = text("abcde\nfghij");
    let mut s = SelectionSet::single(Selection::cursor(9)); // "fgh|ij" col 3
    s.move_all(&t, Motion::Up, false);
    assert_eq!(s.primary().head, 3); // col 3 on line 0
}

#[test]
fn page_down_moves_n_lines_keeping_goal_column() {
    // 5 lines "l0".."l4", each 3 bytes incl. newline. Cursor at col 1 of line 0.
    let t = text("aa\nbb\ncc\ndd\nee");
    let mut s = SelectionSet::single(Selection::cursor(1)); // "a|a" col 1, line 0
    s.move_all(&t, Motion::PageDown(3), false);
    // Down 3 lines -> line 3 "dd", col 1 -> byte 9 (line 3 starts at 9) + 1.
    assert_eq!(s.primary().head, 10);
}

#[test]
fn page_up_moves_n_lines_keeping_goal_column() {
    let t = text("aa\nbb\ncc\ndd\nee");
    let mut s = SelectionSet::single(Selection::cursor(13)); // line 4 "e|e" col 1
    s.move_all(&t, Motion::PageUp(2), false);
    // Up 2 lines -> line 2 "cc", col 1 -> byte 6 + 1.
    assert_eq!(s.primary().head, 7);
}

#[test]
fn down_on_the_virtual_trailing_line_stays_put() {
    // A newline-terminated buffer has a virtual empty line below the last content
    // line, reachable by Right at end-of-file (offset == byte_len). A Down there
    // must not collapse the caret to a line above it (the clamp ceiling used to be
    // `line_count - 1`, one short of that trailing line).
    let t = text("a\n"); // line 0 "a", virtual line 1 at offset 2
    let mut s = SelectionSet::single(Selection::cursor(2));
    s.move_all(&t, Motion::Down, false);
    assert_eq!(
        s.primary().head,
        2,
        "Down on the trailing line should stay put"
    );

    let t = text("a\nb\n"); // virtual line 2 at offset 4
    let mut s = SelectionSet::single(Selection::cursor(4));
    s.move_all(&t, Motion::PageDown(3), false);
    assert_eq!(
        s.primary().head,
        4,
        "PageDown on the trailing line should stay put"
    );
}

#[test]
fn down_reaches_the_virtual_trailing_line_and_up_returns() {
    // Down from the last content line lands on the trailing empty line; Up comes
    // back. The trailing line is navigable, consistent with horizontal motion.
    let t = text("a\n");
    let mut s = SelectionSet::at_origin(); // caret at 0 on "a"
    s.move_all(&t, Motion::Down, false);
    assert_eq!(
        s.primary().head,
        2,
        "Down should reach the trailing empty line"
    );
    s.move_all(&t, Motion::Up, false);
    assert_eq!(s.primary().head, 0, "Up should return to the content line");
}

#[test]
fn move_left_between_cr_and_lf_does_not_panic() {
    // A caret can land between a CR and LF when an edit/paste inserts a lone CR next
    // to an existing LF ("a\r\nb", caret at byte 2). crop treats "\r\n" as one line
    // break, so the caret's byte column (2) exceeds line 0's content "a" (len 1);
    // grapheme_before must clamp rather than slice out of bounds (SPEC §8).
    let t = text("a\r\nb");
    let mut s = SelectionSet::single(Selection::cursor(2)); // between \r and \n
    s.move_all(&t, Motion::Left, false);
    assert_eq!(
        s.primary().head,
        1,
        "Left should step back over the CR to offset 1"
    );
}

#[test]
fn page_motion_clamps_to_buffer_edges() {
    // A page larger than the buffer lands on the first/last line, not past it.
    let t = text("aa\nbb\ncc");
    let mut down = SelectionSet::single(Selection::cursor(0));
    down.move_all(&t, Motion::PageDown(100), false);
    // Last line "cc" starts at byte 6; goal col 0 -> byte 6.
    assert_eq!(down.primary().head, 6);

    let mut up = SelectionSet::single(Selection::cursor(7)); // line 2 col 1
    up.move_all(&t, Motion::PageUp(100), false);
    // First line "aa", goal col 1 -> byte 1.
    assert_eq!(up.primary().head, 1);
}

#[test]
fn page_down_preserves_goal_through_a_short_line() {
    // Goal column survives a page motion that lands on (or crosses) a short line,
    // the same contract as single-step vertical motion.
    let t = text("aaaaa\nbb\nccccc"); // line 1 "bb" is short
    let mut s = SelectionSet::single(Selection::cursor(4)); // line 0 col 4
    s.move_all(&t, Motion::PageDown(1), false); // -> line 1 "bb", clamped to col 2
    assert_eq!(s.primary().head, 8); // byte 6 (line 1 start) + 2
    s.move_all(&t, Motion::PageDown(1), false); // -> line 2, goal col 4 restored
    assert_eq!(s.primary().head, 13); // byte 9 (line 2 start) + 4
}

#[test]
fn page_down_extends_selection_when_asked() {
    let t = text("aa\nbb\ncc\ndd");
    let mut s = SelectionSet::single(Selection::cursor(0));
    s.move_all(&t, Motion::PageDown(2), true); // extend
    let sel = s.primary();
    assert_eq!(sel.anchor, 0); // anchor held
    assert_eq!(sel.head, 6); // moved to line 2 col 0
    assert!(!sel.is_cursor());
}

#[test]
fn overlapping_selections_merge() {
    // Directly exercise the disjoint invariant with two overlapping ranges.
    let mut set = SelectionSet {
        selections: vec![Selection::new(0, 5), Selection::new(3, 8)],
        primary: 1,
    };
    // A no-op absolute motion still runs normalize over the set.
    let t = text("0123456789");
    set.move_all(&t, Motion::BufferEnd, true); // extend keeps anchors
    // After extend to buffer end (10), both heads go to 10; spans [0,10] and
    // [3,10] overlap and merge into one.
    assert_eq!(set.len(), 1);
    assert_eq!(*set.primary(), Selection::new(0, 10));
}

#[test]
fn disjoint_cursors_do_not_merge() {
    let t = text("abcdef");
    let mut set = SelectionSet {
        selections: vec![Selection::cursor(1), Selection::cursor(4)],
        primary: 0,
    };
    set.move_all(&t, Motion::Left, false); // 1->0, 4->3; still disjoint
    assert_eq!(set.len(), 2);
    assert_eq!(set.all()[0], Selection::cursor(0));
    assert_eq!(set.all()[1], Selection::cursor(3));
}

#[test]
fn coincident_cursors_merge_to_one() {
    let t = text("abcdef");
    let mut set = SelectionSet {
        selections: vec![Selection::cursor(1), Selection::cursor(2)],
        primary: 1,
    };
    // Move both left: 1->0, 2->1; still disjoint. Move left again: 0->0, 1->0
    // => coincident, merge.
    set.move_all(&t, Motion::Left, false);
    set.move_all(&t, Motion::Left, false);
    assert_eq!(set.len(), 1);
    assert_eq!(set.primary().head, 0);
}

#[test]
fn primary_tracks_through_merge() {
    let t = text("0123456789");
    let mut set = SelectionSet {
        selections: vec![Selection::new(0, 2), Selection::new(6, 8)],
        primary: 1, // the [6,8] selection
    };
    // Extend the second selection's head left into the first so they merge.
    // Do it via a direct construction + normalize by moving BufferStart on
    // all with extend: heads -> 0, spans [0,0] and [0,6]? Instead test merge
    // keeps a valid primary index within bounds.
    set.move_all(&t, Motion::BufferStart, false); // all collapse to cursor 0
    assert_eq!(set.len(), 1);
    assert!(set.primary().is_cursor());
    assert_eq!(set.primary().head, 0);
}

#[test]
fn primary_index_points_at_primary_selection() {
    let set = SelectionSet {
        selections: vec![Selection::new(0, 2), Selection::new(6, 8)],
        primary: 1,
    };
    assert_eq!(set.primary_index(), 1);
    assert_eq!(&set.all()[set.primary_index()], set.primary());
}

#[test]
fn byte_of_position_consistency_smoke() {
    // Guard that motions and buffer coordinate conversion agree: moving right
    // grapheme-by-grapheme visits the same offsets Buffer reports as valid.
    let b = RopeBuffer::from("a日b\ncd");
    let t = b.text();
    let mut s = SelectionSet::at_origin();
    let mut seen = vec![0];
    for _ in 0..6 {
        s.move_all(&t, Motion::Right, false);
        seen.push(s.primary().head);
    }
    // Every visited offset is a valid position in the buffer.
    for &off in &seen {
        let pos = b.position_of_byte(off);
        assert_eq!(b.byte_of_position(pos), Some(off));
    }
}
