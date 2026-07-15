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
    // Held only to keep the snapshot channel open: `publish` returns false (and
    // the file ops bail as "frontend gone") if the receiver is dropped. Tests
    // assert on state and notifications, not this, hence the underscore.
    _snap_rx: Receiver<ViewSnapshot>,
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
            _snap_rx: snap_rx,
            note_tx,
            note_rx,
        }
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

    let alive = smol::block_on(apply_edit(
        &mut e,
        EditKind::Insert("X".into()),
        &delta_tx,
        &snapshots,
        &note_tx,
    ));

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

    let alive = smol::block_on(apply_edit(
        &mut e,
        EditKind::Insert("X".into()),
        &delta_tx,
        &snapshots,
        &note_tx,
    ));

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
    assert!(!e.modified);
    let h = Harness::new();
    smol::block_on(apply_edit(
        &mut e,
        EditKind::Insert("d".into()),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(e.modified);
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
    assert!(!e.modified); // a freshly opened buffer matches disk
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
    assert!(!e.modified);
    assert_eq!(e.version, 0); // empty->empty: no delta, no version bump
    assert!(h.delta_rx.is_empty());
    match h.note_rx.try_recv() {
        Ok(Notification::FileOpened { existed, .. }) => assert!(!existed),
        other => panic!("expected FileOpened, got {other:?}"),
    }
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
    e.modified = true;

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "saved text");
    assert!(!e.modified); // clean after a successful save
    match h.note_rx.try_recv() {
        Ok(Notification::FileSaved { path: p, .. }) => assert_eq!(p, path),
        other => panic!("expected FileSaved, got {other:?}"),
    }
    // No stray temp file left behind by the atomic write.
    assert!(!dir.file(".out.txt.vortex-tmp").exists());
}

#[test]
fn save_without_path_errors_and_keeps_buffer_dirty() {
    // Save with no bound file: surfaced as FileError, buffer stays dirty so no
    // work is lost (SPEC §8). Save-as (a target path) lands with the prompt UI.
    let mut e = editor_with("unsaved", SelectionSet::at_origin());
    e.modified = true;

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.modified); // still dirty
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
    e.modified = true;

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.modified); // failed save keeps the buffer dirty
    assert!(path.is_dir()); // the target directory is intact, not clobbered
    match h.note_rx.try_recv() {
        Ok(Notification::FileError { path: p, .. }) => assert_eq!(p, Some(path.clone())),
        other => panic!("expected FileError, got {other:?}"),
    }
    // The temp file was cleaned up on the failed rename.
    assert!(!dir.file(".a-directory.vortex-tmp").exists());
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
    smol::block_on(apply_edit(
        &mut e,
        EditKind::Insert("d".into()),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(e.modified);
    smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(!e.modified);
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
    smol::block_on(apply_edit(
        &mut e,
        EditKind::Insert("brand new".into()),
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
    e.modified = true;
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
    e.modified = true;

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.modified);
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
