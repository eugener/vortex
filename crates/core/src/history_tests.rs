use super::*;
use crate::selection::Selection;

/// A single-cursor selection set at `offset` - the common shape a character insert
/// brackets its revision with.
fn caret(offset: usize) -> SelectionSet {
    SelectionSet::single(Selection::cursor(offset))
}

/// A pure insert of `text` at `at` (in the pre-edit buffer), with the caret moving
/// from `at` to just past the inserted text - the exact shape `apply_edit` records
/// for a single-cursor `Insert`.
fn insert(at: usize, text: &str) -> (Vec<Change>, SelectionSet, SelectionSet) {
    (
        vec![Change {
            start: at,
            removed: String::new(),
            inserted: text.to_string(),
        }],
        caret(at),
        caret(at + text.len()),
    )
}

/// Apply an edit list (descending by start, as `undo`/`redo` return) to `buf` so a
/// test can assert the reverted text without a real buffer.
fn apply(buf: &mut String, edits: &[(Range<usize>, String)]) {
    for (range, replacement) in edits {
        buf.replace_range(range.clone(), replacement);
    }
}

#[test]
fn fresh_history_is_at_the_saved_root_with_nothing_to_undo() {
    let mut h = History::new();
    assert!(h.at_saved());
    assert!(h.undo().is_none());
    assert!(h.redo().is_none());
}

#[test]
fn default_matches_new() {
    // `Default` is the idiomatic pairing for a no-arg `new`; it must give the same
    // fresh, saved-at-root history.
    let mut h = History::default();
    assert!(h.at_saved());
    assert!(h.undo().is_none());
}

#[test]
fn coalescing_guards_refuse_a_non_coalescable_target() {
    // `try_coalesce` runs only when `coalescing` is set, which the public `record`
    // path only sets after a coalescable insert - so its "target is the root" and
    // "target is not a coalescable insert" guards are defensive. Force each state
    // directly (the child test module can touch private fields) and confirm the
    // guards make `record` create a new revision instead of mutating the target.

    // (1) Target is the root (no revision to fold into).
    let mut at_root = History::new();
    at_root.coalescing = true; // an impossible-through-public-API state
    at_root.record(insert(0, "a").0, caret(0), caret(1));
    // A new child was created rather than coalescing into the root.
    assert!(at_root.undo().is_some());
    assert!(at_root.at_saved(), "undo returned to the root");

    // (2) Target is a non-coalescable revision (a delete).
    let mut at_delete = History::new();
    let delete = vec![Change {
        start: 0,
        removed: "a".to_string(),
        inserted: String::new(),
    }];
    at_delete.record(delete, caret(1), caret(0));
    at_delete.coalescing = true; // pretend a run is open over the delete node
    at_delete.record(insert(0, "b").0, caret(0), caret(1));
    // The insert did not merge into the delete: two separate units remain.
    assert!(at_delete.undo().is_some()); // undo the insert
    assert!(at_delete.undo().is_some()); // undo the delete
    assert!(at_delete.undo().is_none());
}

#[test]
fn record_then_undo_yields_the_inverse_and_before_selections() {
    let mut h = History::new();
    let (changes, before, after) = insert(0, "hello");
    h.record(changes, before, after);
    assert!(!h.at_saved()); // moved off the saved root

    let mut buf = "hello".to_string();
    let reverted = h.undo().expect("one edit to undo");
    apply(&mut buf, &reverted.edits);
    assert_eq!(buf, ""); // the insert was inverted
    assert_eq!(reverted.selections, caret(0)); // caret restored to before-edit
    assert!(h.at_saved()); // back at the root, which is saved
}

#[test]
fn undo_then_redo_round_trips_text_and_selections() {
    let mut h = History::new();
    let (changes, before, after) = insert(0, "hi");
    h.record(changes, before, after);

    let mut buf = "hi".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "");

    let redo = h.redo().expect("one edit to redo");
    apply(&mut buf, &redo.edits);
    assert_eq!(buf, "hi");
    assert_eq!(redo.selections, caret(2)); // after-edit caret
}

#[test]
fn adjacent_char_inserts_coalesce_into_one_undo_unit() {
    // Type 'a', 'b', 'c' as three separate single-char inserts with no motion
    // between them: they collapse into one revision, so a single undo removes all.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1));
    h.record(insert(1, "b").0, caret(1), caret(2));
    h.record(insert(2, "c").0, caret(2), caret(3));

    let mut buf = "abc".to_string();
    let reverted = h.undo().unwrap();
    apply(&mut buf, &reverted.edits);
    assert_eq!(buf, "", "one undo should remove the whole coalesced run");
    assert_eq!(reverted.selections, caret(0));
    assert!(h.undo().is_none(), "nothing left after the single unit");
}

#[test]
fn a_newline_insert_breaks_coalescing() {
    // 'a' then '\n' then 'b': the newline is not coalescable (rule (c)), so the run
    // splits into three revisions - each undo peels one off.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1));
    h.record(insert(1, "\n").0, caret(1), caret(2));
    h.record(insert(2, "b").0, caret(2), caret(3));

    let mut buf = "a\nb".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "a\n"); // only 'b' undone
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "a"); // then the newline
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, ""); // then 'a'
}

#[test]
fn a_multi_char_insert_does_not_coalesce_with_adjacent_typing() {
    // A multi-character insert (a paste / bracketed paste - one `Insert` of the whole
    // payload) is its own undo unit even with no newline: only a single typed grapheme
    // opens or extends a run. Type 'a', paste "bc" adjacently, type 'd'; the paste and
    // each typed char stay separate, so three undos peel them one at a time.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1)); // single char: opens a run
    h.record(insert(1, "bc").0, caret(1), caret(3)); // multi-char: its own unit
    h.record(insert(3, "d").0, caret(3), caret(4)); // typing after paste: new unit

    let mut buf = "abcd".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "abc", "the trailing 'd' is its own unit");
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "a", "the pasted \"bc\" is its own unit");
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "", "the leading 'a' remains");
}

#[test]
fn a_single_multibyte_grapheme_still_coalesces() {
    // A typed multi-byte character (é, an emoji) is one grapheme from one keypress, so
    // it must still coalesce - the fix keys on grapheme count, not byte length.
    let mut h = History::new();
    h.record(insert(0, "é").0, caret(0), caret(2)); // 2 bytes, 1 grapheme
    h.record(insert(2, "a").0, caret(2), caret(3)); // adjacent single char

    let mut buf = "éa".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "", "one undo removes the whole coalesced run");
}

#[test]
fn break_coalescing_starts_a_new_undo_unit() {
    // The explicit break (a paste, a save) prevents the merge, so undo peels the
    // second insert only.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1));
    h.break_coalescing();
    h.record(insert(1, "b").0, caret(1), caret(2));

    let mut buf = "ab".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "a"); // 'b' only
}

#[test]
fn adjacency_still_guards_a_matching_selection_set() {
    // The converse of the test below, and the reason rule (b) stays a separate check.
    // Coalescing appends the new text onto the current revision's `inserted`, which is
    // only sound if the two edits are contiguous. Here the selections match (so the
    // rule-(d) check passes) but the edit lands at offset 5 rather than 1 - another
    // forced state, since a real insert happens *at* its caret. Without this guard the
    // run would merge two disjoint edits into one revision whose recorded text never
    // existed in the buffer, and undo would restore garbage.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1));
    let non_adjacent = vec![Change {
        start: 5,
        removed: String::new(),
        inserted: "b".to_string(),
    }];
    h.record(non_adjacent, caret(1), caret(6));

    assert!(h.undo().is_some(), "the second insert is its own unit");
    assert!(h.undo().is_some(), "the first insert survives separately");
    assert!(h.undo().is_none());
}

#[test]
fn a_changed_selection_set_breaks_the_run_even_when_the_edit_stays_adjacent() {
    // Isolates the rule-(d) selection check from the rule-(b) adjacency check: the
    // second insert *is* adjacent (offset 1, right where the first ended), so only
    // the selection comparison can refuse it.
    //
    // Like `coalescing_guards_refuse_a_non_coalescable_target`, this forces a state
    // the public API cannot currently produce - today an N-caret set fans an insert
    // into N changes, which `is_typed_grapheme` already rejects, so a one-change
    // edit always carries a one-caret set whose position adjacency alone would
    // catch. The guard is what makes rule (d) hold *structurally* rather than as a
    // side effect of adjacency, which is what lets the editor drop its per-action
    // break calls; this test is what keeps it honest as new selection actions land.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1));
    let two_carets =
        SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(9)]);
    h.record(insert(1, "b").0, two_carets.clone(), two_carets);

    let mut buf = "ab".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "a", "the cursor-set change ended the run");
}

#[test]
fn non_adjacent_insert_does_not_coalesce() {
    // Two inserts that are not back-to-back (rule (b)): a caret jump would leave
    // them at non-adjacent offsets. Even without an explicit break, adjacency fails.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1));
    // Next insert at offset 5, not at 1 (where the previous insert ended).
    h.record(insert(5, "b").0, caret(5), caret(6));

    assert!(h.undo().is_some()); // second revision exists on its own
    // After undoing the second, a first revision still remains.
    let mut h2 = History::new();
    h2.record(insert(0, "a").0, caret(0), caret(1));
    h2.record(insert(5, "b").0, caret(5), caret(6));
    h2.undo();
    assert!(h2.undo().is_some(), "the first insert is a separate unit");
}

#[test]
fn a_delete_is_its_own_unit_and_breaks_the_insert_run() {
    // Insert 'a' (coalescable), then a delete (removes something): the delete is a
    // separate revision, and a following insert does not merge into the delete.
    let mut h = History::new();
    h.record(insert(0, "ab").0, caret(0), caret(2));
    // Backspace: remove "b" at offset 1.
    let delete = vec![Change {
        start: 1,
        removed: "b".to_string(),
        inserted: String::new(),
    }];
    h.record(delete, caret(2), caret(1));

    let mut buf = "a".to_string();
    // Undo the delete: "b" comes back.
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "ab");
    // Undo the insert: back to empty.
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "");
}

#[test]
fn typing_after_undo_forks_a_branch_and_redo_takes_the_newest() {
    // The tree's reason to exist (SPEC §2.4): undo, then type - the old redo branch
    // survives, and redo now follows the newest branch, not the discarded one.
    let mut h = History::new();
    h.record(insert(0, "a").0, caret(0), caret(1)); // branch A: "a"
    h.undo(); // back to root
    h.record(insert(0, "b").0, caret(0), caret(1)); // branch B: "b" (newest)

    // Undo B, then redo: we must land back on B (newest child), not A.
    let mut buf = "b".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(buf, "");
    let redo = h.redo().unwrap();
    apply(&mut buf, &redo.edits);
    assert_eq!(buf, "b", "redo follows the newest branch");
}

#[test]
fn save_point_tracks_across_undo_and_redo() {
    // Save at a node, edit away (modified), undo back to it (clean again), redo away
    // (modified again) - the save marker follows node identity, not text.
    let mut h = History::new();
    h.record(insert(0, "x").0, caret(0), caret(1));
    h.mark_saved();
    assert!(h.at_saved());

    h.break_coalescing();
    h.record(insert(1, "y").0, caret(1), caret(2));
    assert!(!h.at_saved(), "edited past the save point");

    h.undo();
    assert!(h.at_saved(), "undo back to the saved node is clean");

    h.redo();
    assert!(!h.at_saved(), "redo away from it is modified again");
}

#[test]
fn a_save_does_not_let_later_typing_mutate_the_saved_revision() {
    // Regression: without breaking coalescing on save, typing right after a save
    // would coalesce into the saved node, changing its content out from under the
    // on-disk file - so undoing back to "saved" would no longer match disk.
    let mut h = History::new();
    h.record(insert(0, "x").0, caret(0), caret(1));
    h.mark_saved();
    // Adjacent single-char insert: would coalesce if the save had not broken the run.
    h.record(insert(1, "y").0, caret(1), caret(2));
    assert!(!h.at_saved(), "typing moved off the saved node");

    // Undo the "y": we must be back at the saved node with just "x".
    let mut buf = "xy".to_string();
    apply(&mut buf, &h.undo().unwrap().edits);
    assert_eq!(
        buf, "x",
        "the saved revision still holds exactly what was saved"
    );
    assert!(h.at_saved());
}

#[test]
fn multi_change_revision_inverts_with_correct_offsets() {
    // A single action over two cursors: insert "XX" at offsets 1 and 4 of "abcdef".
    // The inverse must delete both inserted spans in the shifted (child) buffer.
    let mut h = History::new();
    let changes = vec![
        Change {
            start: 1,
            removed: String::new(),
            inserted: "XX".to_string(),
        },
        Change {
            start: 4,
            removed: String::new(),
            inserted: "XX".to_string(),
        },
    ];
    // Selections are incidental here; use the origin set.
    h.record(
        changes,
        SelectionSet::at_origin(),
        SelectionSet::at_origin(),
    );

    // Forward buffer: inserts apply back-to-front (at 4, then at 1), so
    // "abcdef" -> "abcdXXef" -> "aXXbcdXXef".
    let mut buf = "aXXbcdXXef".to_string();
    let reverted = h.undo().unwrap();
    apply(&mut buf, &reverted.edits);
    assert_eq!(
        buf, "abcdef",
        "both inserted spans removed at shifted offsets"
    );
}

#[test]
fn multi_change_revision_redoes_with_correct_offsets() {
    let mut h = History::new();
    let changes = vec![
        Change {
            start: 1,
            removed: String::new(),
            inserted: "XX".to_string(),
        },
        Change {
            start: 4,
            removed: String::new(),
            inserted: "XX".to_string(),
        },
    ];
    h.record(
        changes,
        SelectionSet::at_origin(),
        SelectionSet::at_origin(),
    );

    h.undo(); // buf conceptually back to "abcdef"
    let mut buf = "abcdef".to_string();
    let redo = h.redo().unwrap();
    apply(&mut buf, &redo.edits);
    assert_eq!(
        buf, "aXXbcdXXef",
        "redo re-applies both inserts in parent coords"
    );
}
