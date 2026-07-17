use super::*;

/// Transform an `After`-biased anchor at `offset` through one edit, returning the new
/// offset (the common caret case).
fn after(offset: usize, start: usize, old_end: usize, insert_len: usize) -> usize {
    Anchor::after(offset)
        .transform(start, old_end, insert_len)
        .offset()
}

/// Transform a `Before`-biased anchor at `offset` through one edit.
fn before(offset: usize, start: usize, old_end: usize, insert_len: usize) -> usize {
    Anchor::before(offset)
        .transform(start, old_end, insert_len)
        .offset()
}

#[test]
fn position_before_the_edit_is_unchanged() {
    // Insert 3 bytes at 10; a position at 4 sits before it and does not move.
    assert_eq!(after(4, 10, 10, 3), 4);
    assert_eq!(before(4, 10, 10, 3), 4);
}

#[test]
fn position_after_an_insert_shifts_right() {
    // Insert 3 bytes at 2; a position at 8 shifts to 11.
    assert_eq!(after(8, 2, 2, 3), 11);
    assert_eq!(before(8, 2, 2, 3), 11);
}

#[test]
fn position_after_a_delete_shifts_left() {
    // Delete 2..5 (3 bytes); a position at 9 shifts left by 3 to 6.
    assert_eq!(after(9, 2, 5, 0), 6);
    assert_eq!(before(9, 2, 5, 0), 6);
}

#[test]
fn position_after_a_replace_shifts_by_net_length() {
    // Replace 2..5 (remove 3) with 5 bytes: net +2; a position at 9 -> 11.
    assert_eq!(after(9, 2, 5, 5), 11);
    // A replacement that removes more than it inserts shifts left.
    assert_eq!(after(9, 2, 5, 1), 7);
}

#[test]
fn insertion_at_the_position_respects_bias() {
    // Insert 4 bytes exactly at offset 6.
    // After bias sticks to the text on its right: pushed to 10.
    assert_eq!(after(6, 6, 6, 4), 10);
    // Before bias sticks to the text on its left: stays at 6.
    assert_eq!(before(6, 6, 6, 4), 6);
}

#[test]
fn position_inside_a_deletion_collapses_to_a_boundary() {
    // Delete 2..8; a position at 5 is strictly inside.
    // Before -> left boundary (2); After -> right boundary (2 + 0 inserted = 2).
    assert_eq!(before(5, 2, 8, 0), 2);
    assert_eq!(after(5, 2, 8, 0), 2);
}

#[test]
fn position_inside_a_replacement_collapses_by_bias() {
    // Replace 2..8 with 3 bytes; a position at 5 is inside the removed span.
    // Before clings to the left edge (2); After lands past the inserted text (5).
    assert_eq!(before(5, 2, 8, 3), 2);
    assert_eq!(after(5, 2, 8, 3), 5);
}

#[test]
fn deletion_boundaries_follow_bias() {
    // Delete 2..8. The left edge (2) and right edge (8) are both "at or inside".
    // Left edge: Before stays at 2, After -> 2 (nothing inserted).
    assert_eq!(before(2, 2, 8, 0), 2);
    assert_eq!(after(2, 2, 8, 0), 2);
    // Right edge collapses to the deletion start too (the span is gone).
    assert_eq!(before(8, 2, 8, 0), 2);
    assert_eq!(after(8, 2, 8, 0), 2);
}

#[test]
fn caret_after_typing_lands_past_the_inserted_text() {
    // The load-bearing caret case: a cursor at 3 typing "hi" (After bias, insert at
    // the caret) ends at 5, past the two inserted bytes.
    assert_eq!(after(3, 3, 3, 2), 5);
}

#[test]
fn caret_after_backspace_lands_at_the_deletion_start() {
    // Cursor at 4, backspace deletes 3..4: the caret (After@4, at the deletion's
    // right edge) collapses to 3.
    assert_eq!(after(4, 3, 4, 0), 3);
}

#[test]
fn transform_through_composes_two_inserts() {
    // Two inserts of 1 byte at base offsets 1 and 4 (the multi-cursor "X" case).
    let edits = [
        Edit {
            start: 1,
            old_end: 1,
            insert_len: 1,
        },
        Edit {
            start: 4,
            old_end: 4,
            insert_len: 1,
        },
    ];
    // Caret at 1: its own insert pushes it to 2; the later insert is past it.
    assert_eq!(Anchor::after(1).transform_through(&edits).offset(), 2);
    // Caret at 4: the earlier insert shifts it to 5, then its own insert -> 6.
    assert_eq!(Anchor::after(4).transform_through(&edits).offset(), 6);
    // A position before both edits is untouched.
    assert_eq!(Anchor::after(0).transform_through(&edits).offset(), 0);
}

#[test]
fn transform_through_composes_mixed_insert_and_delete() {
    // Delete 1..3 (2 bytes) then insert 2 bytes at base 6.
    let edits = [
        Edit {
            start: 1,
            old_end: 3,
            insert_len: 0,
        },
        Edit {
            start: 6,
            old_end: 6,
            insert_len: 2,
        },
    ];
    // A position at 8 (after both): -2 from the delete, +2 from the insert -> 8.
    assert_eq!(Anchor::after(8).transform_through(&edits).offset(), 8);
    // A position at 5 (after the delete, before the insert) -> 3.
    assert_eq!(Anchor::after(5).transform_through(&edits).offset(), 3);
    // A position inside the deletion collapses to its start (1).
    assert_eq!(Anchor::before(2).transform_through(&edits).offset(), 1);
}

#[test]
fn transform_through_empty_batch_is_identity() {
    assert_eq!(Anchor::after(7).transform_through(&[]).offset(), 7);
    assert_eq!(Anchor::before(7).transform_through(&[]).offset(), 7);
}

#[test]
fn offset_and_constructors_round_trip() {
    assert_eq!(Anchor::before(9).offset(), 9);
    assert_eq!(Anchor::after(9).offset(), 9);
    // Bias is preserved across a transform (an anchor keeps its gravity).
    let a = Anchor::before(0).transform(0, 0, 3);
    assert_eq!(a, Anchor::before(0)); // Before@0, insert at 0 -> stays at 0
}
