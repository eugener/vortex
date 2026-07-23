use super::*;
use std::future::Future;

/// Drive the core the way §13 interaction tests do: send actions, assert on
/// emitted snapshots/notifications. No terminal, no PTY. The test owns the
/// executor and spawns the actor loop concurrently, exactly as a frontend
/// would - the test body runs as a future on the same executor so the actor
/// makes progress while the body awaits channel ops.
fn drive<F, Fut, T>(f: F) -> T
where
    F: FnOnce(CoreHandle) -> Fut,
    Fut: Future<Output = T>,
{
    let ex = smol::Executor::new();
    let Core { handle, run } = new(16);
    ex.spawn(run).detach();
    smol::block_on(ex.run(f(handle)))
}

/// Send an action and await the resulting snapshot. Edits also emit deltas on
/// a separate channel; drain any pending ones so the bounded delta channel
/// never fills across a long script (and so tests can inspect them).
async fn step(h: &CoreHandle, action: Action) -> ViewSnapshot {
    h.actions.send(action).await.unwrap();
    h.snapshots.recv().await.unwrap()
}

#[test]
fn request_snapshot_round_trips() {
    drive(|h| async move {
        let snap = step(&h, Action::RequestSnapshot).await;
        assert_eq!(snap.buffer_id, BufferId(0));
        // No edits yet, so the document version is its initial 0.
        assert_eq!(snap.version, 0);
        assert!(snap.text.is_empty());
        // A fresh buffer holds a single cursor at the origin (SPEC §2.2).
        assert_eq!(snap.selections.as_ref(), &[Selection::cursor(0)]);
        // The primary index is valid and points at that sole selection.
        assert_eq!(snap.primary, 0);
    });
}

#[test]
fn snapshot_version_is_stable_without_edits() {
    // `version` is the document version (SPEC §2.1, §5): it advances on edits,
    // NOT on snapshot requests. Repeated requests must report the same version
    // - otherwise anchors/LSP keyed on it would desync from actual edits.
    drive(|h| async move {
        let first = step(&h, Action::RequestSnapshot).await;
        let second = step(&h, Action::RequestSnapshot).await;
        assert_eq!(first.version, 0);
        assert_eq!(second.version, first.version);
    });
}

#[test]
fn insert_updates_text_and_advances_version() {
    drive(|h| async move {
        let snap = step(&h, Action::Insert("hello".into())).await;
        assert_eq!(snap.text.to_string(), "hello");
        assert_eq!(snap.version, 1);
        // Cursor sits after the inserted text.
        assert_eq!(snap.selections.as_ref(), &[Selection::cursor(5)]);
    });
}

#[test]
fn insert_emits_matching_delta() {
    drive(|h| async move {
        h.actions.send(Action::Insert("hi".into())).await.unwrap();
        let delta = h.deltas.recv().await.unwrap();
        assert_eq!(delta.base_version, 0);
        assert_eq!(delta.range, 0..0);
        assert_eq!(delta.new_text, "hi");
        let snap = h.snapshots.recv().await.unwrap();
        assert_eq!(snap.version, 1);
        assert_eq!(snap.dirty, Some(0..2));
    });
}

#[test]
fn sequential_inserts_accumulate() {
    drive(|h| async move {
        step(&h, Action::Insert("ab".into())).await;
        let snap = step(&h, Action::Insert("cd".into())).await;
        assert_eq!(snap.text.to_string(), "abcd");
        assert_eq!(snap.version, 2);
        assert_eq!(snap.selections.as_ref(), &[Selection::cursor(4)]);
    });
}

#[test]
fn delete_backward_removes_prior_grapheme() {
    drive(|h| async move {
        step(&h, Action::Insert("héllo".into())).await; // é is 2 bytes
        // Cursor at end (byte 6). Backspace deletes 'o'.
        let snap = step(&h, Action::DeleteBackward).await;
        assert_eq!(snap.text.to_string(), "héll");
    });
}

#[test]
fn delete_backward_at_start_is_noop() {
    drive(|h| async move {
        // No edit yet: cursor at 0, backspace does nothing, version unchanged.
        let snap = step(&h, Action::DeleteBackward).await;
        assert!(snap.text.is_empty());
        assert_eq!(snap.version, 0);
    });
}

#[test]
fn delete_forward_removes_next_grapheme() {
    drive(|h| async move {
        step(&h, Action::Insert("abc".into())).await;
        step(
            &h,
            Action::MoveCursor {
                motion: Motion::BufferStart,
                extend: false,
            },
        )
        .await;
        let snap = step(&h, Action::DeleteForward).await;
        assert_eq!(snap.text.to_string(), "bc");
    });
}

#[test]
fn move_cursor_does_not_change_version_or_text() {
    drive(|h| async move {
        step(&h, Action::Insert("abc".into())).await; // version 1
        let snap = step(
            &h,
            Action::MoveCursor {
                motion: Motion::Left,
                extend: false,
            },
        )
        .await;
        assert_eq!(snap.version, 1); // motion is not an edit
        assert_eq!(snap.text.to_string(), "abc");
        assert_eq!(snap.selections.as_ref(), &[Selection::cursor(2)]);
    });
}

#[test]
fn insert_replaces_non_empty_selection() {
    drive(|h| async move {
        step(&h, Action::Insert("hello".into())).await;
        step(
            &h,
            Action::MoveCursor {
                motion: Motion::BufferStart,
                extend: false,
            },
        )
        .await;
        // Select "hel" by extending right thrice.
        for _ in 0..3 {
            step(
                &h,
                Action::MoveCursor {
                    motion: Motion::Right,
                    extend: true,
                },
            )
            .await;
        }
        let snap = step(&h, Action::Insert("X".into())).await;
        assert_eq!(snap.text.to_string(), "Xlo");
    });
}

#[test]
fn undo_reverts_an_insert_and_restores_the_cursor() {
    drive(|h| async move {
        step(&h, Action::Insert("hello".into())).await;
        let snap = step(&h, Action::Undo).await;
        assert_eq!(snap.text.to_string(), "");
        // Undo is an edit on the wire: it bumps the version (Insert=1, Undo=2).
        assert_eq!(snap.version, 2);
        // The caret returns to where it was before the insert (buffer start).
        assert_eq!(snap.selections.as_ref(), &[Selection::cursor(0)]);
    });
}

#[test]
fn undo_emits_a_delta_that_inverts_the_edit() {
    drive(|h| async move {
        h.actions.send(Action::Insert("hi".into())).await.unwrap();
        let insert_delta = h.deltas.recv().await.unwrap();
        assert_eq!(
            (insert_delta.range.clone(), insert_delta.new_text.as_str()),
            (0..0, "hi")
        );
        h.snapshots.recv().await.unwrap();

        h.actions.send(Action::Undo).await.unwrap();
        let undo_delta = h.deltas.recv().await.unwrap();
        // The undo delta deletes the inserted "hi" span (0..2 -> "").
        assert_eq!(undo_delta.range, 0..2);
        assert_eq!(undo_delta.new_text, "");
        assert_eq!(undo_delta.base_version, 1);
    });
}

#[test]
fn redo_reapplies_an_undone_edit() {
    drive(|h| async move {
        step(&h, Action::Insert("hi".into())).await;
        step(&h, Action::Undo).await;
        let snap = step(&h, Action::Redo).await;
        assert_eq!(snap.text.to_string(), "hi");
        // Caret restored to the post-edit position (past the reinserted text).
        assert_eq!(snap.selections.as_ref(), &[Selection::cursor(2)]);
    });
}

#[test]
fn consecutive_typed_characters_undo_as_one_unit() {
    // Three single-character inserts with no motion between them coalesce into one
    // undo unit (SPEC §2.4), so a single Undo clears the whole run - the behavior
    // that makes undo usable instead of one-char-at-a-time.
    drive(|h| async move {
        step(&h, Action::Insert("a".into())).await;
        step(&h, Action::Insert("b".into())).await;
        step(&h, Action::Insert("c".into())).await;
        let snap = step(&h, Action::Undo).await;
        assert_eq!(snap.text.to_string(), "");
    });
}

#[test]
fn a_cursor_motion_breaks_the_undo_coalescing_run() {
    // A motion between two inserts starts a new undo unit, so Undo peels back only
    // the second insert (SPEC §2.4 break rule (d)).
    drive(|h| async move {
        step(&h, Action::Insert("a".into())).await;
        step(
            &h,
            Action::MoveCursor {
                motion: Motion::Left,
                extend: false,
            },
        )
        .await;
        // Caret now at 0; typing inserts before "a".
        step(&h, Action::Insert("b".into())).await;
        let snap = step(&h, Action::Undo).await;
        assert_eq!(
            snap.text.to_string(),
            "a",
            "only the post-motion insert is undone"
        );
    });
}

#[test]
fn a_newline_insert_breaks_the_undo_coalescing_run() {
    // Pressing Enter is its own undo unit (break rule (c)): Undo removes the text
    // typed after the newline without swallowing the line break too.
    drive(|h| async move {
        step(&h, Action::Insert("a".into())).await;
        step(&h, Action::Insert("\n".into())).await;
        step(&h, Action::Insert("b".into())).await;
        let snap = step(&h, Action::Undo).await;
        assert_eq!(snap.text.to_string(), "a\n");
    });
}

#[test]
fn a_delete_undoes_independently_of_a_prior_insert() {
    // Insert then delete: each is its own undo unit. Undo restores the deleted
    // grapheme; a second undo removes the insert. Works because history records
    // buffer changes, not action kinds - so delete is undoable with no delete-
    // specific code.
    drive(|h| async move {
        step(&h, Action::Insert("hello".into())).await; // one Insert action = one unit
        step(&h, Action::DeleteBackward).await; // "hell"
        let after_first = step(&h, Action::Undo).await;
        assert_eq!(after_first.text.to_string(), "hello", "delete undone");
        let after_second = step(&h, Action::Undo).await;
        assert_eq!(after_second.text.to_string(), "", "insert undone");
    });
}

#[test]
fn undo_at_the_root_is_a_no_op() {
    // Nothing to undo on a fresh buffer: state is unchanged and the version does
    // not advance (no delta was emitted, SPEC §5 invariant).
    drive(|h| async move {
        let snap = step(&h, Action::Undo).await;
        assert_eq!(snap.text.to_string(), "");
        assert_eq!(snap.version, 0);
    });
}

#[test]
fn redo_with_nothing_to_redo_is_a_no_op() {
    drive(|h| async move {
        step(&h, Action::Insert("x".into())).await; // version 1
        let snap = step(&h, Action::Redo).await; // nothing undone, so nothing to redo
        assert_eq!(snap.text.to_string(), "x");
        assert_eq!(snap.version, 1, "a no-op redo does not bump the version");
    });
}

#[test]
fn typing_after_undo_redoes_onto_the_new_branch() {
    // Undo then type: the old redo branch is preserved but redo follows the newest
    // branch (SPEC §2.4 tree). Type "a", undo, type "b": redo after undoing "b"
    // must restore "b", not the discarded "a".
    drive(|h| async move {
        step(&h, Action::Insert("a".into())).await;
        step(&h, Action::Undo).await; // back to empty
        step(&h, Action::Insert("b".into())).await; // new branch
        step(&h, Action::Undo).await; // back to empty
        let snap = step(&h, Action::Redo).await;
        assert_eq!(snap.text.to_string(), "b", "redo takes the newest branch");
    });
}

#[test]
#[should_panic(expected = "action_capacity must be >= 1")]
fn new_rejects_zero_capacity() {
    // A bounded channel needs capacity >= 1; guard it at our API boundary
    // rather than letting async-channel panic with a less clear message.
    let _ = new(0);
}

#[test]
fn quit_shuts_down_and_notifies() {
    drive(|h| async move {
        h.actions.send(Action::Quit).await.unwrap();
        assert_eq!(
            h.notifications.recv().await.unwrap(),
            Notification::ShuttingDown
        );
        // After shutdown the snapshot channel is closed.
        assert!(h.snapshots.recv().await.is_err());
    });
}

#[test]
fn snapshot_send_failure_stops_the_actor() {
    // If the frontend drops the snapshot receiver, a RequestSnapshot can no
    // longer be delivered; the actor detects the closed channel and shuts
    // down cleanly rather than looping.
    drive(|h| async move {
        let CoreHandle {
            actions,
            snapshots,
            notifications,
            ..
        } = h;
        drop(snapshots);
        actions.send(Action::RequestSnapshot).await.unwrap();
        assert_eq!(
            notifications.recv().await.unwrap(),
            Notification::ShuttingDown
        );
    });
}

#[test]
fn dropping_frontend_stops_the_actor() {
    // If the action sender is dropped, the actor's recv errors and it stops
    // cleanly, emitting ShuttingDown (best-effort) before the channels close.
    drive(|h| async move {
        let CoreHandle {
            actions,
            notifications,
            ..
        } = h;
        drop(actions);
        assert_eq!(
            notifications.recv().await.unwrap(),
            Notification::ShuttingDown
        );
    });
}

#[test]
fn edit_after_snapshot_receiver_dropped_stops_the_actor() {
    // Dropping the snapshot cell means an edit's snapshot can't be delivered;
    // the actor detects the closed slot and shuts down cleanly rather than
    // looping (covers the edit-action break arms).
    drive(|h| async move {
        let CoreHandle {
            actions,
            snapshots,
            notifications,
            ..
        } = h;
        drop(snapshots);
        actions.send(Action::Insert("x".into())).await.unwrap();
        assert_eq!(
            notifications.recv().await.unwrap(),
            Notification::ShuttingDown
        );
    });
}

#[test]
fn edit_after_delta_receiver_dropped_stops_the_actor() {
    // Dropping the delta channel means an edit's delta can't be sent; the
    // actor treats the closed lossless channel as "frontend gone" and stops.
    drive(|h| async move {
        let CoreHandle {
            actions,
            deltas,
            notifications,
            ..
        } = h;
        drop(deltas);
        actions.send(Action::Insert("x".into())).await.unwrap();
        assert_eq!(
            notifications.recv().await.unwrap(),
            Notification::ShuttingDown
        );
    });
}

#[test]
fn snapshot_cell_try_recv_reads_latest_then_empties() {
    // The latest-wins cell: after an edit a snapshot is buffered and
    // `try_recv` returns it without awaiting; a second `try_recv` is empty
    // until the next publish (the frontend then paints its last-held frame).
    drive(|h| async move {
        h.actions.send(Action::Insert("hi".into())).await.unwrap();
        // The delta is emitted before the snapshot; drain it so the actor
        // proceeds to publish.
        let _ = h.deltas.recv().await.unwrap();
        // Await once to be sure the snapshot has been published, then confirm
        // the cell is drained.
        let snap = h.snapshots.recv().await.unwrap();
        assert_eq!(snap.text.to_string(), "hi");
        assert!(h.snapshots.try_recv().is_none());
    });
}

// --- Clipboard through the message seam (SPEC §11) -------------------------

/// Select the first `n` graphemes from the buffer start (BufferStart, then extend
/// Right `n` times). Leaves a non-empty primary selection for a copy/cut to read.
async fn select_first(h: &CoreHandle, n: usize) {
    step(
        h,
        Action::MoveCursor {
            motion: Motion::BufferStart,
            extend: false,
        },
    )
    .await;
    for _ in 0..n {
        step(
            h,
            Action::MoveCursor {
                motion: Motion::Right,
                extend: true,
            },
        )
        .await;
    }
}

#[test]
fn copy_emits_set_clipboard_and_does_not_change_the_buffer() {
    // Copy is a pure register write: it emits a SetClipboard notification with the
    // selected text but leaves the buffer and version untouched (SPEC §11).
    drive(|h| async move {
        step(&h, Action::Insert("hello".into())).await;
        select_first(&h, 3).await; // select "hel"
        let snap = step(&h, Action::Copy).await;
        assert_eq!(snap.text.to_string(), "hello"); // unchanged
        assert_eq!(snap.version, 1); // only the insert bumped it
        match h.notifications.try_recv() {
            Ok(Notification::SetClipboard { text }) => assert_eq!(text, "hel"),
            other => panic!("expected SetClipboard, got {other:?}"),
        }
    });
}

#[test]
fn copy_with_no_selection_emits_no_clipboard_notification() {
    // A bare cursor selects nothing: copy is a no-op and must not emit SetClipboard.
    drive(|h| async move {
        step(&h, Action::Insert("hello".into())).await;
        step(&h, Action::Copy).await;
        assert!(h.notifications.try_recv().is_err()); // nothing emitted
    });
}

#[test]
fn copy_then_paste_round_trips_through_the_seam() {
    // Copy "hel", move to end, paste: the register text lands at the caret. Drives
    // the full Copy + Paste actor-loop path end to end (SPEC §11).
    drive(|h| async move {
        step(&h, Action::Insert("hello".into())).await;
        select_first(&h, 3).await; // select "hel"
        step(&h, Action::Copy).await;
        step(
            &h,
            Action::MoveCursor {
                motion: Motion::BufferEnd,
                extend: false,
            },
        )
        .await;
        let snap = step(&h, Action::Paste).await;
        assert_eq!(snap.text.to_string(), "hellohel");
    });
}

#[test]
fn cut_removes_the_selection_and_fills_the_clipboard() {
    // Cut is copy + delete as one edit: the selection is removed, SetClipboard
    // carries the cut text, and the version bumps once for the deletion (SPEC §11).
    drive(|h| async move {
        step(&h, Action::Insert("hello".into())).await;
        select_first(&h, 3).await; // select "hel"
        h.actions.send(Action::Cut).await.unwrap();
        // Cut emits its delete delta before the snapshot; drain it so the actor
        // proceeds to publish.
        let _ = h.deltas.recv().await.unwrap();
        let snap = h.snapshots.recv().await.unwrap();
        assert_eq!(snap.text.to_string(), "lo");
        match h.notifications.try_recv() {
            Ok(Notification::SetClipboard { text }) => assert_eq!(text, "hel"),
            other => panic!("expected SetClipboard, got {other:?}"),
        }
    });
}

#[test]
fn paste_with_empty_register_is_a_noop() {
    // Nothing copied yet: paste plans no edits, so the buffer and version are
    // unchanged (SPEC §11 empty-register rule).
    drive(|h| async move {
        step(&h, Action::Insert("hi".into())).await;
        let snap = step(&h, Action::Paste).await;
        assert_eq!(snap.text.to_string(), "hi");
        assert_eq!(snap.version, 1); // only the insert
    });
}

#[test]
fn cut_with_no_selection_changes_nothing() {
    // Cut over a bare cursor selects nothing: no register write, no delete, no
    // clipboard notification, no version bump.
    drive(|h| async move {
        step(&h, Action::Insert("hi".into())).await;
        let snap = step(&h, Action::Cut).await;
        assert_eq!(snap.text.to_string(), "hi");
        assert_eq!(snap.version, 1);
        assert!(h.notifications.try_recv().is_err());
    });
}

#[test]
fn paste_is_its_own_undo_unit_not_merged_into_prior_typing() {
    // Regression (undo-coalescing bug): a single-char paste right after typing must
    // NOT fold into the typing run. Put "X" in the register, type "a", paste "X":
    // one Undo removes only the paste, leaving "a" - never both (SPEC §2.4: a paste
    // is one distinct action, not part of the typing run).
    drive(|h| async move {
        // Load the register with a single char: insert "X", select it, copy.
        step(&h, Action::Insert("X".into())).await;
        select_first(&h, 1).await;
        step(&h, Action::Copy).await;
        // Reset to an empty buffer with a fresh caret: undo the insert, so the
        // typing run below starts clean at the origin.
        step(&h, Action::Undo).await;
        assert!(step(&h, Action::RequestSnapshot).await.text.is_empty());

        step(&h, Action::Insert("a".into())).await; // opens a coalescing run
        let pasted = step(&h, Action::Paste).await;
        assert_eq!(pasted.text.to_string(), "aX");
        // One Undo peels off only the paste.
        let after = step(&h, Action::Undo).await;
        assert_eq!(
            after.text.to_string(),
            "a",
            "paste must be its own undo unit"
        );
    });
}

#[test]
fn multi_char_insert_is_its_own_undo_unit() {
    // Regression (bracketed-paste bug): Event::Paste maps to one Action::Insert of
    // the whole payload; a multi-character insert must be its own undo unit even
    // with no newline, so it does not coalesce with adjacent typing. Type "a", then
    // insert "hello" (the bracketed-paste shape); one Undo removes only "hello".
    drive(|h| async move {
        step(&h, Action::Insert("a".into())).await;
        step(&h, Action::Insert("hello".into())).await;
        let after = step(&h, Action::Undo).await;
        assert_eq!(
            after.text.to_string(),
            "a",
            "a multi-char insert must not merge with prior typing"
        );
    });
}

#[test]
fn typing_after_a_paste_starts_a_fresh_undo_unit() {
    // The reverse coupling: after a paste closes the run, the next typed character
    // must open a NEW unit, not extend the paste. Type "a", paste "hello", type "b":
    // one Undo removes only "b", leaving "ahello".
    drive(|h| async move {
        step(&h, Action::Insert("a".into())).await;
        step(&h, Action::Insert("hello".into())).await; // paste-shaped, closes the run
        step(&h, Action::Insert("b".into())).await; // must be its own unit
        let after = step(&h, Action::Undo).await;
        assert_eq!(
            after.text.to_string(),
            "ahello",
            "typing after a paste must not extend the paste's undo unit"
        );
    });
}

// --- LSP integration (SPEC §3, §4, §5; M2) ---
//
// Drives the real actor loop against a *fake* server on the same channels the
// real client uses, so document sync and diagnostics are tested end to end
// without a subprocess. The real `rust-analyzer` path is covered separately by
// the `lsp_rust_analyzer` integration test.

use async_channel::{Receiver, Sender};

use crate::decoration::{GutterKind, Severity};
use crate::lsp::LspHandle;

/// The fixture the M2 spike fed rust-analyzer: byte / char / UTF-16 columns of
/// the trailing `msg` are 32 / 23 / 24, so only a correct UTF-16 reading lands
/// on it.
const FIXTURE: &str = "pub fn bad() -> i32 {\n    let msg = \"日本語 😀\"; msg\n}\n";

/// The server side of the seam: what the editor sent us, and a way to push
/// events back.
struct FakeServer {
    sync: Receiver<crate::lsp::DocumentSync>,
    events: Sender<crate::lsp::LspEvent>,
}

/// Like [`drive`], but with a language server attached.
fn drive_lsp<F, Fut, T>(f: F) -> T
where
    F: FnOnce(CoreHandle, FakeServer) -> Fut,
    Fut: Future<Output = T>,
{
    let ex = smol::Executor::new();
    let (sync_tx, sync_rx) = async_channel::bounded(16);
    let (event_tx, event_rx) = async_channel::bounded(16);
    let Core { handle, run } = with_lsp(
        16,
        LspHandle {
            sync: sync_tx,
            events: event_rx,
        },
    );
    ex.spawn(run).detach();
    smol::block_on(ex.run(f(
        handle,
        FakeServer {
            sync: sync_rx,
            events: event_tx,
        },
    )))
}

/// A diagnostic over `start..end` in UTF-16 space on `line`.
fn diag(line: usize, start: usize, end: usize, severity: Severity) -> Diagnostic {
    Diagnostic {
        start: Utf16Position::new(line, start),
        end: Utf16Position::new(line, end),
        severity,
        message: "mismatched types".into(),
    }
}

/// Write `FIXTURE` to a temp file, open it in the core, and drain the resulting
/// snapshot. Returns the path.
async fn open_fixture(h: &CoreHandle, dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("lib.rs");
    std::fs::write(&path, FIXTURE).unwrap();
    step(h, Action::Open(path.clone())).await;
    path
}

/// A temp dir removed on drop, mirroring `editor_tests`' helper.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("vortex-lsp-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn opening_a_file_announces_it_to_the_server() {
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        // Whole-capture the fake server (see `an_edit_sends...` for why): this test
        // reads only its sync side.
        let FakeServer {
            sync,
            events: _events,
        } = server;
        let path = open_fixture(&h, &dir.0).await;
        match sync.recv().await.unwrap() {
            crate::lsp::DocumentSync::Opened {
                path: p,
                language_id,
                text,
            } => {
                assert_eq!(p, path);
                // The LSP identifier, not the file extension.
                assert_eq!(language_id, "rust");
                assert_eq!(text, FIXTURE);
            }
            other => panic!("expected didOpen, got {other:?}"),
        }
    });
}

#[test]
fn an_edit_sends_the_whole_document_as_a_change() {
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        // Read only the sync side, but keep the whole fake server alive: Rust 2021
        // disjoint capture would otherwise drop the unused `server.events`, closing
        // that channel, which the core correctly treats as the server dying.
        let FakeServer {
            sync,
            events: _events,
        } = server;
        open_fixture(&h, &dir.0).await;
        sync.recv().await.unwrap(); // the didOpen

        step(&h, Action::Insert("x".into())).await;
        match sync.recv().await.unwrap() {
            crate::lsp::DocumentSync::Changed { version, text, .. } => {
                // Full-text sync (SPEC §5): the entire buffer, not a delta.
                assert_eq!(text, format!("x{FIXTURE}"));
                // The load itself is version 1 (one whole-buffer delta), so the
                // first edit after it is version 2.
                assert_eq!(version, 2, "the change carries the new buffer version");
            }
            other => panic!("expected didChange, got {other:?}"),
        }
    });
}

#[test]
fn a_diagnostic_underlines_the_right_span_end_to_end() {
    // M2's acceptance criterion, driven through the actor loop with the exact
    // positions rust-analyzer produced for this fixture.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        let path = open_fixture(&h, &dir.0).await;
        server
            .events
            .send(LspEvent::Diagnostics {
                path,
                diagnostics: vec![diag(1, 24, 27, Severity::Error)],
            })
            .await
            .unwrap();

        let snap = h.snapshots.recv().await.unwrap();
        let underlines: Vec<_> = snap
            .decorations
            .underlines_in(0..snap.text.byte_len())
            .collect();
        assert_eq!(underlines.len(), 1);
        let (range, severity) = underlines.into_iter().next().unwrap();
        assert_eq!(severity, Severity::Error);
        assert_eq!(
            snap.text.slice(range),
            "msg",
            "the underline must cover exactly the flagged identifier"
        );
        // ...and the gutter is marked on that line.
        assert_eq!(
            snap.decorations.gutter_mark(&snap.text, 1),
            Some(GutterKind::Diagnostic(Severity::Error))
        );
    });
}

#[test]
fn diagnostics_for_another_file_are_ignored() {
    // A server analyzes the whole workspace and publishes for files the editor is
    // not showing; those must not decorate the open buffer.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        open_fixture(&h, &dir.0).await;
        server
            .events
            .send(LspEvent::Diagnostics {
                path: dir.0.join("other.rs"),
                diagnostics: vec![diag(1, 24, 27, Severity::Error)],
            })
            .await
            .unwrap();

        // No snapshot should be published for an ignored batch, so a following
        // action's snapshot is the next thing to arrive - and it is clean.
        let snap = step(&h, Action::RequestSnapshot).await;
        assert!(snap.decorations.is_empty());
    });
}

#[test]
fn an_empty_batch_clears_the_squiggles() {
    // publishDiagnostics with an empty list is how a server says "this file is
    // clean now" - the fix must actually remove the underline.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        let path = open_fixture(&h, &dir.0).await;
        server
            .events
            .send(LspEvent::Diagnostics {
                path: path.clone(),
                diagnostics: vec![diag(1, 24, 27, Severity::Error)],
            })
            .await
            .unwrap();
        assert!(!h.snapshots.recv().await.unwrap().decorations.is_empty());

        server
            .events
            .send(LspEvent::Diagnostics {
                path,
                diagnostics: vec![],
            })
            .await
            .unwrap();
        assert!(h.snapshots.recv().await.unwrap().decorations.is_empty());
    });
}

#[test]
fn typing_before_a_diagnostic_shifts_its_underline() {
    // Decorations ride edits (SPEC §5) so the squiggle stays on the token it
    // flagged while the server catches up.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        let path = open_fixture(&h, &dir.0).await;
        server
            .events
            .send(LspEvent::Diagnostics {
                path,
                diagnostics: vec![diag(1, 24, 27, Severity::Error)],
            })
            .await
            .unwrap();
        h.snapshots.recv().await.unwrap();

        // Insert at the very start of the buffer, before the flagged span.
        let snap = step(&h, Action::Insert("//\n".into())).await;
        let (range, _) = snap
            .decorations
            .underlines_in(0..snap.text.byte_len())
            .next()
            .expect("the underline survives the edit");
        assert_eq!(
            snap.text.slice(range),
            "msg",
            "the underline must still cover the identifier after the shift"
        );
    });
}

#[test]
fn opening_another_file_clears_decorations_and_reannounces() {
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        let path = open_fixture(&h, &dir.0).await;
        server.sync.recv().await.unwrap(); // didOpen for the first file
        server
            .events
            .send(LspEvent::Diagnostics {
                path,
                diagnostics: vec![diag(1, 24, 27, Severity::Error)],
            })
            .await
            .unwrap();
        assert!(!h.snapshots.recv().await.unwrap().decorations.is_empty());

        let other = dir.0.join("other.rs");
        std::fs::write(&other, "fn main() {}\n").unwrap();
        let snap = step(&h, Action::Open(other.clone())).await;
        assert!(
            snap.decorations.is_empty(),
            "the previous file's squiggles describe text that is no longer open"
        );
        // The new file is announced as a fresh document, not as a change to the
        // old one's identity.
        match server.sync.recv().await.unwrap() {
            crate::lsp::DocumentSync::Opened { path: p, .. } => assert_eq!(p, other),
            other => panic!("expected a fresh didOpen, got {other:?}"),
        }
    });
}

#[test]
fn the_editor_survives_the_language_server_dying() {
    // A crashed server must degrade to "no diagnostics", never take the editor
    // down (SPEC §8) - and must not spin the actor loop on its closed channel.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        open_fixture(&h, &dir.0).await;
        drop(server); // the client task is gone

        let snap = step(&h, Action::Insert("still alive".into())).await;
        assert!(snap.text.to_string().starts_with("still alive"));
        let snap = step(&h, Action::Insert("!".into())).await;
        assert!(snap.text.to_string().starts_with("still alive!"));
    });
}

#[test]
fn a_repeated_identical_batch_does_not_republish() {
    // Servers re-send the same diagnostics while indexing; an unchanged screen
    // must not cost a frame.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        let path = open_fixture(&h, &dir.0).await;
        let batch = vec![diag(1, 24, 27, Severity::Error)];
        server
            .events
            .send(LspEvent::Diagnostics {
                path: path.clone(),
                diagnostics: batch.clone(),
            })
            .await
            .unwrap();
        h.snapshots.recv().await.unwrap();

        server
            .events
            .send(LspEvent::Diagnostics {
                path,
                diagnostics: batch,
            })
            .await
            .unwrap();
        // If the duplicate had republished, this snapshot would be that one
        // rather than the RequestSnapshot's - assert by checking the version is
        // still the post-open version and the cell held nothing stale.
        let snap = step(&h, Action::RequestSnapshot).await;
        assert!(!snap.decorations.is_empty());
        assert!(h.snapshots.try_recv().is_none(), "no extra snapshot queued");
    });
}

#[test]
fn the_core_stops_when_the_frontend_hangs_up_with_a_server_attached() {
    // The shutdown path must work identically whether or not an LSP is wired in:
    // dropping the frontend's handle ends the actor rather than leaving it parked
    // on the server's channel forever.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        open_fixture(&h, &dir.0).await;
        let notifications = h.notifications.clone();
        drop(h); // the frontend is gone

        // The actor drains and emits its final notification (SPEC §6).
        loop {
            match notifications.recv().await {
                Ok(Notification::ShuttingDown) => break,
                Ok(_) => continue,
                Err(_) => panic!("core stopped without announcing shutdown"),
            }
        }
        drop(server);
    });
}

#[test]
fn a_second_file_opened_into_an_attached_server_is_announced() {
    // The picker-open case: with a server already attached, opening another file
    // (same workspace) must announce it - a didOpen for the new path - so it is
    // analyzed too, without re-attaching a server.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        // Whole-capture the fake server (see `an_edit_sends...`): sync side only.
        let FakeServer {
            sync,
            events: _events,
        } = server;
        open_fixture(&h, &dir.0).await;
        // Drain the first file's didOpen.
        assert!(matches!(
            sync.recv().await.unwrap(),
            crate::lsp::DocumentSync::Opened { .. }
        ));

        // Open a second file (as the picker would).
        let other = dir.0.join("other.rs");
        std::fs::write(&other, "fn main() {}\n").unwrap();
        step(&h, Action::Open(other.clone())).await;

        // The already-attached server is told about it as a fresh document.
        match sync.recv().await.unwrap() {
            crate::lsp::DocumentSync::Opened { path, .. } => assert_eq!(path, other),
            other => panic!("expected a didOpen for the second file, got {other:?}"),
        }
    });
}

// --- M4: the syntax highlighter as a second decoration producer -------------
//
// These drive the editor through the *real* attach seam (`CoreHandle::syntax`),
// with a fake highlighter standing in for the tree-sitter loop - the exact twin
// of the `FakeServer` LSP tests above. The engine's own parsing is covered in
// `syntax::engine`; here we prove the editor wires a highlighter's output onto the
// snapshot's decoration channel and survives it attaching, closing, and repeating.

/// The highlighter side of the seam: the text the editor sent us, and a way to
/// push highlight batches back.
struct FakeHighlighter {
    /// editor -> us: the buffer to reparse.
    sync: async_channel::Receiver<SyntaxSync>,
    /// us -> editor: highlight batches.
    events: async_channel::Sender<SyntaxEvent>,
}

/// Attach a fake highlighter over `CoreHandle::syntax`, as a frontend would after
/// loading a grammar.
async fn attach_syntax(h: &CoreHandle) -> FakeHighlighter {
    let (sync_tx, sync_rx) = async_channel::bounded(16);
    let (event_tx, event_rx) = async_channel::bounded(16);
    h.syntax
        .send(SyntaxHandle {
            sync: sync_tx,
            events: event_rx,
        })
        .await
        .unwrap();
    FakeHighlighter {
        sync: sync_rx,
        events: event_tx,
    }
}

#[test]
fn attaching_a_highlighter_announces_the_current_buffer() {
    // Attach re-announces the buffer (a first parse), exactly as an LSP attach
    // re-sends a didOpen: text typed before the highlighter arrived is still
    // highlighted.
    drive(|h| async move {
        step(&h, Action::Insert("fn f() {}".into())).await;
        let fake = attach_syntax(&h).await;
        // The editor flushes the current buffer to the highlighter on its next turn.
        // Drain to the newest sync (an empty-buffer announce may precede it if the
        // attach raced ahead of the edit's dirty flag).
        let mut latest = fake.sync.recv().await.unwrap();
        while let Ok(newer) = fake.sync.try_recv() {
            latest = newer;
        }
        assert_eq!(latest.text, "fn f() {}");
    });
}

#[test]
fn a_highlight_batch_lands_on_the_snapshot_decorations() {
    drive(|h| async move {
        let fake = attach_syntax(&h).await;
        let snap = step(&h, Action::Insert("fn f() {}".into())).await;
        // Color `fn` as a keyword. A real highlighter computes this; here we push
        // it directly to test the editor's plumbing, not the parser.
        fake.events
            .send(SyntaxEvent::Highlights {
                version: snap.version,
                spans: vec![HighlightSpan {
                    range: 0..2,
                    kind: HighlightKind::Keyword,
                }],
            })
            .await
            .unwrap();
        let snap = h.snapshots.recv().await.unwrap();
        assert_eq!(
            snap.decorations.highlights_in(0..9).collect::<Vec<_>>(),
            vec![(0..2, HighlightKind::Keyword)]
        );
    });
}

#[test]
fn re_publishing_an_identical_batch_changes_nothing() {
    // A reparse that yields the same spans (an edit that left tokens intact) must
    // not cost a frame: `apply_highlights` returns false and the loop skips the
    // publish. We exercise that branch, then prove the editor is still live.
    drive(|h| async move {
        let fake = attach_syntax(&h).await;
        let snap = step(&h, Action::Insert("fn f() {}".into())).await;
        let batch = || SyntaxEvent::Highlights {
            version: snap.version,
            spans: vec![HighlightSpan {
                range: 0..2,
                kind: HighlightKind::Keyword,
            }],
        };
        fake.events.send(batch()).await.unwrap();
        // First batch changes the set and republishes.
        let first = h.snapshots.recv().await.unwrap();
        assert_eq!(first.decorations.highlights_in(0..9).count(), 1);
        // Identical batch: no change, no publish. Follow with a snapshot request to
        // prove the editor processed the duplicate and kept running.
        fake.events.send(batch()).await.unwrap();
        let after = step(&h, Action::RequestSnapshot).await;
        assert_eq!(
            after.decorations.highlights_in(0..9).collect::<Vec<_>>(),
            vec![(0..2, HighlightKind::Keyword)]
        );
    });
}

#[test]
fn a_highlight_batch_for_a_stale_version_is_dropped() {
    // The spans are byte offsets in the version the highlighter parsed; a batch that
    // arrives after the buffer has advanced would place them at stale offsets, so it
    // is dropped rather than misplacing highlights (SPEC §5: overlays trail, never
    // misplace). Without the version guard this batch would install `Keyword` over
    // `fn` at v0's coordinates into the v1 buffer.
    drive(|h| async move {
        let fake = attach_syntax(&h).await;
        let snap = step(&h, Action::Insert("fn f() {}".into())).await;
        assert_eq!(snap.version, 1, "the first edit is version 1");
        // A batch tagged with the old version 0.
        fake.events
            .send(SyntaxEvent::Highlights {
                version: 0,
                spans: vec![HighlightSpan {
                    range: 0..2,
                    kind: HighlightKind::Keyword,
                }],
            })
            .await
            .unwrap();
        // Force a snapshot: the stale batch left no highlights on it (whenever the
        // actor processed the drop, it never touched the decoration set).
        let after = step(&h, Action::RequestSnapshot).await;
        assert!(
            after.decorations.is_empty(),
            "a stale-version batch must not install any highlights"
        );
    });
}

#[test]
fn attaching_a_second_highlighter_replaces_the_first() {
    // Reopening as a different language attaches a new grammar; the core swaps it in
    // over the connected one (the attach arrives while the first is live), the same
    // re-root an LSP attach does.
    drive(|h| async move {
        let first = attach_syntax(&h).await;
        step(&h, Action::Insert("fn f() {}".into())).await;
        // Drain the first highlighter's announce so it is definitely connected.
        let _ = first.sync.recv().await.unwrap();

        // A second attach replaces the first in the core (SyntaxAttach while a
        // highlighter is already connected). The new one is announced the buffer.
        let second = attach_syntax(&h).await;
        let mut latest = second.sync.recv().await.unwrap();
        while let Ok(newer) = second.sync.try_recv() {
            latest = newer;
        }
        assert_eq!(latest.text, "fn f() {}");

        // Highlights from the second highlighter drive the snapshot.
        let snap = step(&h, Action::RequestSnapshot).await;
        second
            .events
            .send(SyntaxEvent::Highlights {
                version: snap.version,
                spans: vec![HighlightSpan {
                    range: 0..2,
                    kind: HighlightKind::Keyword,
                }],
            })
            .await
            .unwrap();
        let snap = h.snapshots.recv().await.unwrap();
        assert_eq!(snap.decorations.highlights_in(0..9).count(), 1);
    });
}

#[test]
fn a_highlight_batch_that_cannot_be_published_stops_the_actor() {
    // The frontend gone while a highlight batch is applied (snapshot receiver
    // dropped) shuts the actor down cleanly, the syntax twin of
    // `snapshot_send_failure_stops_the_actor`.
    drive(|h| async move {
        let fake = attach_syntax(&h).await;
        let CoreHandle {
            snapshots,
            notifications,
            actions: _actions,
            deltas: _deltas,
            lsp: _lsp,
            syntax: _syntax,
        } = h;
        drop(snapshots);
        // A non-empty batch for the current version (0, a fresh buffer) changes the
        // empty set, so the actor tries to publish, finds the channel closed, and
        // stops.
        fake.events
            .send(SyntaxEvent::Highlights {
                version: 0,
                spans: vec![HighlightSpan {
                    range: 0..2,
                    kind: HighlightKind::Keyword,
                }],
            })
            .await
            .unwrap();
        assert_eq!(
            notifications.recv().await.unwrap(),
            Notification::ShuttingDown
        );
    });
}

#[test]
fn the_highlighter_closing_does_not_take_the_editor_with_it() {
    // The highlighter dying (its event channel closed) must degrade to "no fresh
    // highlights", never to "no editor" (SPEC §8) - the same guarantee as a dead
    // language server.
    drive(|h| async move {
        let fake = attach_syntax(&h).await;
        step(&h, Action::Insert("fn f() {}".into())).await;
        // Drop the whole fake: closing the event channel is the highlighter's task
        // ending. The editor treats it as SyntaxClosed and carries on.
        drop(fake);
        let snap = step(&h, Action::Insert("!".into())).await;
        assert_eq!(snap.text.to_string(), "fn f() {}!");
    });
}
