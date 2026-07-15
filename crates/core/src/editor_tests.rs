use super::*;
use crate::selection::Motion;

// Directly exercise the pure edit-planning logic that the async actor path
// wraps. These cover the multi-cursor branches (descending edit sort, offset
// shift composition) that the single-selection public seam cannot yet reach
// from a message script - the machinery is built now (SPEC §2.2) so M3's
// multi-cursor rides on tested code.

fn editor_with(text: &str, selections: SelectionSet) -> Editor {
    let mut e = Editor::new();
    e.buffer = RopeBuffer::from(text);
    e.selections = selections;
    e
}

#[test]
fn plan_insert_over_two_cursors_is_descending() {
    // Two cursors; an insert plans one edit each, sorted descending by start
    // so back-to-front application keeps offsets stable.
    let set = SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
    let e = editor_with("abcdef", set);
    let edits = e.plan_edit(EditKind::Insert("X".into()));
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0].0.start, 4); // later cursor first
    assert_eq!(edits[1].0.start, 1);
}

#[test]
fn selections_after_two_inserts_account_for_shift() {
    // Edits at (descending) starts 4 and 1, each inserting "X" (1 byte).
    // "abcdef" -> insert X at 1 -> "aXbcdef" (caret 2) -> insert X at shifted
    // 5 -> "aXbcXdef" (caret 6). The earlier insert's +1 shift moves the
    // later caret from 5 to 6.
    let edits = vec![(4..4, "X".to_string()), (1..1, "X".to_string())];
    let set = selections_after_edits(&edits);
    let cursors: Vec<usize> = set.all().iter().map(|s| s.head).collect();
    assert_eq!(cursors, vec![2, 6]);
}

#[test]
fn plan_delete_backward_over_two_cursors() {
    let set = SelectionSet::from_sorted_cursors(vec![Selection::cursor(2), Selection::cursor(5)]);
    let e = editor_with("abcdef", set);
    let edits = e.plan_edit(EditKind::DeleteBackward);
    // Each cursor deletes the grapheme before it: ranges 4..5 and 1..2.
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0].0, 4..5);
    assert_eq!(edits[1].0, 1..2);
}

#[test]
fn move_cursor_helper_maps_over_buffer() {
    let mut e = editor_with("hello", SelectionSet::at_origin());
    e.move_cursor(Motion::Right, false);
    assert_eq!(e.selections.primary().head, 1);
}

#[test]
fn snapshot_reflects_state() {
    let e = editor_with("hi", SelectionSet::single(Selection::cursor(2)));
    let snap = e.snapshot(Some(0..2));
    assert_eq!(snap.text.to_string(), "hi");
    assert_eq!(snap.dirty, Some(0..2));
    assert_eq!(snap.selections.as_ref(), &[Selection::cursor(2)]);
}
