use super::*;
use crate::selection::Motion;
use std::sync::atomic::{AtomicU64, Ordering};

/// A unique temp directory for one test, removed on drop so file tests stay
/// hermetic without a `tempfile` dependency. The name mixes the process id with a
/// per-process counter so parallel tests never collide.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("vortex-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// A path to `name` inside this dir (the file need not exist).
    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Channels + sink for driving `open_file`/`save_file`/`apply_edit` directly, the
/// way [`multi_cursor_insert_merges_dirty_range`] drives `apply_edit`. Returns the
/// sender/receivers so a test can assert on emitted deltas and notifications.
struct Harness {
    delta_tx: Sender<Delta>,
    delta_rx: Receiver<Delta>,
    snapshots: SnapshotSink,
    // Kept so `publish` succeeds (it returns false / the file ops bail as
    // "frontend gone" if the receiver is dropped) and so tests can read the
    // published snapshot back via [`Harness::snapshot`].
    snap_rx: Receiver<ViewSnapshot>,
    note_tx: Sender<Notification>,
    note_rx: Receiver<Notification>,
}

impl Harness {
    fn new() -> Self {
        let (delta_tx, delta_rx) = async_channel::bounded::<Delta>(16);
        let (snap_tx, snap_rx) = async_channel::bounded::<ViewSnapshot>(1);
        let (note_tx, note_rx) = async_channel::bounded::<Notification>(16);
        Self {
            delta_tx,
            delta_rx,
            snapshots: SnapshotSink { tx: snap_tx },
            snap_rx,
            note_tx,
            note_rx,
        }
    }

    /// The last snapshot the file op published (latest-wins cell).
    fn snapshot(&self) -> ViewSnapshot {
        self.snap_rx.try_recv().expect("a snapshot was published")
    }
}

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

/// Put the editor in the "modified" state by recording a dummy revision, moving
/// history off its saved node - the same state a real edit leaves behind
/// (`modified` is derived from the history, not stored).
fn mark_dirty(e: &mut Editor) {
    e.history.record(
        vec![Change {
            start: 0,
            removed: String::new(),
            inserted: "x".into(),
        }],
        e.selections.clone(),
        e.selections.clone(),
    );
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
    // Two pre-edit carets at 1 and 4, each inserting "X" (1 byte) at itself.
    // "abcdef" -> caret 1's X -> caret 2; caret 4 shifts to 5 by the earlier
    // insert, then its own X -> caret 6. Each caret is an After-anchor transformed
    // through the applied edits (SPEC §2.1).
    let before =
        SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
    let changes = vec![
        Change {
            start: 1,
            removed: String::new(),
            inserted: "X".to_string(),
        },
        Change {
            start: 4,
            removed: String::new(),
            inserted: "X".to_string(),
        },
    ];
    let set = selections_after_edits(&before, &changes);
    let cursors: Vec<usize> = set.all().iter().map(|s| s.head).collect();
    assert_eq!(cursors, vec![2, 6]);
}

#[test]
fn selections_after_edits_keeps_a_no_op_cursor() {
    // Multi-cursor: a cursor at buffer start (whose backspace is a no-op) must
    // survive an edit made by another cursor, shifted by it - not be dropped. Here a
    // delete at offset 3..4 leaves the start caret at 0 and pulls the second caret in.
    let before =
        SelectionSet::from_sorted_cursors(vec![Selection::cursor(0), Selection::cursor(4)]);
    let changes = vec![Change {
        start: 3,
        removed: "d".to_string(),
        inserted: String::new(),
    }];
    let set = selections_after_edits(&before, &changes);
    let cursors: Vec<usize> = set.all().iter().map(|s| s.head).collect();
    assert_eq!(
        cursors,
        vec![0, 3],
        "the start cursor is kept, the other shifts"
    );
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
fn place_cursor_helper_sets_and_extends_caret() {
    let mut e = editor_with("hello", SelectionSet::at_origin());
    // A plain click places a cursor at the offset.
    e.place_cursor(3, false);
    assert_eq!(*e.selections.primary(), Selection::cursor(3));
    // A drag/extend keeps the anchor and moves only the head.
    e.place_cursor(5, true);
    assert_eq!(*e.selections.primary(), Selection::new(3, 5));
}

#[test]
fn snapshot_reflects_state() {
    let e = editor_with("hi", SelectionSet::single(Selection::cursor(2)));
    let snap = e.snapshot(Some(0..2));
    assert_eq!(snap.text.to_string(), "hi");
    assert_eq!(snap.dirty, Some(0..2));
    assert_eq!(snap.selections.as_ref(), &[Selection::cursor(2)]);
}

#[test]
fn multi_cursor_insert_merges_dirty_range() {
    // One action over TWO cursors fans into two edits; the snapshot's `dirty`
    // hint must grow to span both (the merge arm), not report only the last
    // edit applied. Reachable only via apply_edit with >1 selection - the path
    // the single-selection message seam cannot exercise until M3 multi-cursor.
    let set = SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
    let mut e = editor_with("abcdef", set);
    let (delta_tx, delta_rx) = async_channel::bounded::<Delta>(16);
    let (snap_tx, snap_rx) = async_channel::bounded::<ViewSnapshot>(1);
    let (note_tx, _note_rx) = async_channel::bounded::<Notification>(16);
    let snapshots = SnapshotSink { tx: snap_tx };

    let edits = e.plan_edit(EditKind::Insert("X".into()));
    let alive = smol::block_on(apply_edit(&mut e, edits, &delta_tx, &snapshots, &note_tx));

    assert!(alive);
    assert_eq!(e.buffer.text().to_string(), "aXbcdXef");
    assert_eq!(delta_rx.len(), 2); // one delta per cursor
    let snap = snap_rx.try_recv().unwrap();
    // Merged hint spans from the earliest edit's start to past the latest's.
    // Endpoints are in base-buffer offsets (a repaint hint, not exact final
    // coords) - painting the whole viewport is always correct if ignored.
    assert_eq!(snap.dirty, Some(1..5));
}

#[test]
fn rejected_edit_is_surfaced_and_leaves_state_unchanged() {
    // Defensive path (SPEC §8): a planned edit whose range does not apply must
    // emit EditRejected and skip, never panic. Not expected in M1 (ranges come
    // from valid selections), so it is only reachable by handing apply_edit a
    // cursor past the buffer end. When EVERY edit is rejected, nothing changed,
    // so the version must NOT advance - a version bump with no delta would
    // diverge a remote frontend replaying the delta stream (SPEC §5 invariant).
    let mut e = editor_with("abc", SelectionSet::single(Selection::cursor(99)));
    let (delta_tx, delta_rx) = async_channel::bounded::<Delta>(16);
    let (snap_tx, _snap_rx) = async_channel::bounded::<ViewSnapshot>(1);
    let (note_tx, note_rx) = async_channel::bounded::<Notification>(16);
    let snapshots = SnapshotSink { tx: snap_tx };

    let edits = e.plan_edit(EditKind::Insert("X".into()));
    let alive = smol::block_on(apply_edit(&mut e, edits, &delta_tx, &snapshots, &note_tx));

    assert!(alive);
    assert_eq!(e.buffer.text().to_string(), "abc"); // untouched
    assert!(delta_rx.is_empty()); // no delta for a rejected edit
    assert_eq!(e.version, 0); // no applied edit => no version bump
    match note_rx.try_recv() {
        Ok(Notification::EditRejected {
            buffer_id, message, ..
        }) => {
            assert_eq!(buffer_id, e.id);
            assert!(message.contains("out of bounds"), "message: {message}");
        }
        other => panic!("expected EditRejected, got {other:?}"),
    }
}

#[test]
fn edit_sets_modified_flag() {
    // The modified axis is independent of version: a fresh buffer is clean; the
    // first applied edit marks it dirty (SPEC §8).
    let mut e = editor_with("abc", SelectionSet::single(Selection::cursor(3)));
    assert!(!e.modified());
    let h = Harness::new();
    let edits = e.plan_edit(EditKind::Insert("d".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(e.modified());
}

#[test]
fn open_existing_file_loads_contents_and_binds_path() {
    let dir = TempDir::new();
    let path = dir.file("hello.txt");
    std::fs::write(&path, "line one\nline two").unwrap();

    let mut e = Editor::new();
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(e.buffer.text().to_string(), "line one\nline two");
    assert_eq!(e.path, Some(path.clone()));
    assert!(!e.modified()); // a freshly opened buffer matches disk
    assert_eq!(e.version, 1); // one whole-buffer delta was emitted
    assert_eq!(e.selections.primary().head, 0); // cursor resets to origin

    // The load is one whole-buffer delta (SPEC §5): replace 0..0 with the file.
    let delta = h.delta_rx.try_recv().unwrap();
    assert_eq!(delta.range, 0..0);
    assert_eq!(delta.new_text, "line one\nline two");

    match h.note_rx.try_recv() {
        Ok(Notification::FileOpened {
            path: p, existed, ..
        }) => {
            assert_eq!(p, path);
            assert!(existed);
        }
        other => panic!("expected FileOpened, got {other:?}"),
    }
}

#[test]
fn open_replaces_existing_buffer_as_one_delta() {
    // Opening over a non-empty buffer replaces its whole contents with a single
    // delta whose range spans the old buffer - so the delta stream still
    // reproduces the snapshot (SPEC §5 invariant).
    let dir = TempDir::new();
    let path = dir.file("replace.txt");
    std::fs::write(&path, "fresh").unwrap();

    let mut e = editor_with("stale contents", SelectionSet::single(Selection::cursor(5)));
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert_eq!(e.buffer.text().to_string(), "fresh");
    let delta = h.delta_rx.try_recv().unwrap();
    assert_eq!(delta.range, 0.."stale contents".len());
    assert_eq!(delta.new_text, "fresh");
}

#[test]
fn open_missing_file_opens_empty_buffer_bound_to_path() {
    // A missing path is not an error (Vim's behavior): empty buffer, path bound,
    // created on save. `existed` is false so the frontend can say "[New File]".
    let dir = TempDir::new();
    let path = dir.file("does-not-exist.txt");

    let mut e = Editor::new();
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert!(e.buffer.text().is_empty());
    assert_eq!(e.path, Some(path.clone()));
    assert!(!e.modified());
    assert_eq!(e.version, 0); // empty->empty: no delta, no version bump
    assert!(h.delta_rx.is_empty());
    // No edit happened, so the repaint hint is None (not a spurious Some(0..0)).
    assert_eq!(h.snapshot().dirty, None);
    match h.note_rx.try_recv() {
        Ok(Notification::FileOpened { existed, .. }) => assert!(!existed),
        other => panic!("expected FileOpened, got {other:?}"),
    }
}

#[test]
fn open_nonempty_file_reports_dirty_hint() {
    // The complementary case: loading actual content emits a delta and the
    // snapshot's repaint hint spans the whole new buffer.
    let dir = TempDir::new();
    let path = dir.file("has-text.txt");
    std::fs::write(&path, "abcde").unwrap();

    let mut e = Editor::new();
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(h.snapshot().dirty, Some(0..5));
}

#[test]
fn open_non_utf8_file_errors_and_leaves_buffer_unchanged() {
    let dir = TempDir::new();
    let path = dir.file("binary.bin");
    std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

    let mut e = editor_with("keep me", SelectionSet::single(Selection::cursor(0)));
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(e.buffer.text().to_string(), "keep me"); // untouched
    assert_eq!(e.path, None); // binding not changed on a failed open
    assert!(h.delta_rx.is_empty());
    match h.note_rx.try_recv() {
        Ok(Notification::FileError {
            message, path: p, ..
        }) => {
            assert_eq!(p, Some(path));
            assert!(message.contains("UTF-8"), "message: {message}");
        }
        other => panic!("expected FileError, got {other:?}"),
    }
}

#[test]
fn save_writes_buffer_to_bound_file_and_clears_modified() {
    let dir = TempDir::new();
    let path = dir.file("out.txt");

    let mut e = editor_with("saved text", SelectionSet::at_origin());
    e.path = Some(path.clone());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "saved text");
    assert!(!e.modified()); // clean after a successful save
    match h.note_rx.try_recv() {
        Ok(Notification::FileSaved { path: p, .. }) => assert_eq!(p, path),
        other => panic!("expected FileSaved, got {other:?}"),
    }
    // No stray temp file left behind by the atomic write (the rename consumed it).
    assert!(!has_temp_file(&dir.path), "leftover .vortex-tmp file");
}

/// Whether any `.<name>.vortex-tmp-*` scratch file remains in `dir`. The atomic
/// write names its temp with a pid+counter suffix, so this scans by prefix rather
/// than guessing the exact name.
fn has_temp_file(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".vortex-tmp-"))
}

#[test]
fn save_without_path_errors_and_keeps_buffer_dirty() {
    // Save with no bound file: surfaced as FileError, buffer stays dirty so no
    // work is lost (SPEC §8). Save-as (a target path) lands with the prompt UI.
    let mut e = editor_with("unsaved", SelectionSet::at_origin());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.modified()); // still dirty
    match h.note_rx.try_recv() {
        Ok(Notification::FileError { path, message, .. }) => {
            assert_eq!(path, None);
            assert!(message.contains("no file name"), "message: {message}");
        }
        other => panic!("expected FileError, got {other:?}"),
    }
}

#[test]
fn save_failure_keeps_buffer_dirty_and_does_not_corrupt_original() {
    // Point the buffer's path at a directory: the atomic write's rename-over
    // fails, so the buffer must stay dirty and the (pre-existing) target is
    // untouched (SPEC §8: a failed save never loses work or corrupts the file).
    let dir = TempDir::new();
    let path = dir.file("a-directory");
    std::fs::create_dir(&path).unwrap();

    let mut e = editor_with("new work", SelectionSet::at_origin());
    e.path = Some(path.clone());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.modified()); // failed save keeps the buffer dirty
    assert!(path.is_dir()); // the target directory is intact, not clobbered
    match h.note_rx.try_recv() {
        Ok(Notification::FileError { path: p, .. }) => assert_eq!(p, Some(path.clone())),
        other => panic!("expected FileError, got {other:?}"),
    }
    // The temp file was cleaned up on the failed rename.
    assert!(!has_temp_file(&dir.path), "leftover .vortex-tmp file");
}

#[test]
fn open_then_edit_then_save_round_trips_through_disk() {
    // End-to-end file lifecycle over the same editor: open a file, edit it, save,
    // and confirm the new contents landed on disk and the buffer is clean.
    let dir = TempDir::new();
    let path = dir.file("round.txt");
    std::fs::write(&path, "abc").unwrap();

    let mut e = Editor::new();
    let h = Harness::new();

    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    // Move to end and append "d".
    e.selections = SelectionSet::single(Selection::cursor(3));
    let edits = e.plan_edit(EditKind::Insert("d".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(e.modified());
    smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(!e.modified());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "abcd");
}

#[test]
fn save_writes_to_a_new_file_that_did_not_exist() {
    // Opening a missing path then saving creates the file (Vim's behavior).
    let dir = TempDir::new();
    let path = dir.file("created-on-save.txt");

    let mut e = Editor::new();
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    e.selections = SelectionSet::at_origin();
    let edits = e.plan_edit(EditKind::Insert("brand new".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(path.exists());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "brand new");
}

#[test]
fn snapshot_carries_path_and_modified() {
    let mut e = editor_with("x", SelectionSet::at_origin());
    e.path = Some(PathBuf::from("/tmp/demo.txt"));
    mark_dirty(&mut e);
    let snap = e.snapshot(None);
    assert_eq!(snap.path, Some(PathBuf::from("/tmp/demo.txt")));
    assert!(snap.modified);
}

#[test]
fn open_unreadable_path_errors_and_leaves_buffer_unchanged() {
    // A path that exists but is not a readable file (a directory) surfaces a
    // FileError via the general read-error arm (not the NotFound arm) and leaves
    // the buffer untouched (SPEC §8).
    let dir = TempDir::new();
    let mut e = editor_with("keep me", SelectionSet::single(Selection::cursor(0)));
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        dir.path.clone(), // the directory itself - read() fails, not NotFound
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(e.buffer.text().to_string(), "keep me"); // untouched
    assert_eq!(e.path, None);
    assert!(matches!(
        h.note_rx.try_recv(),
        Ok(Notification::FileError { .. })
    ));
}

#[test]
fn save_into_missing_directory_errors_and_keeps_buffer_dirty() {
    // The atomic write's temp `File::create` fails when the target's parent
    // directory does not exist: surfaced as FileError, buffer stays dirty, no
    // temp file leaks (covers the write-failure cleanup arm).
    let dir = TempDir::new();
    let path = dir.path.join("no-such-subdir").join("file.txt");
    let mut e = editor_with("work", SelectionSet::at_origin());
    e.path = Some(path.clone());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.modified());
    assert!(matches!(
        h.note_rx.try_recv(),
        Ok(Notification::FileError { .. })
    ));
}

/// Drive the full actor loop (`run`) through the message seam, exactly as a
/// frontend does, and return the final snapshot + any file-lifecycle
/// notification. Exercises the loop's `Open`/`Save` dispatch arms that the
/// direct-function tests above bypass (SPEC §1 headless seam).
fn run_seam(script: &[Action]) -> (ViewSnapshot, Vec<Notification>) {
    let ex = smol::Executor::new();
    let Core { handle, run } = crate::editor::new(16);
    ex.spawn(run).detach();
    smol::block_on(ex.run(async move {
        let mut snap = None;
        for action in script {
            handle.actions.send(action.clone()).await.unwrap();
            while handle.deltas.try_recv().is_ok() {}
            snap = Some(handle.snapshots.recv().await.unwrap());
        }
        // Collect any notifications emitted by the file ops in the script.
        let mut notes = Vec::new();
        while let Ok(n) = handle.notifications.try_recv() {
            notes.push(n);
        }
        (snap.expect("script must have an action"), notes)
    }))
}

#[test]
fn open_with_delta_receiver_dropped_reports_frontend_gone() {
    // Opening a non-empty file emits a whole-buffer delta; if the frontend has
    // dropped the delta receiver, the send fails and open_file returns false
    // ("frontend gone"), so the actor loop can stop cleanly.
    let dir = TempDir::new();
    let path = dir.file("has-content.txt");
    std::fs::write(&path, "content").unwrap();

    let mut e = Editor::new();
    let h = Harness::new();
    drop(h.delta_rx); // frontend hung up the lossless delta channel
    let alive = smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(!alive);
}

#[test]
fn place_cursor_through_the_actor_loop() {
    // End-to-end through the real loop: type text, then a click places the caret
    // mid-buffer and a shift/drag extends the selection - no version bump, since
    // placing the caret changes no text.
    let (snap, _) = run_seam(&[
        Action::Insert("hello".into()),
        Action::PlaceCursor {
            offset: 1,
            extend: false,
        },
        Action::PlaceCursor {
            offset: 4,
            extend: true,
        },
    ]);
    assert_eq!(snap.selections.as_ref(), &[Selection::new(1, 4)]);
    assert_eq!(snap.primary, 0);
    assert_eq!(snap.version, 1); // only the Insert bumped the version
}

#[test]
fn open_then_save_through_the_actor_loop() {
    // End-to-end through the real actor loop: Open binds the path and loads the
    // file; an Insert dirties it; Save writes it back and clears modified.
    let dir = TempDir::new();
    let path = dir.file("seam.txt");
    std::fs::write(&path, "abc").unwrap();

    let (snap, notes) = run_seam(&[
        Action::Open(path.clone()),
        Action::Insert("Z".into()),
        Action::Save,
    ]);

    assert_eq!(snap.path, Some(path.clone()));
    assert!(!snap.modified); // clean after the save
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Zabc");
    // The loop emitted both a FileOpened and a FileSaved for this path.
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::FileOpened { .. }))
    );
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::FileSaved { .. }))
    );
}

#[test]
fn multi_cursor_undo_restores_every_cursor() {
    // One Insert over two cursors is one undo unit (SPEC §2.4); undoing it must
    // remove both inserted spans at their shifted offsets, not just one. Reachable
    // only via apply_edit + reapply with >1 selection - the multi-cursor path the
    // single-selection message seam cannot yet drive.
    let set = SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
    let mut e = editor_with("abcdef", set);
    let h = Harness::new();

    let edits = e.plan_edit(EditKind::Insert("X".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(e.buffer.text().to_string(), "aXbcdXef");

    let reverted = e.history.undo();
    let alive = smol::block_on(reapply(
        &mut e,
        reverted,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(alive);
    assert_eq!(
        e.buffer.text().to_string(),
        "abcdef",
        "both cursors' inserts undone"
    );
    // Selections restored to the two original carets.
    assert_eq!(
        e.selections.all(),
        &[Selection::cursor(1), Selection::cursor(4)]
    );
}

#[test]
fn add_cursor_below_then_type_edits_every_line_through_the_loop() {
    // The full multi-cursor path, now reachable through the message seam: type two
    // lines, go to the start, add a cursor below, then type - both cursors insert as
    // one action. Previously unreachable (no Action added a second cursor).
    let (snap, _notes) = run_seam(&[
        Action::Insert("ab\ncd".into()),
        Action::MoveCursor {
            motion: Motion::BufferStart,
            extend: false,
        },
        Action::AddCursorBelow,
        Action::Insert("X".into()),
    ]);
    assert_eq!(snap.text.to_string(), "Xab\nXcd");
    // Two carets survive the edit, each just past its own inserted "X".
    let heads: Vec<usize> = snap.selections.iter().map(|s| s.head).collect();
    assert_eq!(heads, vec![1, 5]);
    // AddCursorBelow made the lower caret primary; it stays primary across the edit
    // (index 1, head 5) rather than snapping back to the topmost caret.
    assert_eq!(
        snap.primary, 1,
        "the primary cursor is carried across the edit"
    );
}

#[test]
fn add_cursor_above_then_type_edits_every_line_through_the_loop() {
    // Mirror of the AddCursorBelow path: type two lines, sit on the lower one, add a
    // caret above, then type - both lines get the insert as one action. Covers the
    // `above = true` branch of add_cursor_vertical and the AddCursorAbove dispatch arm.
    let (snap, _notes) = run_seam(&[
        Action::Insert("ab\ncd".into()),
        Action::MoveCursor {
            motion: Motion::BufferEnd,
            extend: false,
        },
        Action::AddCursorAbove,
        Action::Insert("X".into()),
    ]);
    assert_eq!(snap.text.to_string(), "abX\ncdX");
    let heads: Vec<usize> = snap.selections.iter().map(|s| s.head).collect();
    assert_eq!(heads, vec![3, 7]);
    // AddCursorAbove made the upper caret primary; it stays primary across the edit
    // (index 0, head 3) rather than snapping to the originating lower caret.
    assert_eq!(
        snap.primary, 0,
        "the primary cursor is carried across the edit"
    );
}

#[test]
fn one_multi_cursor_insert_is_a_single_undo_unit_through_the_loop() {
    // SPEC §2.4: one keystroke over N cursors is ONE undo entry. Build two cursors,
    // type over both, then a single Undo restores the pre-edit text and both carets.
    let (snap, _notes) = run_seam(&[
        Action::Insert("ab\ncd".into()),
        Action::MoveCursor {
            motion: Motion::BufferStart,
            extend: false,
        },
        Action::AddCursorBelow, // cursors at 0 and 3
        Action::Insert("X".into()),
        Action::Undo,
    ]);
    assert_eq!(
        snap.text.to_string(),
        "ab\ncd",
        "one undo reverts both inserts"
    );
    let heads: Vec<usize> = snap.selections.iter().map(|s| s.head).collect();
    assert_eq!(heads, vec![0, 3], "both carets restored");
}

#[test]
fn a_motion_between_keystrokes_splits_the_undo_run_through_the_loop() {
    // SPEC §2.4 break rule (d), end to end: type "ab", move the caret, type "X".
    // The first Undo must peel only "X" - if the run had swallowed it, undo would
    // jump straight back to the empty buffer and eat work the user expected to keep.
    // This is the integration guard for the rule being structural: no action arm
    // announces the break, `History` infers it from the edit's own selections.
    let (snap, _notes) = run_seam(&[
        Action::Insert("a".into()),
        Action::Insert("b".into()), // coalesces with "a"
        Action::MoveCursor {
            motion: Motion::Left,
            extend: false,
        },
        Action::Insert("X".into()), // starts from a different caret -> new unit
        Action::Undo,
    ]);
    assert_eq!(snap.text.to_string(), "ab", "only the post-motion insert");

    // A second Undo then removes the whole coalesced "ab" run at once.
    let (snap, _notes) = run_seam(&[
        Action::Insert("a".into()),
        Action::Insert("b".into()),
        Action::MoveCursor {
            motion: Motion::Left,
            extend: false,
        },
        Action::Insert("X".into()),
        Action::Undo,
        Action::Undo,
    ]);
    assert_eq!(snap.text.to_string(), "");
}

#[test]
fn adding_a_cursor_between_keystrokes_splits_the_undo_run_through_the_loop() {
    // The cursor-set half of break rule (d): growing the selection set between two
    // keystrokes ends the run just as a motion does, with no per-action break call.
    let (snap, _notes) = run_seam(&[
        Action::Insert("ab\ncd".into()),
        Action::MoveCursor {
            motion: Motion::BufferStart,
            extend: false,
        },
        Action::Insert("X".into()), // one caret
        Action::AddCursorBelow,     // cursor set changes
        Action::Insert("Y".into()), // must not fold into the "X" unit
        Action::Undo,
    ]);
    assert_eq!(
        snap.text.to_string(),
        "Xab\ncd",
        "undo peels only the multi-cursor insert, leaving the earlier X"
    );
}

#[test]
fn a_round_trip_motion_between_keystrokes_keeps_one_undo_unit() {
    // The one behavior change from making break rule (d) structural: moving away
    // and back leaves the selection set exactly as it was, which is indistinguishable
    // from never having moved, so the typing run survives. Previously every motion
    // announced a break, so this split into two undo units. Asserted end to end so
    // the equivalence is a checked decision rather than an undocumented side effect.
    let (snap, _notes) = run_seam(&[
        Action::Insert("a".into()),
        Action::MoveCursor {
            motion: Motion::Left,
            extend: false,
        },
        Action::MoveCursor {
            motion: Motion::Right,
            extend: false,
        },
        Action::Insert("b".into()),
        Action::Undo,
    ]);
    assert_eq!(
        snap.text.to_string(),
        "",
        "one undo removes both characters"
    );
}

#[test]
fn consecutive_keystrokes_still_coalesce_through_the_loop() {
    // The other side of the same mechanism: with no selection change between them,
    // a run of typed characters is still ONE undo unit (without this, undo reverts
    // one keystroke at a time - unusable, SPEC §2.4).
    let (snap, _notes) = run_seam(&[
        Action::Insert("a".into()),
        Action::Insert("b".into()),
        Action::Insert("c".into()),
        Action::Undo,
    ]);
    assert_eq!(snap.text.to_string(), "", "one undo removes the whole run");
}

#[test]
fn collapse_selections_reduces_to_the_primary_through_the_loop() {
    let (snap, _notes) = run_seam(&[
        Action::Insert("ab\ncd\nef".into()),
        Action::MoveCursor {
            motion: Motion::BufferStart,
            extend: false,
        },
        Action::AddCursorBelow,
        Action::AddCursorBelow, // three cursors
        Action::CollapseSelections,
    ]);
    assert_eq!(snap.selections.len(), 1, "collapsed to a single selection");
}

#[test]
fn add_cursor_at_offset_through_the_loop_keeps_both_cursors() {
    // A modifier-click adds a cursor without collapsing the set (unlike PlaceCursor).
    let (snap, _notes) = run_seam(&[
        Action::Insert("abcdef".into()),
        Action::PlaceCursor {
            offset: 1,
            extend: false,
        },
        Action::AddCursorAt { offset: 4 },
    ]);
    let heads: Vec<usize> = snap.selections.iter().map(|s| s.head).collect();
    assert_eq!(heads, vec![1, 4]);
    assert_eq!(snap.version, 1, "adding cursors changes no text");
}

#[test]
fn undo_reports_frontend_gone_when_the_delta_channel_is_closed() {
    // Undo emits a delta (it is an edit on the wire); if the frontend dropped the
    // lossless delta receiver, the send fails and `reapply` returns false so the
    // actor loop can stop cleanly - the same contract as an ordinary edit.
    let mut e = editor_with("abc", SelectionSet::single(Selection::cursor(3)));
    let h = Harness::new();
    // Record an edit so there is something to undo.
    let edits = e.plan_edit(EditKind::Insert("d".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    drop(h.delta_rx); // frontend hangs up the delta channel

    let reverted = e.history.undo();
    let alive = smol::block_on(reapply(
        &mut e,
        reverted,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(!alive);
}

#[test]
fn undo_back_to_the_saved_state_clears_modified_through_the_loop() {
    // Save point tracking (SPEC §8): after saving, edit again (dirty), then undo
    // back to the saved node - the buffer is clean again even though the version
    // kept advancing. Driven end-to-end through the real actor loop.
    let dir = TempDir::new();
    let path = dir.file("savepoint.txt");
    std::fs::write(&path, "").unwrap();

    let (snap, _notes) = run_seam(&[
        Action::Open(path.clone()),
        Action::Insert("x".into()),
        Action::Save,               // saved state = "x"
        Action::Insert("y".into()), // dirty: "xy"
        Action::Undo,               // back to the saved node
    ]);
    assert_eq!(snap.text.to_string(), "x");
    assert!(
        !snap.modified,
        "undo to the saved state clears the modified marker"
    );
}

#[test]
fn open_resets_undo_history() {
    // Undo does not cross a file open (SPEC §2.4): after opening, there is nothing
    // from before the load to undo. Type, open a file, then Undo - the buffer holds
    // the file's content unchanged (the pre-open edit is not on this history).
    let dir = TempDir::new();
    let path = dir.file("reset.txt");
    std::fs::write(&path, "loaded").unwrap();

    let (snap, _notes) = run_seam(&[
        Action::Insert("scratch".into()),
        Action::Open(path.clone()),
        Action::Undo,
    ]);
    assert_eq!(
        snap.text.to_string(),
        "loaded",
        "undo cannot reach across the open"
    );
}

// Atomic-write hardening (SPEC §8). These are Unix-specific because they assert
// on permission bits and symlink semantics that Windows models differently.
#[cfg(unix)]
mod atomic_write {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn save_preserves_existing_file_permissions() {
        // A restrictive mode (0o600) must survive a save: the temp+rename must not
        // reset it to File::create's default 0o644, which would silently widen a
        // private file to world-readable.
        let dir = TempDir::new();
        let path = dir.file("private.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_atomic(&path, b"new contents").expect("save succeeds");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new contents");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "mode should be preserved, got {mode:o}"
        );
    }

    #[test]
    fn save_preserves_executable_bit() {
        let dir = TempDir::new();
        let path = dir.file("script.sh");
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        write_atomic(&path, b"#!/bin/sh\necho hi\n").expect("save succeeds");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "executable bits should survive");
    }

    #[test]
    fn save_writes_through_a_symlink_instead_of_replacing_it() {
        // A symlinked file (real dotfile setup: ~/.vimrc -> dotfiles/vimrc) must
        // stay a symlink after save, with the *target* updated - not be replaced
        // by a standalone regular file that detaches the link.
        let dir = TempDir::new();
        let real = dir.file("real.txt");
        let link = dir.file("link.txt");
        std::fs::write(&real, "before").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, b"after").expect("save succeeds");

        // The link is still a link, and the real file behind it got the update.
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link.txt should still be a symlink"
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "after");
    }

    #[test]
    fn save_through_a_looping_symlink_surfaces_the_error() {
        // A self-referential symlink (link -> link) lets `symlink_metadata` succeed
        // (it does not follow the link) but makes `canonicalize` fail with a loop
        // error - NOT NotFound, so write_atomic cannot resolve it by hand and must
        // surface the error instead of hanging or panicking.
        let dir = TempDir::new();
        let link = dir.file("loop.txt");
        std::os::unix::fs::symlink(&link, &link).unwrap();

        let err =
            write_atomic(&link, b"never lands").expect_err("a symlink loop cannot be resolved");
        assert!(!err.is_empty(), "the underlying error is surfaced: {err}");
        assert!(!has_temp_file(&dir.path), "no temp file should leak");
    }

    #[test]
    fn save_creates_a_new_file_with_default_mode() {
        // A brand-new file has no existing mode to copy; it just uses the default.
        let dir = TempDir::new();
        let path = dir.file("brand-new.txt");
        write_atomic(&path, b"hello").expect("save succeeds");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!has_temp_file(&dir.path));
    }

    #[test]
    fn concurrent_saves_to_same_file_do_not_collide_on_temp_name() {
        // Two saves of the same target use distinct temp names (pid + counter), so
        // one never truncates the other's in-flight temp. Sequential here (the temp
        // name is unique per call regardless of timing); the assertion is that both
        // succeed and no temp leaks.
        let dir = TempDir::new();
        let path = dir.file("shared.txt");
        std::fs::write(&path, "seed").unwrap();

        write_atomic(&path, b"first").expect("first save");
        write_atomic(&path, b"second").expect("second save");

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!has_temp_file(&dir.path), "no temp file should leak");
    }

    #[test]
    fn save_through_a_dangling_symlink_creates_its_target() {
        // A symlink whose target does not exist yet (a fresh dotfile: link -> real,
        // real not created). canonicalize fails NotFound on it, so write_atomic must
        // resolve the link by hand and write *through* it, creating the target while
        // leaving the link a link - the way vim handles a first save of ~/.vimrc.
        let dir = TempDir::new();
        let real = dir.file("real.txt"); // does not exist yet
        let link = dir.file("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_atomic(&link, b"first write").expect("save through dangling link succeeds");

        assert_eq!(std::fs::read_to_string(&real).unwrap(), "first write");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link should remain a symlink pointing at the created target"
        );
        assert!(!has_temp_file(&dir.path), "no temp file should leak");
    }

    #[test]
    fn save_through_a_dangling_relative_symlink_resolves_against_the_link_dir() {
        // A *relative* dangling link (`link -> real.txt`, the common dotfile shape)
        // resolves its target against the link's own directory, not the process cwd,
        // so the created file lands next to the link.
        let dir = TempDir::new();
        let link = dir.file("link.txt");
        // Relative target: `read_link` returns "real.txt", joined with the link's dir.
        std::os::unix::fs::symlink(Path::new("real.txt"), &link).unwrap();

        write_atomic(&link, b"relative write").expect("save through relative link succeeds");

        assert_eq!(
            std::fs::read_to_string(dir.file("real.txt")).unwrap(),
            "relative write",
            "target should be created beside the link"
        );
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "link should remain a symlink"
        );
        assert!(!has_temp_file(&dir.path), "no temp file should leak");
    }

    #[test]
    fn save_never_exposes_a_private_file_in_a_world_readable_temp() {
        // A 0600 target's contents must never touch disk in a wider-mode temp, even
        // for the write+fsync window (that window would expose e.g. an SSH key to any
        // local user). A watcher thread records the widest mode any temp shows; a
        // group/other-accessible temp fails the test. The watcher can only *tighten*
        // the assertion, so correct code never flakes - at worst a very fast machine
        // misses the window (a false negative, never a false positive).
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU32};

        let dir = TempDir::new();
        let path = dir.file("private.txt");
        std::fs::write(&path, "seed").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let widest = Arc::new(AtomicU32::new(0));
        let watch_dir = dir.path.clone();
        let (w_done, w_widest) = (Arc::clone(&done), Arc::clone(&widest));
        let watcher = std::thread::spawn(move || {
            while !w_done.load(Ordering::Relaxed) {
                if let Ok(entries) = std::fs::read_dir(&watch_dir) {
                    for e in entries.flatten() {
                        if e.file_name().to_string_lossy().contains(".vortex-tmp-")
                            && let Ok(meta) = e.metadata()
                        {
                            w_widest
                                .fetch_max(meta.permissions().mode() & 0o777, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // A multi-megabyte payload widens the write+fsync window enough for the
        // watcher to observe the temp before it is renamed.
        let big = vec![b'x'; 8 * 1024 * 1024];
        write_atomic(&path, &big).expect("save succeeds");
        done.store(true, Ordering::Relaxed);
        watcher.join().unwrap();

        let seen = widest.load(Ordering::Relaxed);
        assert_eq!(
            seen & 0o077,
            0,
            "temp must never be group/other-accessible; saw mode {seen:o}"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "final file keeps its private mode"
        );
    }

    // --- Clipboard register (SPEC §11) -------------------------------------

    #[test]
    fn fill_register_copies_each_nonempty_selection_in_order() {
        // Two selections over "abcdef": [0,2) = "ab", [4,6) = "ef". The register
        // gets one entry per selection in the set's sorted (top-to-bottom) order.
        // The spans are already sorted and disjoint (2 < 4), so they survive
        // normalization as two separate selections.
        let set =
            SelectionSet::from_sorted_cursors(vec![Selection::new(0, 2), Selection::new(4, 6)]);
        let mut e = editor_with("abcdef", set);
        assert!(e.fill_register());
        assert_eq!(e.register, vec!["ab".to_string(), "ef".to_string()]);
    }

    #[test]
    fn fill_register_is_noop_for_bare_cursors() {
        // Nothing selected: the register is left untouched and the caller is told
        // not to emit a clipboard notification.
        let mut e = editor_with("abc", SelectionSet::single(Selection::cursor(1)));
        e.register = vec!["previous".into()];
        assert!(!e.fill_register());
        assert_eq!(e.register, vec!["previous".to_string()]); // unchanged
    }

    #[test]
    fn register_flattened_joins_entries_with_newline() {
        let mut e = editor_with("", SelectionSet::at_origin());
        e.register = vec!["one".into(), "two".into(), "three".into()];
        assert_eq!(e.register_flattened(), "one\ntwo\nthree");
    }

    #[test]
    fn plan_paste_single_entry_splats_to_every_cursor() {
        // One register entry goes to all cursors (the common single-copy paste).
        let set =
            SelectionSet::from_sorted_cursors(vec![Selection::cursor(0), Selection::cursor(2)]);
        let mut e = editor_with("ab", set);
        e.register = vec!["X".into()];
        let edits = e.plan_paste();
        // Descending by start; both cursors get "X".
        assert_eq!(edits, vec![(2..2, "X".into()), (0..0, "X".into())]);
    }

    #[test]
    fn plan_paste_matched_counts_distribute_per_cursor() {
        // Register length == cursor count: the i-th entry lands at the i-th cursor
        // (the multi-cursor copy/paste round-trip).
        let set =
            SelectionSet::from_sorted_cursors(vec![Selection::cursor(0), Selection::cursor(2)]);
        let mut e = editor_with("ab", set);
        e.register = vec!["P".into(), "Q".into()];
        let edits = e.plan_paste();
        // Descending by start: cursor 1 (start 2) -> "Q", cursor 0 (start 0) -> "P".
        assert_eq!(edits, vec![(2..2, "Q".into()), (0..0, "P".into())]);
    }

    #[test]
    fn plan_paste_mismatched_counts_join_with_newline() {
        // Three entries, two cursors: neither 1 nor equal, so every cursor gets the
        // whole register joined with newlines (the leftover policy).
        let set =
            SelectionSet::from_sorted_cursors(vec![Selection::cursor(0), Selection::cursor(2)]);
        let mut e = editor_with("ab", set);
        e.register = vec!["a".into(), "b".into(), "c".into()];
        let edits = e.plan_paste();
        assert_eq!(
            edits,
            vec![(2..2, "a\nb\nc".into()), (0..0, "a\nb\nc".into())]
        );
    }

    #[test]
    fn plan_paste_empty_register_is_noop() {
        let e = editor_with("ab", SelectionSet::single(Selection::cursor(1)));
        assert!(e.plan_paste().is_empty());
    }

    #[test]
    fn plan_paste_replaces_a_nonempty_selection() {
        // Paste over a selection replaces it (the range is the selection span, not a
        // zero-width insert), mirroring Insert's replace-then-insert.
        let set = SelectionSet::single(Selection::new(0, 3)); // "abc" selected
        let mut e = editor_with("abcdef", set);
        e.register = vec!["Z".into()];
        assert_eq!(e.plan_paste(), vec![(0..3, "Z".into())]);
    }

    #[test]
    fn delete_selection_editkind_skips_bare_cursors() {
        // The cut edit deletes only non-empty selections; a bare cursor contributes
        // nothing (unlike backspace/delete, which step a grapheme at a cursor).
        let cursor = editor_with("abc", SelectionSet::single(Selection::cursor(1)));
        assert!(cursor.plan_edit(EditKind::DeleteSelection).is_empty());

        let selected = editor_with("abc", SelectionSet::single(Selection::new(0, 2)));
        assert_eq!(
            selected.plan_edit(EditKind::DeleteSelection),
            vec![(0..2, String::new())]
        );
    }

    #[test]
    fn copy_then_paste_round_trips_through_the_register() {
        // End-to-end register path: select "ab", copy (fills register + flattens for
        // the clipboard mirror), collapse to a caret at end, then paste it back.
        let mut e = editor_with("abcdef", SelectionSet::single(Selection::new(0, 2)));
        assert!(e.fill_register());
        assert_eq!(e.register_flattened(), "ab");

        // Caret at end of buffer, paste the register there.
        e.selections = SelectionSet::single(Selection::cursor(6));
        let edits = e.plan_paste();
        let h = Harness::new();
        smol::block_on(apply_edit(
            &mut e,
            edits,
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        ));
        assert_eq!(e.buffer.text().to_string(), "abcdefab");
        assert_eq!(e.version, 1);
    }

    #[test]
    fn cut_is_one_edit_that_deletes_the_selection() {
        // Cut fills the register then applies DeleteSelection as one edit: the
        // selected text is removed and one undo unit recorded (version bumps once).
        let mut e = editor_with("abcdef", SelectionSet::single(Selection::new(2, 4)));
        assert!(e.fill_register());
        assert_eq!(e.register, vec!["cd".to_string()]);

        let edits = e.plan_edit(EditKind::DeleteSelection);
        let h = Harness::new();
        smol::block_on(apply_edit(
            &mut e,
            edits,
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        ));
        assert_eq!(e.buffer.text().to_string(), "abef");
        assert_eq!(e.version, 1);
        assert_eq!(h.delta_rx.len(), 1); // one delta for the single deletion
    }
}

// --- LSP language identifiers (SPEC §3, M2) ---

#[test]
fn language_id_maps_extensions_to_lsp_identifiers() {
    // The LSP `languageId` is the protocol's own vocabulary, not the file
    // extension - a server keyed on "rust" ignores a document announced as "rs".
    for (file, expected) in [
        ("a.rs", "rust"),
        ("a.js", "javascript"),
        ("a.mjs", "javascript"),
        ("a.cjs", "javascript"),
        ("a.ts", "typescript"),
        ("a.tsx", "typescriptreact"),
        ("a.jsx", "javascriptreact"),
        ("a.py", "python"),
        ("a.go", "go"),
        ("a.c", "c"),
        ("a.h", "c"),
        ("a.cc", "cpp"),
        ("a.cpp", "cpp"),
        ("a.hpp", "cpp"),
        ("a.cxx", "cpp"),
        ("a.md", "markdown"),
    ] {
        assert_eq!(language_id(Path::new(file)), expected, "for {file}");
    }
}

#[test]
fn an_unknown_extension_falls_back_to_the_extension_itself() {
    // Guessing costs nothing (a server ignores documents it does not claim),
    // while refusing to guess would mean no server ever sees a file type this
    // list has not been taught.
    assert_eq!(language_id(Path::new("a.zig")), "zig");
    // A file with no extension has no identifier to offer.
    assert_eq!(language_id(Path::new("Makefile")), "");
}
