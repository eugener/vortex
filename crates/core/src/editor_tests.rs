use super::*;
use crate::buffer::Position;
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

/// A session holding one document with `text`/`selections`. The file and edit
/// paths take the whole session (a snapshot names the active buffer *and* lists
/// the others), so this is what nearly every test drives.
fn session_with(text: &str, selections: SelectionSet) -> Session {
    let mut s = Session::new();
    let doc = s.active_mut();
    doc.buffer = RopeBuffer::from(text);
    doc.selections = selections;
    s
}

/// Put the active document in the "modified" state by recording a dummy revision,
/// moving history off its saved node - the same state a real edit leaves behind
/// (`modified` is derived from the history, not stored).
fn mark_dirty(e: &mut Session) {
    let doc = e.active_mut();
    let selections = doc.selections.clone();
    doc.history.record(
        vec![Change {
            start: 0,
            removed: String::new(),
            inserted: "x".into(),
        }],
        selections.clone(),
        selections,
    );
}

#[test]
fn plan_insert_over_two_cursors_is_descending() {
    // Two cursors; an insert plans one edit each, sorted descending by start
    // so back-to-front application keeps offsets stable.
    let set = SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
    let e = session_with("abcdef", set);
    let edits = e.active().plan_edit(EditKind::Insert("X".into()));
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
    let set = selections_after_edits(&before, &edits_from_changes(&changes));
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
    let set = selections_after_edits(&before, &edits_from_changes(&changes));
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
    let e = session_with("abcdef", set);
    let edits = e.active().plan_edit(EditKind::DeleteBackward);
    // Each cursor deletes the grapheme before it: ranges 4..5 and 1..2.
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[0].0, 4..5);
    assert_eq!(edits[1].0, 1..2);
}

#[test]
fn move_cursor_helper_maps_over_buffer() {
    let mut e = session_with("hello", SelectionSet::at_origin());
    e.active_mut().move_cursor(Motion::Right, false);
    assert_eq!(e.active().selections.primary().head, 1);
}

#[test]
fn place_cursor_helper_sets_and_extends_caret() {
    let mut e = session_with("hello", SelectionSet::at_origin());
    // A plain click places a cursor at the offset.
    e.active_mut().place_cursor(3, false);
    assert_eq!(*e.active().selections.primary(), Selection::cursor(3));
    // A drag/extend keeps the anchor and moves only the head.
    e.active_mut().place_cursor(5, true);
    assert_eq!(*e.active().selections.primary(), Selection::new(3, 5));
}

#[test]
fn snapshot_reflects_state() {
    let mut e = session_with("hi", SelectionSet::single(Selection::cursor(2)));
    let snap = e.snapshot(Some(0..2));
    assert_eq!(snap.text.to_string(), "hi");
    assert_eq!(snap.dirty, Some(0..2));
    assert_eq!(snap.selections.as_ref(), &[Selection::cursor(2)]);
    // The snapshot names the active buffer and lists every open one (SPEC §5).
    assert_eq!(snap.buffers.len(), 1);
    assert_eq!(snap.buffers[0].id, snap.buffer_id);
}

#[test]
fn multi_cursor_insert_merges_dirty_range() {
    // One action over TWO cursors fans into two edits; the snapshot's `dirty`
    // hint must grow to span both (the merge arm), not report only the last
    // edit applied. Reachable only via apply_edit with >1 selection - the path
    // the single-selection message seam cannot exercise until M3 multi-cursor.
    let set = SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
    let mut e = session_with("abcdef", set);
    let (delta_tx, delta_rx) = async_channel::bounded::<Delta>(16);
    let (snap_tx, snap_rx) = async_channel::bounded::<ViewSnapshot>(1);
    let (note_tx, _note_rx) = async_channel::bounded::<Notification>(16);
    let snapshots = SnapshotSink { tx: snap_tx };

    let edits = e.active().plan_edit(EditKind::Insert("X".into()));
    let alive = smol::block_on(apply_edit(&mut e, edits, &delta_tx, &snapshots, &note_tx));

    assert!(alive);
    assert_eq!(e.active().buffer.text().to_string(), "aXbcdXef");
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
    let mut e = session_with("abc", SelectionSet::single(Selection::cursor(99)));
    let (delta_tx, delta_rx) = async_channel::bounded::<Delta>(16);
    let (snap_tx, _snap_rx) = async_channel::bounded::<ViewSnapshot>(1);
    let (note_tx, note_rx) = async_channel::bounded::<Notification>(16);
    let snapshots = SnapshotSink { tx: snap_tx };

    let edits = e.active().plan_edit(EditKind::Insert("X".into()));
    let alive = smol::block_on(apply_edit(&mut e, edits, &delta_tx, &snapshots, &note_tx));

    assert!(alive);
    assert_eq!(e.active().buffer.text().to_string(), "abc"); // untouched
    assert!(delta_rx.is_empty()); // no delta for a rejected edit
    assert_eq!(e.active().version, 0); // no applied edit => no version bump
    match note_rx.try_recv() {
        Ok(Notification::EditRejected {
            buffer_id, message, ..
        }) => {
            assert_eq!(buffer_id, e.active().id);
            assert!(message.contains("out of bounds"), "message: {message}");
        }
        other => panic!("expected EditRejected, got {other:?}"),
    }
}

#[test]
fn edit_sets_modified_flag() {
    // The modified axis is independent of version: a fresh buffer is clean; the
    // first applied edit marks it dirty (SPEC §8).
    let mut e = session_with("abc", SelectionSet::single(Selection::cursor(3)));
    assert!(!e.active().modified());
    let h = Harness::new();
    let edits = e.active().plan_edit(EditKind::Insert("d".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(e.active().modified());
}

#[test]
fn open_existing_file_loads_contents_and_binds_path() {
    let dir = TempDir::new();
    let path = dir.file("hello.txt");
    std::fs::write(&path, "line one\nline two").unwrap();

    let mut e = Session::new();
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(e.active().buffer.text().to_string(), "line one\nline two");
    assert_eq!(e.active().path, Some(path.clone()));
    assert!(!e.active().modified()); // a freshly opened buffer matches disk
    assert_eq!(e.active().version, 1); // one whole-buffer delta was emitted
    assert_eq!(e.active().selections.primary().head, 0); // cursor resets to origin

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
fn open_beside_a_used_buffer_creates_a_second_one() {
    // The active buffer holds work, so an open must not clobber it: a new buffer is
    // created and focused, and the original is still there to switch back to. The
    // load is one whole-buffer delta naming the *new* buffer, so the delta stream
    // still reproduces the snapshot (SPEC §5 invariant).
    let dir = TempDir::new();
    let path = dir.file("second.txt");
    std::fs::write(&path, "fresh").unwrap();

    let mut e = session_with("existing work", SelectionSet::single(Selection::cursor(5)));
    let first = e.active().id;
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert_eq!(e.docs.len(), 2);
    assert_eq!(e.active().buffer.text().to_string(), "fresh");
    assert_ne!(e.active().id, first);
    // The original buffer is untouched, contents and all.
    assert_eq!(e.docs[0].buffer.text().to_string(), "existing work");

    let delta = h.delta_rx.try_recv().unwrap();
    assert_eq!(delta.buffer_id, e.active().id);
    assert_eq!(delta.range, 0..0); // the new buffer started empty
    assert_eq!(delta.new_text, "fresh");
}

#[test]
fn opening_the_same_file_by_a_different_spelling_switches_to_it() {
    // REGRESSION (code review, M7): the already-open check compared raw `PathBuf`s,
    // so the two ways a path reaches the core - argv passes what the user typed, the
    // file picker always sends an absolute path - never matched each other. Launching
    // on `notes.txt` and then picking that same file opened a *second* buffer over
    // it, with its own history, and whichever was saved last discarded the other's
    // edits. Exactly the hazard `Action::Open`'s contract promises to prevent.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "shared").unwrap();
    let h = Harness::new();
    let mut e = Session::new();

    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    let first = e.active().id;

    // The same file reached by a different spelling. A `..` hop is used because
    // `PathBuf` equality already folds away `.` components but cannot fold `..`
    // without consulting the filesystem - which is precisely the gap canonicalizing
    // closes. (The case that bites in practice is argv's relative path against the
    // picker's absolute one; that needs a process-wide cwd change, so it is not
    // hermetic enough for a test and is covered by the same code path here.)
    std::fs::create_dir(dir.file("sub")).unwrap();
    let indirect = dir.file("sub").join("..").join("notes.txt");
    assert_ne!(indirect, path, "the premise: a different spelling");
    smol::block_on(open_file(
        &mut e,
        indirect,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert_eq!(e.docs.len(), 1, "one file must mean one buffer");
    assert_eq!(e.active().id, first, "and it is the buffer already open");
}

#[test]
fn saving_as_a_file_another_buffer_holds_is_refused() {
    // The same duplicate-buffer hazard from the other direction: adopting a path a
    // second buffer already owns would leave both claiming one file. Refused before
    // the write, so the target on disk is untouched (SPEC §8).
    let dir = TempDir::new();
    let (first, second) = (dir.file("a.txt"), dir.file("b.txt"));
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&second, "second").unwrap();
    let h = Harness::new();
    let mut e = Session::new();
    for path in [first.clone(), second.clone()] {
        smol::block_on(open_file(
            &mut e,
            path,
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        ));
    }
    assert_eq!(e.docs.len(), 2);
    while h.note_rx.try_recv().is_ok() {}

    // b.txt is active; try to save it over a.txt, which the other buffer holds.
    let alive = smol::block_on(save_as_file(
        &mut e,
        first.clone(),
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert!(
        matches!(h.note_rx.try_recv(), Ok(Notification::FileError { .. })),
        "the clash must be surfaced, not silently allowed"
    );
    assert_eq!(
        std::fs::read_to_string(&first).unwrap(),
        "first",
        "the other buffer's file must not have been overwritten"
    );
    assert_eq!(
        e.active().path.as_deref(),
        Some(second.as_path()),
        "and the rejected save-as leaves the binding alone"
    );
}

#[test]
fn open_into_an_untouched_scratch_reuses_it() {
    // Launching bare and then opening a file must not strand an empty tab: an
    // unnamed, empty, unmodified buffer is loaded into rather than left behind.
    let dir = TempDir::new();
    let path = dir.file("only.txt");
    std::fs::write(&path, "contents").unwrap();

    let mut e = Session::new();
    let scratch = e.active().id;
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert_eq!(e.docs.len(), 1, "the scratch was reused, not added to");
    assert_eq!(e.active().id, scratch);
    assert_eq!(e.active().buffer.text().to_string(), "contents");
}

#[test]
fn opening_an_already_open_file_switches_to_it() {
    // Two buffers over one path would each carry their own history and could
    // overwrite each other's saves, so a repeat open is a switch: no second buffer,
    // no reload, and the buffer's own edits survive.
    let dir = TempDir::new();
    let path = dir.file("shared.txt");
    std::fs::write(&path, "on disk").unwrap();

    let mut e = Session::new();
    let h = Harness::new();
    let open = |e: &mut Session| {
        smol::block_on(open_file(
            e,
            path.clone(),
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        ))
    };
    open(&mut e);
    let first = e.active().id;
    // Type into it, then open a different file so it is no longer active.
    e.active_mut().buffer = RopeBuffer::from("edited");
    let other = dir.file("other.txt");
    std::fs::write(&other, "other").unwrap();
    smol::block_on(open_file(
        &mut e,
        other,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_ne!(e.active().id, first);

    // Re-opening the first path returns to that same buffer, edits intact.
    open(&mut e);
    assert_eq!(e.docs.len(), 2, "no third buffer was created");
    assert_eq!(e.active().id, first);
    assert_eq!(e.active().buffer.text().to_string(), "edited");

    // A switch announces itself so the frontend can re-attach server and grammar.
    let notes: Vec<_> = std::iter::from_fn(|| h.note_rx.try_recv().ok()).collect();
    assert!(
        notes.iter().any(|n| matches!(
            n,
            Notification::BufferSwitched { buffer_id, .. } if *buffer_id == first
        )),
        "expected a BufferSwitched for the reopened buffer, got {notes:?}"
    );
}

#[test]
fn open_missing_file_opens_empty_buffer_bound_to_path() {
    // A missing path is not an error (Vim's behavior): empty buffer, path bound,
    // created on save. `existed` is false so the frontend can say "[New File]".
    let dir = TempDir::new();
    let path = dir.file("does-not-exist.txt");

    let mut e = Session::new();
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert!(e.active().buffer.text().is_empty());
    assert_eq!(e.active().path, Some(path.clone()));
    assert!(!e.active().modified());
    assert_eq!(e.active().version, 0); // empty->empty: no delta, no version bump
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

    let mut e = Session::new();
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
fn open_binary_file_errors_and_leaves_buffer_unchanged() {
    let dir = TempDir::new();
    let path = dir.file("image.png");
    std::fs::write(&path, [0x89, b'P', b'N', b'G', 0x00, 0x1a]).unwrap();

    let mut e = session_with("keep me", SelectionSet::single(Selection::cursor(0)));
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(e.active().buffer.text().to_string(), "keep me"); // untouched
    assert_eq!(e.active().path, None); // binding not changed on a failed open
    assert!(h.delta_rx.is_empty());
    match h.note_rx.try_recv() {
        Ok(Notification::FileError {
            message, path: p, ..
        }) => {
            assert_eq!(p, Some(path));
            assert!(message.contains("binary"), "message: {message}");
        }
        other => panic!("expected FileError, got {other:?}"),
    }
}

/// Drop `path`'s write permission, returning whether it took - a test running as
/// root can write anything, so the read-only tests skip rather than fail there.
#[cfg(unix)]
fn make_unwritable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444)).unwrap();
    !is_writable(path)
}

#[cfg(unix)]
#[test]
fn a_file_this_process_cannot_write_opens_read_only() {
    let dir = TempDir::new();
    let path = dir.file("locked.txt");
    std::fs::write(&path, "look but do not touch\n").unwrap();
    if !make_unwritable(&path) {
        return; // running as root: the permission bits do not bind
    }

    let mut e = Session::new();
    let h = Harness::new();
    assert!(smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    assert_eq!(e.active().read_only, Some(ReadOnly::Permissions));
    assert!(h.snapshot().read_only); // the frontend can mark it
    assert_eq!(
        e.active().buffer.text().to_string(),
        "look but do not touch\n"
    );
}

#[cfg(unix)]
#[test]
fn a_read_only_buffer_refuses_edits_and_saves_but_still_yields_to_save_as() {
    let dir = TempDir::new();
    let path = dir.file("locked.txt");
    std::fs::write(&path, "original\n").unwrap();
    if !make_unwritable(&path) {
        return;
    }

    let target = dir.file("mine.txt");
    let (snap, notes) = run_seam(&[
        Action::Open(path.clone()),
        Action::Insert("nope".into()),
        Action::Save,
        Action::SaveAs(target.clone()),
        Action::Insert("yes".into()),
    ]);

    // The edit and the save were both refused, and the file never changed.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::EditRejected { message, .. } if message.contains("read-only"))),
        "expected an EditRejected, got {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::FileError { message, .. } if message.contains("read-only"))),
        "expected a FileError for the save, got {notes:?}"
    );
    // Save-as is the way out: the copy is writable, so the buffer is editable again
    // and the trailing insert landed.
    assert!(!snap.read_only);
    assert_eq!(snap.path, Some(target.clone()));
    assert_eq!(snap.text.to_string(), "yesoriginal\n");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "original\n");
}

#[test]
fn a_file_that_did_not_fully_decode_opens_read_only() {
    // A BOM claiming UTF-16 over bytes that are not: the text carries replacement
    // characters, so saving it back would overwrite what did not decode (SPEC §8).
    let dir = TempDir::new();
    let path = dir.file("truncated.txt");
    std::fs::write(&path, [0xFF, 0xFE, b'a', 0x00, 0x00, 0xD8]).unwrap();

    let mut e = Session::new();
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert_eq!(e.active().read_only, Some(ReadOnly::Undecodable));
    assert!(h.snapshot().read_only);
    // And the refusal explains itself with the reason that applies, rather than
    // claiming a permission problem the file does not have.
    let message = e.active().read_only.unwrap().message();
    assert!(
        message.contains("could not be decoded"),
        "message: {message}"
    );
}

// --- External changes (SPEC §10.2) --------------------------------------------

/// A session with `path` open, plus a channel standing in for the frontend's
/// watcher so the tests can assert on what the core asked it to watch.
fn watching(path: &Path, h: &Harness) -> (Session, Receiver<WatchRequest>) {
    let (watch_tx, watch_rx) = async_channel::bounded::<WatchRequest>(16);
    let mut e = Session::new();
    e.watch = Some(watch_tx);
    assert!(smol::block_on(open_file(
        &mut e,
        path.to_path_buf(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    let _ = h.snapshot(); // drain the open's snapshot
    while h.delta_rx.try_recv().is_ok() {} // ...and the delta that loaded it
    (e, watch_rx)
}

/// Every notification queued so far.
fn drain_notes(h: &Harness) -> Vec<Notification> {
    std::iter::from_fn(|| h.note_rx.try_recv().ok()).collect()
}

#[test]
fn a_clean_buffer_follows_its_file() {
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "before\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    let _ = drain_notes(&h);

    std::fs::write(&path, "after the change\n").unwrap();
    assert!(smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    assert_eq!(e.active().buffer.text().to_string(), "after the change\n");
    assert!(!e.active().modified(), "a reload is the on-disk state");
    let notes = drain_notes(&h);
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::FileReloaded { .. })),
        "expected FileReloaded, got {notes:?}"
    );
    // The reload is one whole-buffer delta, so a remote frontend replaying the
    // stream still lands on the same text (SPEC §5).
    let delta = h.delta_rx.try_recv().expect("a delta for the reload");
    assert_eq!(delta.new_text, "after the change\n");
}

#[test]
fn a_modified_buffer_asks_instead_of_following() {
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "before\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    mark_dirty(&mut e);
    let _ = drain_notes(&h);

    std::fs::write(&path, "someone else's version\n").unwrap();
    assert!(smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    // Neither side was overwritten: the buffer is untouched and so is the file.
    assert_eq!(e.active().buffer.text().to_string(), "before\n");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "someone else's version\n"
    );
    let notes = drain_notes(&h);
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::ExternalChange { removed: false, .. })),
        "expected an ExternalChange, got {notes:?}"
    );
}

#[test]
fn the_editors_own_save_is_not_an_external_change() {
    // The loudest source of change events is this editor writing the file. Reading
    // that back as someone else's edit would make the feature unusable.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "before\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    e.active_mut().selections = SelectionSet::at_origin();
    let edits = e.active().plan_edit(EditKind::Insert("typed ".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx)));
    let _ = drain_notes(&h);

    assert!(smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert_eq!(
        drain_notes(&h),
        Vec::new(),
        "our own write must raise nothing"
    );
}

#[test]
fn one_external_write_raises_one_prompt_however_many_events_arrive() {
    // Platforms send several events for a single write. The stamp is advanced when
    // the conflict is reported, so the repeats find nothing new to say.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "before\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    mark_dirty(&mut e);
    let _ = drain_notes(&h);

    std::fs::write(&path, "changed by someone else\n").unwrap();
    for _ in 0..3 {
        assert!(smol::block_on(external_change(
            &mut e,
            &path,
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        )));
    }

    let conflicts = drain_notes(&h)
        .iter()
        .filter(|n| matches!(n, Notification::ExternalChange { .. }))
        .count();
    assert_eq!(conflicts, 1);
}

#[test]
fn a_removed_file_keeps_the_buffer_that_is_now_its_only_copy() {
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "precious\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    let _ = drain_notes(&h);

    std::fs::remove_file(&path).unwrap();
    assert!(smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    // Emphatically not reloaded to empty.
    assert_eq!(e.active().buffer.text().to_string(), "precious\n");
    let notes = drain_notes(&h);
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::ExternalChange { removed: true, .. })),
        "expected a removal, got {notes:?}"
    );
}

#[test]
fn an_event_for_a_file_no_buffer_holds_is_ignored() {
    // Directories are watched, not files, so events about the neighbours arrive as
    // a matter of course.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "mine\n").unwrap();
    let stranger = dir.file("theirs.txt");
    std::fs::write(&stranger, "not mine\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    let _ = drain_notes(&h);

    assert!(smol::block_on(external_change(
        &mut e,
        &stranger,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert_eq!(drain_notes(&h), Vec::new());
}

#[test]
fn a_reload_keeps_the_cursor_where_the_text_still_reaches() {
    let dir = TempDir::new();
    let path = dir.file("log.txt");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    e.active_mut().selections = SelectionSet::single(Selection::cursor(9));

    // A file that grew: the caret is still a valid position, so it does not move.
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(e.active().selections.primary().head, 9);

    // A file that shrank past the caret: clamped to the end, not sent home.
    std::fs::write(&path, "one\n").unwrap();
    smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(e.active().selections.primary().head, 4);
}

#[test]
fn a_reload_that_finds_the_same_bytes_emits_nothing() {
    // Idempotence is what makes a burst of duplicate events harmless.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "unchanged\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    let version = e.active().version;
    let _ = drain_notes(&h);
    while h.delta_rx.try_recv().is_ok() {}

    assert!(smol::block_on(reload(
        &mut e,
        0,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert_eq!(
        e.active().version,
        version,
        "no version bump without a delta"
    );
    assert!(h.delta_rx.is_empty());
}

#[test]
fn reload_is_refused_on_a_modified_buffer_unless_forced() {
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "on disk\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    mark_dirty(&mut e);
    let id = e.active().id;
    let _ = drain_notes(&h);

    assert!(smol::block_on(reload_buffer(
        &mut e,
        id,
        false,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert!(e.active().modified(), "the buffer is untouched");
    let notes = drain_notes(&h);
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::ReloadRejected { .. })),
        "expected ReloadRejected, got {notes:?}"
    );

    // Forced - what the frontend sends once the user has confirmed.
    assert!(smol::block_on(reload_buffer(
        &mut e,
        id,
        true,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert_eq!(e.active().buffer.text().to_string(), "on disk\n");
    assert!(!e.active().modified());
}

#[test]
fn reloading_an_unknown_or_unbound_buffer_is_a_no_op() {
    let mut e = session_with("scratch", SelectionSet::at_origin());
    let h = Harness::new();
    let id = e.active().id;

    // No file bound: nothing to re-read, and not an error either.
    assert!(smol::block_on(reload_buffer(
        &mut e,
        id,
        true,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert_eq!(e.active().buffer.text().to_string(), "scratch");
    // A stale id from a frontend that has not seen a close yet.
    assert!(smol::block_on(reload_buffer(
        &mut e,
        BufferId(999),
        true,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert_eq!(drain_notes(&h), Vec::new());
}

#[test]
fn a_reload_whose_file_has_gone_reports_it_and_keeps_the_buffer() {
    // The user confirms a reload, and the file is deleted before it runs. The
    // buffer is the only copy again, so it is kept and the failure surfaces.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "still here\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    let id = e.active().id;
    std::fs::remove_file(&path).unwrap();
    let _ = drain_notes(&h);

    assert!(smol::block_on(reload_buffer(
        &mut e,
        id,
        true,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));
    assert_eq!(e.active().buffer.text().to_string(), "still here\n");
    let notes = drain_notes(&h);
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::FileError { .. })),
        "expected a FileError, got {notes:?}"
    );
}

#[test]
fn a_reload_re_detects_the_files_form_and_whether_it_can_be_written() {
    // Whoever rewrote the file may have changed its encoding, its line endings, or
    // its permissions. A reload that kept the old answers would save it back wrong.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "plain\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    assert_eq!(e.active().format.encoding_name(), "UTF-8");
    assert_eq!(e.active().format.eol, crate::file::LineEnding::Lf);
    assert!(e.active().read_only.is_none());

    // Rewritten as CRLF latin-1, and truncated mid-UTF-16 - two different answers.
    std::fs::write(&path, b"caf\xE9\r\nmore\r\n").unwrap();
    smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(e.active().format.encoding_name(), "windows-1252");
    assert_eq!(e.active().format.eol, crate::file::LineEnding::Crlf);
    assert!(e.active().read_only.is_none());

    std::fs::write(&path, [0xFF, 0xFE, b'a', 0x00, 0x00, 0xD8]).unwrap();
    smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(
        e.active().read_only,
        Some(ReadOnly::Undecodable),
        "a file that stopped decoding stops being editable"
    );
}

#[cfg(unix)]
#[test]
fn a_file_that_became_unwritable_stops_being_editable_on_reload() {
    // The counterpart to re-detecting the encoding: whoever rewrote the file may
    // have changed its permissions, and a buffer that kept the old answer would let
    // the user type into a file the save is going to refuse.
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "writable\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    assert!(e.active().read_only.is_none());

    std::fs::write(&path, "locked down now\n").unwrap();
    if !make_unwritable(&path) {
        return; // running as root: the permission bits do not bind
    }
    smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(e.active().read_only, Some(ReadOnly::Permissions));
    assert_eq!(e.active().buffer.text().to_string(), "locked down now\n");
}

#[test]
fn a_file_replaced_by_a_binary_one_is_reported_not_loaded() {
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "text\n").unwrap();

    let h = Harness::new();
    let (mut e, _watch) = watching(&path, &h);
    let _ = drain_notes(&h);

    std::fs::write(&path, [0x89, b'P', b'N', b'G', 0x00]).unwrap();
    assert!(smol::block_on(external_change(
        &mut e,
        &path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    assert_eq!(e.active().buffer.text().to_string(), "text\n");
    let notes = drain_notes(&h);
    assert!(
        notes.iter().any(|n| matches!(
            n,
            Notification::FileError { message, .. } if message.contains("binary")
        )),
        "expected a binary FileError, got {notes:?}"
    );
}

#[test]
fn the_core_asks_the_watcher_to_follow_the_files_it_holds() {
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "hello\n").unwrap();

    let h = Harness::new();
    let (mut e, watch) = watching(&path, &h);
    assert_eq!(
        watch.try_recv(),
        Ok(WatchRequest::Watch(path.clone())),
        "opening a file starts watching it"
    );

    // A save-as moves the watch with the buffer: the old file is no longer ours.
    let target = dir.file("renamed.txt");
    smol::block_on(save_as_file(
        &mut e,
        target.clone(),
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(watch.try_recv(), Ok(WatchRequest::Unwatch(path)));
    assert_eq!(watch.try_recv(), Ok(WatchRequest::Watch(target.clone())));

    // ...and closing releases it.
    let id = e.active().id;
    close_buffer(&mut e, id, true, &h.snapshots, &h.note_tx);
    assert_eq!(watch.try_recv(), Ok(WatchRequest::Unwatch(target)));
}

#[test]
fn a_watcher_attaching_late_is_told_about_the_files_already_open() {
    let dir = TempDir::new();
    let path = dir.file("notes.txt");
    std::fs::write(&path, "hello\n").unwrap();

    let h = Harness::new();
    let mut e = Session::new();
    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    // The watcher arrives after the file did - which is the ordering at startup.
    let (watch_tx, watch_rx) = async_channel::bounded::<WatchRequest>(16);
    e.watch = Some(watch_tx);
    e.announce_watched();
    assert_eq!(watch_rx.try_recv(), Ok(WatchRequest::Watch(path)));
}

#[test]
fn save_as_onto_a_read_only_buffers_own_file_is_still_refused() {
    // Regression: "save as" over the same name is a plain save, and must not be the
    // way around the guard. For this buffer that would write U+FFFD over the bytes
    // that never decoded.
    let dir = TempDir::new();
    let path = dir.file("truncated.txt");
    let original = [0xFF, 0xFE, b'a', 0x00, 0x00, 0xD8];
    std::fs::write(&path, original).unwrap();

    let mut e = Session::new();
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(e.active().read_only.is_some());

    // The same file, spelled differently, so identity is what has to catch it.
    let spelled_differently = dir.path.join(".").join("truncated.txt");
    assert!(smol::block_on(save_as_file(
        &mut e,
        spelled_differently,
        &h.snapshots,
        &h.note_tx
    )));
    assert_eq!(std::fs::read(&path).unwrap(), original); // untouched
    let notes: Vec<_> = std::iter::from_fn(|| h.note_rx.try_recv().ok()).collect();
    assert!(
        notes.iter().any(|n| matches!(
            n,
            Notification::FileError { message, .. } if message.contains("decoded")
        )),
        "expected a read-only FileError, got {notes:?}"
    );
}

#[test]
fn a_path_that_runs_through_a_file_is_an_error_not_a_new_buffer() {
    // `dir/notes.txt/deeper` is not "missing", it is impossible - the stat fails
    // with something that is not NotFound, which must not be mistaken for the
    // open-an-empty-buffer case.
    let dir = TempDir::new();
    let file = dir.file("notes.txt");
    std::fs::write(&file, "hello\n").unwrap();

    let mut e = Session::new();
    let h = Harness::new();
    assert!(smol::block_on(open_file(
        &mut e,
        file.join("deeper"),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    assert_eq!(e.active().path, None); // nothing was bound
    assert!(matches!(
        h.note_rx.try_recv(),
        Ok(Notification::FileError { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_file_that_cannot_even_be_read_is_an_error() {
    // Mode 000: the stat succeeds and says regular file, so the failure lands on
    // the read - the one error path the type check above cannot pre-empt.
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let path = dir.file("secret.txt");
    std::fs::write(&path, "classified\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&path).is_ok() {
        return; // running as root: the permission bits do not bind
    }

    let mut e = session_with("keep me", SelectionSet::at_origin());
    let h = Harness::new();
    assert!(smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    assert_eq!(e.active().buffer.text().to_string(), "keep me");
    assert!(matches!(
        h.note_rx.try_recv(),
        Ok(Notification::FileError { .. })
    ));
}

#[test]
fn every_step_declares_whether_it_edits_the_buffer() {
    // The read-only guard sits on the step, so this is the table it consults. Undo
    // and redo are on the blocked side because they change text just as much as an
    // insert does - a read-only buffer walked backwards would still be a read-only
    // file rewritten.
    assert!(Step::Edit(Vec::new()).edits_buffer());
    assert!(Step::Undo.edits_buffer());
    assert!(Step::Redo.edits_buffer());
    // Save is not listed: it writes the file, not the buffer, and `save_file` holds
    // that guard so the refusal can be a `FileError` instead of an `EditRejected`.
    assert!(!Step::Save.edits_buffer());
    assert!(!Step::SaveAs(PathBuf::from("x")).edits_buffer());
    assert!(!Step::Republish.edits_buffer());
    assert!(!Step::Open(PathBuf::from("x")).edits_buffer());
    assert!(!Step::Switch(BufferId(0)).edits_buffer());
    assert!(
        !Step::Close {
            id: BufferId(0),
            force: false
        }
        .edits_buffer()
    );
}

#[test]
fn opening_a_directory_is_an_error_not_an_empty_buffer() {
    let dir = TempDir::new();
    let mut e = session_with("keep me", SelectionSet::at_origin());
    let h = Harness::new();
    assert!(smol::block_on(open_file(
        &mut e,
        dir.path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    assert_eq!(e.active().buffer.text().to_string(), "keep me");
    assert_eq!(e.active().path, None);
    match h.note_rx.try_recv() {
        Ok(Notification::FileError { message, .. }) => {
            assert!(message.contains("directory"), "message: {message}");
        }
        other => panic!("expected FileError, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn opening_a_fifo_is_refused_rather_than_blocking_the_actor() {
    // Reading a FIFO blocks until a writer appears. On the actor thread that is the
    // whole editor hung with no way out, which is why the type check happens before
    // the read (SPEC §10.3). If this test ever hangs, the guard is gone.
    let dir = TempDir::new();
    let path = dir.file("pipe");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("mkfifo");
    assert!(status.success());

    let mut e = Session::new();
    let h = Harness::new();
    assert!(smol::block_on(open_file(
        &mut e,
        path,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    assert_eq!(e.active().path, None);
    match h.note_rx.try_recv() {
        Ok(Notification::FileError { message, .. }) => {
            assert!(message.contains("regular file"), "message: {message}");
        }
        other => panic!("expected FileError, got {other:?}"),
    }
}

#[test]
fn open_non_utf8_text_file_loads_and_saves_back_unchanged() {
    // The other half of the binary guard: a file that is merely not UTF-8 is text
    // and must open (SPEC §10.1), where before M5 it was rejected outright. Saving
    // it reproduces its bytes rather than rewriting it as UTF-8.
    let dir = TempDir::new();
    let path = dir.file("latin1.txt");
    let original = b"caf\xE9 cr\xE8me\r\n"; // latin-1 "café crème", CRLF
    std::fs::write(&path, original).unwrap();

    let mut e = Session::new();
    let h = Harness::new();
    assert!(smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    )));

    let snap = h.snapshot();
    assert_eq!(snap.format.encoding_name(), "windows-1252");
    assert_eq!(snap.format.eol, crate::file::LineEnding::Crlf);
    assert!(!snap.text.to_string().contains('\r')); // the buffer is always LF

    assert!(smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx)));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn saving_a_character_the_files_encoding_cannot_hold_is_refused() {
    // A latin-1 file that gains an emoji: writing it would either mangle the
    // character or rewrite the whole file as UTF-8. Refuse, keep the buffer dirty,
    // and leave the file untouched so the work can go somewhere else (SPEC §8).
    let dir = TempDir::new();
    let path = dir.file("latin1.txt");
    let original = b"caf\xE9\n";
    std::fs::write(&path, original).unwrap();

    let mut e = Session::new();
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    e.active_mut().selections = SelectionSet::at_origin();
    let edits = e.active().plan_edit(EditKind::Insert("😀".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx)));
    assert!(e.active().modified()); // still dirty: nothing was saved
    assert_eq!(std::fs::read(&path).unwrap(), original); // file untouched
    let notes: Vec<_> = std::iter::from_fn(|| h.note_rx.try_recv().ok()).collect();
    assert!(
        notes.iter().any(|n| matches!(
            n,
            Notification::FileError { message, .. } if message.contains("windows-1252")
        )),
        "expected an encoding FileError, got {notes:?}"
    );
}

#[test]
fn save_writes_buffer_to_bound_file_and_clears_modified() {
    let dir = TempDir::new();
    let path = dir.file("out.txt");

    let mut e = session_with("saved text", SelectionSet::at_origin());
    e.active_mut().path = Some(path.clone());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    // The trailing newline is the POSIX final-newline policy (SPEC §10.1), applied
    // to the bytes written and never to the buffer - which is why the document is
    // clean below rather than dirtied by its own save.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "saved text\n");
    assert!(!e.active().modified()); // clean after a successful save
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
    let mut e = session_with("unsaved", SelectionSet::at_origin());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.active().modified()); // still dirty
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

    let mut e = session_with("new work", SelectionSet::at_origin());
    e.active_mut().path = Some(path.clone());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.active().modified()); // failed save keeps the buffer dirty
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

    let mut e = Session::new();
    let h = Harness::new();

    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    // Move to end and append "d".
    e.active_mut().selections = SelectionSet::single(Selection::cursor(3));
    let edits = e.active().plan_edit(EditKind::Insert("d".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(e.active().modified());
    smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(!e.active().modified());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "abcd\n");
}

#[test]
fn save_as_writes_to_target_adopts_it_and_clears_modified() {
    // Save-as on a buffer with no bound file: the write lands, the path is adopted
    // (a following plain Save targets it), and the buffer goes clean (SPEC §7.5, §8).
    let dir = TempDir::new();
    let target = dir.file("as.txt");

    let mut e = session_with("save-as body", SelectionSet::at_origin());
    assert!(e.active().path.is_none());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_as_file(
        &mut e,
        target.clone(),
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "save-as body\n");
    assert_eq!(e.active().path.as_deref(), Some(target.as_path())); // adopted
    assert!(!e.active().modified()); // clean after a successful save-as
    match h.note_rx.try_recv() {
        Ok(Notification::FileSaved { path: p, .. }) => assert_eq!(p, target),
        other => panic!("expected FileSaved, got {other:?}"),
    }
    // The adopted path is now bound: a subsequent plain Save writes to it.
    e.active_mut().selections = SelectionSet::single(Selection::cursor(12));
    let edits = e.active().plan_edit(EditKind::Insert("!".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "save-as body!\n");
    assert!(!has_temp_file(&dir.path), "leftover .vortex-tmp file");
}

#[test]
fn save_as_is_refused_when_the_text_does_not_fit_the_files_encoding() {
    // Save-as keeps the source file's encoding, so it inherits the same refusal a
    // plain save gets - and refuses *before* the write, leaving no half-written
    // target behind (SPEC §8).
    let dir = TempDir::new();
    let source = dir.file("latin1.txt");
    std::fs::write(&source, b"caf\xE9\n").unwrap();
    let target = dir.file("copy.txt");

    let mut e = Session::new();
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        source,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    e.active_mut().selections = SelectionSet::at_origin();
    let edits = e.active().plan_edit(EditKind::Insert("😀".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(smol::block_on(save_as_file(
        &mut e,
        target.clone(),
        &h.snapshots,
        &h.note_tx
    )));
    assert!(!target.exists(), "nothing should have been written");
    assert!(e.active().modified()); // still dirty
    let notes: Vec<_> = std::iter::from_fn(|| h.note_rx.try_recv().ok()).collect();
    assert!(
        notes.iter().any(|n| matches!(
            n,
            Notification::FileError { message, .. } if message.contains("windows-1252")
        )),
        "expected an encoding FileError, got {notes:?}"
    );
}

#[test]
fn save_as_failure_keeps_old_path_and_stays_dirty() {
    // Save-as whose write fails (target is a directory): the buffer keeps its
    // previous path binding and stays dirty, and the target is untouched - a
    // rejected save-as loses neither the work nor the original association (SPEC §8).
    let dir = TempDir::new();
    let old = dir.file("original.txt");
    let target = dir.file("a-directory");
    std::fs::create_dir(&target).unwrap();

    let mut e = session_with("in progress", SelectionSet::at_origin());
    e.active_mut().path = Some(old.clone());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_as_file(
        &mut e,
        target.clone(),
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(e.active().path.as_deref(), Some(old.as_path())); // binding unchanged
    assert!(e.active().modified()); // still dirty
    assert!(target.is_dir()); // the target directory is intact, not clobbered
    match h.note_rx.try_recv() {
        Ok(Notification::FileError { path: p, .. }) => assert_eq!(p, Some(target.clone())),
        other => panic!("expected FileError, got {other:?}"),
    }
    assert!(!has_temp_file(&dir.path), "leftover .vortex-tmp file");
}

#[test]
fn save_as_to_a_new_path_reannounces_to_the_language_server() {
    // Adopting a *different* path changes the document's identity, so the server is
    // re-announced: the next sync sends a fresh didOpen under the new name. A save-as
    // to the *same* path is a plain re-save and leaves the announce state alone.
    let dir = TempDir::new();
    let old = dir.file("a.rs");
    let renamed = dir.file("b.rs");

    let mut e = session_with("fn main() {}", SelectionSet::at_origin());
    e.active_mut().path = Some(old.clone());
    let id = e.active().id;
    // Announced to a connected server, and up to date with it.
    let (tx, _rx) = async_channel::bounded(4);
    let mut lsp = LspConnection::new(tx);
    lsp.opened.insert(id);
    e.lsp = Some(lsp);
    e.active_mut().lsp_dirty = false;

    let h = Harness::new();
    smol::block_on(save_as_file(
        &mut e,
        renamed.clone(),
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(
        !e.lsp.as_ref().unwrap().opened.contains(&id),
        "a renamed document must be re-opened to the server"
    );
    assert!(e.active().lsp_dirty, "the new identity must be re-synced");

    // Saving-as again to the same (now current) path is a plain save: no re-announce.
    e.lsp.as_mut().unwrap().opened.insert(id);
    e.active_mut().lsp_dirty = false;
    smol::block_on(save_as_file(
        &mut e,
        renamed.clone(),
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(
        e.lsp.as_ref().unwrap().opened.contains(&id),
        "same-path save-as must not reset the announce state"
    );
    assert!(!e.active().lsp_dirty);
}

#[test]
fn save_writes_to_a_new_file_that_did_not_exist() {
    // Opening a missing path then saving creates the file (Vim's behavior).
    let dir = TempDir::new();
    let path = dir.file("created-on-save.txt");

    let mut e = Session::new();
    let h = Harness::new();
    smol::block_on(open_file(
        &mut e,
        path.clone(),
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    e.active_mut().selections = SelectionSet::at_origin();
    let edits = e.active().plan_edit(EditKind::Insert("brand new".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(path.exists());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "brand new\n");
}

#[test]
fn snapshot_carries_path_and_modified() {
    let mut e = session_with("x", SelectionSet::at_origin());
    e.active_mut().path = Some(PathBuf::from("/tmp/demo.txt"));
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
    let mut e = session_with("keep me", SelectionSet::single(Selection::cursor(0)));
    let h = Harness::new();
    let alive = smol::block_on(open_file(
        &mut e,
        dir.path.clone(), // the directory itself - read() fails, not NotFound
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert!(alive);
    assert_eq!(e.active().buffer.text().to_string(), "keep me"); // untouched
    assert_eq!(e.active().path, None);
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
    let mut e = session_with("work", SelectionSet::at_origin());
    e.active_mut().path = Some(path.clone());
    mark_dirty(&mut e);

    let h = Harness::new();
    let alive = smol::block_on(save_file(&mut e, &h.snapshots, &h.note_tx));

    assert!(alive);
    assert!(e.active().modified());
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

// --- Multi-buffer (M7) --------------------------------------------------------

/// A session with two buffers open, both bound to real files: buffer 0 holds
/// `first`, buffer 1 holds `second`, and buffer 1 is active (the open order).
/// Returns the session, the two ids, and the temp dir the files live in (kept
/// alive by the caller, since dropping it deletes them).
fn two_buffers(first: &str, second: &str) -> (Session, BufferId, BufferId, TempDir, Harness) {
    let dir = TempDir::new();
    let h = Harness::new();
    let mut e = Session::new();
    for (name, body) in [("a.txt", first), ("b.txt", second)] {
        let path = dir.file(name);
        std::fs::write(&path, body).unwrap();
        smol::block_on(open_file(
            &mut e,
            path,
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        ));
    }
    // Drain so tests assert only on what they trigger themselves.
    while h.delta_rx.try_recv().is_ok() {}
    while h.note_rx.try_recv().is_ok() {}
    let (a, b) = (e.docs[0].id, e.docs[1].id);
    (e, a, b, dir, h)
}

#[test]
fn closing_a_buffer_drops_it_from_the_connection() {
    // Ids are never reused, so a leftover entry could not mis-target a later buffer,
    // but it would accumulate for the session's life. The connection owns the set, so
    // closing a document is the one place that can prune it.
    let (mut e, a, b, _dir, h) = two_buffers("alpha", "beta");
    let (tx, _rx) = async_channel::bounded(4);
    let mut lsp = LspConnection::new(tx);
    lsp.opened.insert(a);
    lsp.opened.insert(b);
    e.lsp = Some(lsp);

    assert!(close_buffer(&mut e, a, false, &h.snapshots, &h.note_tx));

    let opened = &e.lsp.as_ref().unwrap().opened;
    assert!(!opened.contains(&a), "the closed buffer is forgotten");
    assert!(opened.contains(&b), "the surviving one is not");
}

#[test]
fn each_buffer_keeps_its_own_version_history_and_selections() {
    // The core invariant multi-buffer rests on (SPEC §5): an edit in one buffer
    // must not touch another's version, undo tree, or carets.
    let (mut e, a, _b, _dir, h) = two_buffers("alpha", "beta");
    let before = (e.docs[0].version, e.docs[0].selections.clone());

    let edits = e.active().plan_edit(EditKind::Insert("!".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));

    assert_eq!(e.active().buffer.text().to_string(), "!beta");
    assert_eq!(
        e.docs[0].version, before.0,
        "the background buffer's version must not move"
    );
    assert_eq!(e.docs[0].buffer.text().to_string(), "alpha");
    assert_eq!(e.docs[0].selections, before.1);

    // Undo in the active buffer does not reach across into the other's history.
    let reverted = e.active_mut().history.undo();
    smol::block_on(reapply(
        &mut e,
        reverted,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(e.active().buffer.text().to_string(), "beta");
    assert_eq!(e.docs[0].buffer.text().to_string(), "alpha");
    assert_eq!(e.index_of(a), Some(0));
}

#[test]
fn the_snapshot_lists_every_open_buffer() {
    let (mut e, a, b, _dir, _h) = two_buffers("alpha", "beta");
    let snap = e.snapshot(None);

    assert_eq!(snap.buffer_id, b, "the active buffer names the snapshot");
    let ids: Vec<_> = snap.buffers.iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![a, b], "in open order");
    assert!(snap.buffers.iter().all(|i| !i.modified));
    assert!(snap.buffers.iter().all(|i| i.path.is_some()));
}

#[test]
fn the_buffer_list_is_cached_but_tracks_the_active_buffer() {
    // The list is rebuilt only when it would differ, so a repeat publish shares the
    // same allocation (it is cloned per frame). A change to the active buffer's own
    // entry still has to be caught, which is what the cheap comparison is for.
    let (mut e, _a, _b, _dir, _h) = two_buffers("alpha", "beta");
    let first = e.snapshot(None).buffers;
    let again = e.snapshot(None).buffers;
    assert!(
        Arc::ptr_eq(&first, &again),
        "an unchanged list must not be rebuilt"
    );

    mark_dirty(&mut e);
    let after = e.snapshot(None).buffers;
    assert!(
        !Arc::ptr_eq(&first, &after),
        "the active buffer became modified, so the list must be rebuilt"
    );
    assert!(after[1].modified);
    assert!(!after[0].modified);
}

#[test]
fn clipboard_round_trips_through_the_actor_loop() {
    // The copy/cut/paste dispatch arms end to end, over the real loop: these read
    // the active document but write the session-wide register, so a script is the
    // only thing that proves the two are wired to each other correctly.
    let (snap, notes) = run_seam(&[
        Action::Insert("hello world".into()),
        // Select "hello": place at 0, then extend the head to 5.
        Action::PlaceCursor {
            offset: 0,
            extend: false,
        },
        Action::PlaceCursor {
            offset: 5,
            extend: true,
        },
        Action::Copy,
        // Caret to the end, then paste the register there.
        Action::PlaceCursor {
            offset: 11,
            extend: false,
        },
        Action::Paste,
    ]);
    assert_eq!(snap.text.to_string(), "hello worldhello");

    // A copy mirrors the register to the frontend for the OS clipboard (SPEC §11).
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::SetClipboard { text } if text == "hello")),
        "expected a SetClipboard mirror, got {notes:?}"
    );

    // Cut removes the selection and refills the register, still as one undo unit.
    let (snap, _) = run_seam(&[
        Action::Insert("abcdef".into()),
        Action::PlaceCursor {
            offset: 2,
            extend: false,
        },
        Action::PlaceCursor {
            offset: 4,
            extend: true,
        },
        Action::Cut,
        Action::Paste,
    ]);
    // "cd" cut out then pasted straight back at the caret it left behind.
    assert_eq!(snap.text.to_string(), "abcdef");
}

#[test]
fn delete_and_redo_dispatch_through_the_actor_loop() {
    // The remaining edit arms over the real loop: backspace, delete-forward, and
    // redo (undo already has seam coverage above).
    let (snap, _) = run_seam(&[
        Action::Insert("abcd".into()),
        Action::DeleteBackward, // "abc", caret at 3
        Action::PlaceCursor {
            offset: 0,
            extend: false,
        },
        Action::DeleteForward, // "bc"
    ]);
    assert_eq!(snap.text.to_string(), "bc");

    let (snap, _) = run_seam(&[
        Action::Insert("xy".into()),
        Action::Undo,
        Action::Redo,
        Action::RequestSnapshot,
    ]);
    assert_eq!(snap.text.to_string(), "xy");
}

#[test]
fn open_with_delta_receiver_dropped_reports_frontend_gone() {
    // Opening a non-empty file emits a whole-buffer delta; if the frontend has
    // dropped the delta receiver, the send fails and open_file returns false
    // ("frontend gone"), so the actor loop can stop cleanly.
    let dir = TempDir::new();
    let path = dir.file("has-content.txt");
    std::fs::write(&path, "content").unwrap();

    let mut e = Session::new();
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
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "Zabc\n");
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
fn save_as_through_the_actor_loop() {
    // End-to-end through the real loop (the dispatch arm the direct save_as_file
    // tests bypass): type text with no bound file, then SaveAs writes it, adopts the
    // path, and reports it clean - what the frontend's prompt commits (SPEC §7.5).
    let dir = TempDir::new();
    let target = dir.file("committed.txt");

    let (snap, notes) = run_seam(&[
        Action::Insert("hello".into()),
        Action::SaveAs(target.clone()),
    ]);

    assert_eq!(snap.path, Some(target.clone()));
    assert!(!snap.modified); // clean after the save-as
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\n");
    assert!(
        notes
            .iter()
            .any(|n| matches!(n, Notification::FileSaved { path, .. } if path == &target))
    );
}

#[test]
fn multi_cursor_undo_restores_every_cursor() {
    // One Insert over two cursors is one undo unit (SPEC §2.4); undoing it must
    // remove both inserted spans at their shifted offsets, not just one. Reachable
    // only via apply_edit + reapply with >1 selection - the multi-cursor path the
    // single-selection message seam cannot yet drive.
    let set = SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
    let mut e = session_with("abcdef", set);
    let h = Harness::new();

    let edits = e.active().plan_edit(EditKind::Insert("X".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert_eq!(e.active().buffer.text().to_string(), "aXbcdXef");

    let reverted = e.active_mut().history.undo();
    let alive = smol::block_on(reapply(
        &mut e,
        reverted,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    assert!(alive);
    assert_eq!(
        e.active().buffer.text().to_string(),
        "abcdef",
        "both cursors' inserts undone"
    );
    // Selections restored to the two original carets.
    assert_eq!(
        e.active().selections.all(),
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
    let mut e = session_with("abc", SelectionSet::single(Selection::cursor(3)));
    let h = Harness::new();
    // Record an edit so there is something to undo.
    let edits = e.active().plan_edit(EditKind::Insert("d".into()));
    smol::block_on(apply_edit(
        &mut e,
        edits,
        &h.delta_tx,
        &h.snapshots,
        &h.note_tx,
    ));
    drop(h.delta_rx); // frontend hangs up the delta channel

    let reverted = e.active_mut().history.undo();
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
        let mut e = session_with("abcdef", set);
        assert!(e.fill_register());
        assert_eq!(e.register, vec!["ab".to_string(), "ef".to_string()]);
    }

    #[test]
    fn fill_register_is_noop_for_bare_cursors() {
        // Nothing selected: the register is left untouched and the caller is told
        // not to emit a clipboard notification.
        let mut e = session_with("abc", SelectionSet::single(Selection::cursor(1)));
        e.register = vec!["previous".into()];
        assert!(!e.fill_register());
        assert_eq!(e.register, vec!["previous".to_string()]); // unchanged
    }

    #[test]
    fn register_flattened_joins_entries_with_newline() {
        let mut e = session_with("", SelectionSet::at_origin());
        e.register = vec!["one".into(), "two".into(), "three".into()];
        assert_eq!(e.register_flattened(), "one\ntwo\nthree");
    }

    #[test]
    fn plan_paste_single_entry_splats_to_every_cursor() {
        // One register entry goes to all cursors (the common single-copy paste).
        let set =
            SelectionSet::from_sorted_cursors(vec![Selection::cursor(0), Selection::cursor(2)]);
        let mut e = session_with("ab", set);
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
        let mut e = session_with("ab", set);
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
        let mut e = session_with("ab", set);
        e.register = vec!["a".into(), "b".into(), "c".into()];
        let edits = e.plan_paste();
        assert_eq!(
            edits,
            vec![(2..2, "a\nb\nc".into()), (0..0, "a\nb\nc".into())]
        );
    }

    #[test]
    fn plan_paste_empty_register_is_noop() {
        let e = session_with("ab", SelectionSet::single(Selection::cursor(1)));
        assert!(e.plan_paste().is_empty());
    }

    #[test]
    fn plan_paste_replaces_a_nonempty_selection() {
        // Paste over a selection replaces it (the range is the selection span, not a
        // zero-width insert), mirroring Insert's replace-then-insert.
        let set = SelectionSet::single(Selection::new(0, 3)); // "abc" selected
        let mut e = session_with("abcdef", set);
        e.register = vec!["Z".into()];
        assert_eq!(e.plan_paste(), vec![(0..3, "Z".into())]);
    }

    #[test]
    fn delete_selection_editkind_skips_bare_cursors() {
        // The cut edit deletes only non-empty selections; a bare cursor contributes
        // nothing (unlike backspace/delete, which step a grapheme at a cursor).
        let cursor = session_with("abc", SelectionSet::single(Selection::cursor(1)));
        assert!(
            cursor
                .active()
                .plan_edit(EditKind::DeleteSelection)
                .is_empty()
        );

        let selected = session_with("abc", SelectionSet::single(Selection::new(0, 2)));
        assert_eq!(
            selected.active().plan_edit(EditKind::DeleteSelection),
            vec![(0..2, String::new())]
        );
    }

    #[test]
    fn copy_then_paste_round_trips_through_the_register() {
        // End-to-end register path: select "ab", copy (fills register + flattens for
        // the clipboard mirror), collapse to a caret at end, then paste it back.
        let mut e = session_with("abcdef", SelectionSet::single(Selection::new(0, 2)));
        assert!(e.fill_register());
        assert_eq!(e.register_flattened(), "ab");

        // Caret at end of buffer, paste the register there.
        e.active_mut().selections = SelectionSet::single(Selection::cursor(6));
        let edits = e.plan_paste();
        let h = Harness::new();
        smol::block_on(apply_edit(
            &mut e,
            edits,
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        ));
        assert_eq!(e.active().buffer.text().to_string(), "abcdefab");
        assert_eq!(e.active().version, 1);
    }

    #[test]
    fn cut_is_one_edit_that_deletes_the_selection() {
        // Cut fills the register then applies DeleteSelection as one edit: the
        // selected text is removed and one undo unit recorded (version bumps once).
        let mut e = session_with("abcdef", SelectionSet::single(Selection::new(2, 4)));
        assert!(e.fill_register());
        assert_eq!(e.register, vec!["cd".to_string()]);

        let edits = e.active().plan_edit(EditKind::DeleteSelection);
        let h = Harness::new();
        smol::block_on(apply_edit(
            &mut e,
            edits,
            &h.delta_tx,
            &h.snapshots,
            &h.note_tx,
        ));
        assert_eq!(e.active().buffer.text().to_string(), "abef");
        assert_eq!(e.active().version, 1);
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

// --- Producer sync (SPEC §5) -------------------------------------------------
//
// The sync methods live on `Session` (they hold the one attached producer) but
// read and clear per-`Document` flags. These drive them directly: the actor loop
// only ever runs them with no producer attached, so the send paths would
// otherwise go untested.

#[test]
fn sync_lsp_says_nothing_about_an_unnamed_buffer() {
    // A server is attached but the active document has no path. There is no URI to
    // announce, so nothing is sent - a `didOpen` needs a document identity. The
    // dirty flag stays set, so the first `Save`/`Open` that binds a path announces
    // the buffer then.
    let (tx, rx) = async_channel::bounded::<DocumentSync>(4);
    let mut s = session_with("typed but never saved", SelectionSet::at_origin());
    s.lsp = Some(LspConnection::new(tx));
    s.active_mut().lsp_dirty = true;

    s.sync_lsp();

    assert!(rx.is_empty());
    assert!(s.active().lsp_dirty);
}

#[test]
fn sync_lsp_opens_then_changes_the_same_document() {
    // The `didOpen`/`didChange` split: the first sync announces the document (with
    // the languageId its extension maps to), every later one is a change against
    // that same identity. Sending clears the dirty flag so an idle loop turn does
    // not re-send unchanged text.
    let (tx, rx) = async_channel::bounded::<DocumentSync>(4);
    let mut s = session_with("fn main() {}", SelectionSet::at_origin());
    s.lsp = Some(LspConnection::new(tx));
    s.active_mut().path = Some(PathBuf::from("/tmp/x.rs"));
    s.active_mut().lsp_dirty = true;

    s.sync_lsp();

    match rx.try_recv().expect("a didOpen was sent") {
        DocumentSync::Opened {
            path,
            language_id,
            version,
            ..
        } => {
            assert_eq!(path, PathBuf::from("/tmp/x.rs"));
            assert_eq!(language_id, "rust");
            assert_eq!(version, 0);
        }
        other => panic!("expected Opened, got {other:?}"),
    }
    assert!(!s.active().lsp_dirty);
    let id = s.active().id;
    assert!(s.lsp.as_ref().unwrap().opened.contains(&id));

    // Nothing outstanding: a second call is a no-op rather than a redundant resend.
    s.sync_lsp();
    assert!(rx.is_empty());

    // Once the buffer changes again the server gets a change, not another open.
    s.active_mut().lsp_dirty = true;
    s.sync_lsp();
    assert!(matches!(
        rx.try_recv().expect("a didChange was sent"),
        DocumentSync::Changed { .. }
    ));
}

#[test]
fn sync_syntax_sends_the_active_text_once_per_change() {
    // The highlighter has no open/change split - every message is the full text,
    // tagged with the version it belongs to so a stale batch can be recognized on
    // the way back (SPEC §5).
    let (tx, rx) = async_channel::bounded::<SyntaxSync>(4);
    let mut s = session_with("let x = 1;", SelectionSet::at_origin());
    s.syntax_sync = Some(tx);
    s.active_mut().syntax_dirty = true;

    s.sync_syntax();

    let sent = rx.try_recv().expect("the buffer was sent for a parse");
    assert_eq!(sent.text.to_string(), "let x = 1;");
    assert_eq!(sent.version, 0);
    assert!(!s.active().syntax_dirty);

    // Clean again: no resend until something marks the buffer dirty.
    s.sync_syntax();
    assert!(rx.is_empty());
}

#[test]
fn producer_sync_is_a_noop_with_nothing_attached() {
    // The common case - no server, no highlighter. A dirty buffer must not panic or
    // spin; the flags simply stay set until a producer attaches.
    let mut s = session_with("text", SelectionSet::at_origin());
    s.active_mut().lsp_dirty = true;
    s.active_mut().syntax_dirty = true;

    s.sync_lsp();
    s.sync_syntax();

    assert!(s.active().lsp_dirty);
    assert!(s.active().syntax_dirty);
}

#[test]
fn place_cursor_at_resolves_a_position_against_the_buffer_it_lands_in() {
    // The point of carrying a position rather than an offset: the sender named a
    // line in a file, and the core converts it against the document it has - here
    // one whose earlier lines are wide enough that a byte offset guessed frontend-
    // side from a line number would be nowhere near right.
    let (snap, _) = run_seam(&[
        Action::Insert("αβγ\nsecond line\nthird\n".into()),
        Action::PlaceCursorAt {
            position: Position::new(1, 7),
        },
    ]);
    let caret = snap.selections[0].head;
    assert_eq!(&snap.text.to_string()[caret..caret + 4], "line");
}

#[test]
fn place_cursor_at_collapses_to_one_cursor() {
    // A jump is an arrival, not an addition: whatever set was there is replaced,
    // the same way a click does.
    let (snap, _) = run_seam(&[
        Action::Insert("one\ntwo\nthree\n".into()),
        Action::AddCursorAbove,
        Action::PlaceCursorAt {
            position: Position::new(0, 1),
        },
    ]);
    assert_eq!(
        snap.selections.as_ref(),
        &[Selection::cursor(1)],
        "a jump arrives as one plain cursor, replacing whatever set was there"
    );
}

#[test]
fn place_cursor_at_clamps_a_position_the_buffer_no_longer_has() {
    // A hit found against the file on disk can name a line the buffer has since
    // lost. Landing at the end beats refusing to jump (SPEC §8) - and must not
    // panic on the way, which is the part that matters.
    let text = "one\ntwo\n";
    for position in [
        Position::new(99, 0),  // past the last line
        Position::new(1, 99),  // past that line's end
        Position::new(99, 99), // both
    ] {
        let (snap, _) = run_seam(&[
            Action::Insert(text.into()),
            Action::PlaceCursorAt { position },
        ]);
        let caret = snap.selections[0].head;
        assert!(
            caret <= text.len(),
            "{position:?} resolved to {caret}, past the buffer"
        );
    }
}

#[test]
fn place_cursor_at_the_start_of_an_empty_buffer_is_harmless() {
    // The degenerate case the clamp's `saturating_sub` exists for.
    let (snap, _) = run_seam(&[Action::PlaceCursorAt {
        position: Position::new(0, 0),
    }]);
    assert_eq!(snap.selections[0].head, 0);
}
