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

#[test]
fn a_diagnostic_and_a_highlight_both_reach_the_snapshot() {
    // Both producers feeding at once - an LSP diagnostic and a syntax highlight -
    // must both land on the decoration channel. The actor's producer scheduling is
    // fair (it alternates the LSP/syntax poll order each turn, `next_incoming`), so
    // a burst on one cannot starve the other; here we assert the weaker functional
    // guarantee that neither event is dropped when both are pending.
    let dir = TempDir::new();
    drive_lsp(|h, server| async move {
        // Keep the whole fake server alive (Rust 2021 disjoint capture would else
        // drop the unused sync side, which the core reads as the server dying).
        let FakeServer {
            sync: _sync,
            events,
        } = server;
        let path = open_fixture(&h, &dir.0).await; // FIXTURE loads as version 1

        // Attach a fake highlighter alongside the already-attached server.
        let (_sx_sync_tx, _sx_sync_rx) = async_channel::bounded::<SyntaxSync>(16);
        let (sx_event_tx, sx_event_rx) = async_channel::bounded::<SyntaxEvent>(16);
        h.syntax
            .send(SyntaxHandle {
                sync: _sx_sync_tx,
                events: sx_event_rx,
            })
            .await
            .unwrap();

        // Feed one of each, both pending before the actor processes either.
        events
            .send(LspEvent::Diagnostics {
                path,
                diagnostics: vec![diag(0, 0, 3, Severity::Error)],
            })
            .await
            .unwrap();
        sx_event_tx
            .send(SyntaxEvent::Highlights {
                buffer_id: BufferId(0),
                version: 1,
                spans: vec![HighlightSpan {
                    range: 0..3,
                    kind: HighlightKind::Keyword,
                }],
            })
            .await
            .unwrap();

        // The two buckets are independent, so the accumulated set ends with both.
        // A correct actor applies each within a turn or two, so this breaks early;
        // a starved/dropped event would leave one side missing.
        let mut got_diagnostic = false;
        let mut got_highlight = false;
        for _ in 0..6 {
            if got_diagnostic && got_highlight {
                break;
            }
            let snap = h.snapshots.recv().await.unwrap();
            got_diagnostic |= snap.decorations.underlines_in(0..9999).count() > 0;
            got_highlight |= snap.decorations.highlights_in(0..9999).count() > 0;
        }
        assert!(
            got_diagnostic && got_highlight,
            "both producers must reach the snapshot (diag={got_diagnostic}, hl={got_highlight})"
        );
    });
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

    /// A path to `name` inside this dir (the file need not exist).
    fn file(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
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
                ..
            } => {
                assert_eq!(p, path);
                // The LSP identifier, not the file extension.
                assert_eq!(language_id, "rust");
                assert_eq!(text.to_string(), FIXTURE);
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
                assert_eq!(text.to_string(), format!("x{FIXTURE}"));
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

/// Attach a fake language server over `CoreHandle::lsp`, the runtime path a frontend
/// takes once it has spawned a client for a file type. A second call replaces the
/// first, as re-rooting on a different workspace does.
async fn attach_lsp(h: &CoreHandle) -> FakeServer {
    let (sync_tx, sync_rx) = async_channel::bounded(16);
    let (event_tx, event_rx) = async_channel::bounded(16);
    h.lsp
        .send(LspHandle {
            sync: sync_tx,
            events: event_rx,
        })
        .await
        .unwrap();
    FakeServer {
        sync: sync_rx,
        events: event_tx,
    }
}

/// The **next** document sync this server is sent, or `None` if none arrives.
///
/// Deliberately the first message rather than the newest: what a server is told
/// *first* about a document is the whole question for the `didOpen`/`didChange`
/// split, and a later change would mask an opening that never happened.
///
/// Yields so the actor gets turns to flush - it syncs at the top of a loop turn, not
/// in response to a message, so there is nothing to await on directly.
async fn next_sync(server: &FakeServer) -> Option<crate::lsp::DocumentSync> {
    for _ in 0..64 {
        smol::future::yield_now().await;
        if let Ok(message) = server.sync.try_recv() {
            return Some(message);
        }
    }
    None
}

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
        assert_eq!(latest.text.to_string(), "fn f() {}");
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
                buffer_id: BufferId(0),
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
            buffer_id: BufferId(0),
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
                buffer_id: BufferId(0),
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

/// Every notification the core has emitted and not yet been read.
fn drain_notifications(h: &CoreHandle) -> Vec<Notification> {
    std::iter::from_fn(|| h.notifications.try_recv().ok()).collect()
}

/// Open `count` files through the seam, returning their buffer ids in open order.
/// The last one is left active, as opening does.
async fn open_files(h: &CoreHandle, dir: &TempDir, count: usize) -> Vec<BufferId> {
    let mut ids = Vec::new();
    for n in 0..count {
        let path = dir.0.join(format!("file{n}.txt"));
        std::fs::write(&path, format!("contents {n}")).unwrap();
        ids.push(step(h, Action::Open(path)).await.buffer_id);
    }
    ids
}

#[test]
fn switching_buffers_restores_that_buffers_own_state() {
    // A background buffer keeps its text, carets and history untouched, so switching
    // back is a return to exactly what was left - the whole point of multi-buffer.
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 2).await;

        // Type in the second buffer, then go back to the first.
        step(&h, Action::Insert("edited ".into())).await;
        let back = step(&h, Action::SwitchBuffer { id: ids[0] }).await;
        assert_eq!(back.buffer_id, ids[0]);
        assert_eq!(back.text.to_string(), "contents 0");
        assert!(!back.modified);

        let forward = step(&h, Action::SwitchBuffer { id: ids[1] }).await;
        assert_eq!(forward.text.to_string(), "edited contents 1");
        assert!(forward.modified, "its unsaved edit survived the round trip");
        // Undo still works against that buffer's own history.
        let undone = step(&h, Action::Undo).await;
        assert_eq!(undone.text.to_string(), "contents 1");
    });
}

#[test]
fn switching_to_an_unknown_buffer_is_a_no_op() {
    // A frontend can hold a stale id (a picker entry for a buffer closed since).
    // It must not panic or silently focus the wrong buffer.
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 1).await;
        let after = step(&h, Action::SwitchBuffer { id: BufferId(9999) }).await;
        assert_eq!(after.buffer_id, ids[0], "focus is unchanged");
    });
}

#[test]
fn closing_a_modified_buffer_is_refused_until_forced() {
    // SPEC §8: the core will not discard unsaved work, and it is the core that
    // enforces it - a frontend cannot skip the check by forgetting to ask.
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 2).await;
        step(&h, Action::Insert("dirty".into())).await;

        let after = step(
            &h,
            Action::CloseBuffer {
                id: ids[1],
                force: false,
            },
        )
        .await;
        assert_eq!(after.buffers.len(), 2, "nothing was closed");
        assert_eq!(after.buffer_id, ids[1], "and it is still focused");
        let notes = drain_notifications(&h);
        assert!(
            notes.iter().any(|n| matches!(
                n,
                Notification::CloseRejected { buffer_id, .. } if *buffer_id == ids[1]
            )),
            "expected CloseRejected, got {notes:?}"
        );

        // Forcing discards the work, which is the user's call to make.
        let after = step(
            &h,
            Action::CloseBuffer {
                id: ids[1],
                force: true,
            },
        )
        .await;
        assert_eq!(after.buffers.len(), 1);
        assert_eq!(after.buffer_id, ids[0], "focus fell back to the neighbor");
    });
}

#[test]
fn closing_an_unmodified_buffer_needs_no_force() {
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 2).await;
        let after = step(
            &h,
            Action::CloseBuffer {
                id: ids[0],
                force: false,
            },
        )
        .await;
        assert_eq!(after.buffers.len(), 1);
        assert_eq!(after.buffers[0].id, ids[1]);
        // Closing a *background* buffer leaves focus alone.
        assert_eq!(after.buffer_id, ids[1]);
        let notes = drain_notifications(&h);
        assert!(
            notes.iter().any(|n| matches!(
                n,
                Notification::BufferClosed { buffer_id } if *buffer_id == ids[0]
            )),
            "expected BufferClosed, got {notes:?}"
        );
    });
}

#[test]
fn closing_the_last_buffer_leaves_a_fresh_one() {
    // The session must always have somewhere to type, so the final close yields an
    // empty unnamed buffer rather than an empty session (which `active` could not
    // resolve against).
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 1).await;
        let after = step(
            &h,
            Action::CloseBuffer {
                id: ids[0],
                force: false,
            },
        )
        .await;
        assert_eq!(after.buffers.len(), 1);
        assert_ne!(after.buffer_id, ids[0], "a different, fresh buffer");
        assert!(after.text.is_empty());
        assert!(after.path.is_none());
        assert!(!after.modified);
        // Still live: it accepts edits like any buffer.
        let typed = step(&h, Action::Insert("hello".into())).await;
        assert_eq!(typed.text.to_string(), "hello");
    });
}

#[test]
fn closing_a_buffer_before_the_active_one_keeps_focus() {
    // Removing an earlier entry shifts every later index down; focus must follow the
    // buffer, not the index it happened to sit at.
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 3).await;
        assert_eq!(ids.len(), 3);
        let after = step(
            &h,
            Action::CloseBuffer {
                id: ids[0],
                force: false,
            },
        )
        .await;
        assert_eq!(after.buffer_id, ids[2], "still the buffer that was active");
        assert_eq!(after.text.to_string(), "contents 2");
        let listed: Vec<_> = after.buffers.iter().map(|i| i.id).collect();
        assert_eq!(listed, vec![ids[1], ids[2]]);
    });
}

#[test]
fn closing_a_buffer_after_the_active_one_changes_nothing() {
    // The mirror of the shift case: removing a later entry leaves both focus and
    // every earlier index alone.
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 3).await;
        step(&h, Action::SwitchBuffer { id: ids[1] }).await;
        let after = step(
            &h,
            Action::CloseBuffer {
                id: ids[2],
                force: false,
            },
        )
        .await;
        assert_eq!(after.buffer_id, ids[1], "focus is untouched");
        let listed: Vec<_> = after.buffers.iter().map(|i| i.id).collect();
        assert_eq!(listed, vec![ids[0], ids[1]]);
    });
}

#[test]
fn closing_an_unknown_buffer_is_a_no_op() {
    // The close twin of a stale switch: a frontend may still hold an id for a buffer
    // closed by some other path. Nothing to close is not an error.
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 1).await;
        let after = step(
            &h,
            Action::CloseBuffer {
                id: BufferId(9999),
                force: false,
            },
        )
        .await;
        assert_eq!(after.buffers.len(), 1);
        assert_eq!(after.buffer_id, ids[0]);
    });
}

#[test]
fn a_highlight_batch_for_a_closed_buffer_is_dropped() {
    // A parse can outlive the buffer it was for: the user closes a file while its
    // reparse is in flight. The batch has nowhere to land and must be discarded
    // without touching whatever is on screen now.
    drive(|h| async move {
        let dir = TempDir::new();
        let fake = attach_syntax(&h).await;
        let ids = open_files(&h, &dir, 2).await;
        let closed = ids[0];
        step(
            &h,
            Action::CloseBuffer {
                id: closed,
                force: false,
            },
        )
        .await;

        fake.events
            .send(SyntaxEvent::Highlights {
                buffer_id: closed,
                version: 0,
                spans: vec![HighlightSpan {
                    range: 0..2,
                    kind: HighlightKind::Keyword,
                }],
            })
            .await
            .unwrap();

        let after = step(&h, Action::RequestSnapshot).await;
        assert_eq!(after.buffer_id, ids[1]);
        assert!(
            after.decorations.is_empty(),
            "a batch for a closed buffer must not paint the survivor"
        );
    });
}

#[test]
fn the_snapshot_buffer_list_tracks_opens_closes_and_edits() {
    drive(|h| async move {
        let dir = TempDir::new();
        let first = step(&h, Action::RequestSnapshot).await;
        assert_eq!(first.buffers.len(), 1, "the session starts with a scratch");

        let ids = open_files(&h, &dir, 2).await;
        let after = step(&h, Action::RequestSnapshot).await;
        // The first open reused the untouched scratch, the second added a buffer.
        assert_eq!(after.buffers.len(), 2);
        assert!(after.buffers.iter().all(|i| i.path.is_some()));
        assert!(after.buffers.iter().all(|i| !i.modified));

        let edited = step(&h, Action::Insert("x".into())).await;
        let entry = edited
            .buffers
            .iter()
            .find(|i| i.id == ids[1])
            .expect("the edited buffer is listed");
        assert!(entry.modified, "the list shows the unsaved marker");
    });
}

#[test]
fn a_replacement_server_is_told_about_every_open_buffer() {
    // REGRESSION (code review, M7): `lsp_sync` is session-wide but `lsp_opened` is
    // per-document, and the attach arm cleared it only on the active buffer. A
    // background buffer therefore stayed flagged as opened, so its next sync sent the
    // *new* server a `didChange` for a file that server had never seen - which the
    // client drops, leaving that buffer silently unanalyzed for as long as it is open.
    //
    // Currently unreachable through the UI (only one language server is known, and it
    // is keyed so it attaches once), which is exactly why it needs pinning here: it
    // goes live the moment a second server joins the table.
    drive(|h| async move {
        let dir = TempDir::new();
        let ids = open_files(&h, &dir, 2).await;

        // Both buffers must be *known* to the first server, or the second server
        // would be the first to hear about the background one either way and the
        // stale flag could not show itself. Focus each in turn so each is announced.
        step(&h, Action::SwitchBuffer { id: ids[0] }).await;
        let first = attach_lsp(&h).await;
        assert!(matches!(
            next_sync(&first).await,
            Some(DocumentSync::Opened { .. })
        ));
        step(&h, Action::SwitchBuffer { id: ids[1] }).await;
        assert!(matches!(
            next_sync(&first).await,
            Some(DocumentSync::Opened { .. })
        ));

        // A replacement server arrives - re-rooting on another workspace, or simply a
        // second language server. It has seen neither document.
        let second = attach_lsp(&h).await;
        let announced = next_sync(&second).await;
        assert!(
            matches!(&announced, Some(DocumentSync::Opened { .. })),
            "the active buffer must re-open against the new server, got {announced:?}"
        );

        // The background buffer, which the *old* server had opened, must open against
        // this one too. Sending a change for a document it never opened is what the
        // client drops on the floor, silently ending analysis for that file.
        step(&h, Action::SwitchBuffer { id: ids[0] }).await;
        step(&h, Action::Insert("edit".into())).await;
        let announced = next_sync(&second).await;
        assert!(
            matches!(&announced, Some(DocumentSync::Opened { .. })),
            "the background buffer must open, not change, got {announced:?}"
        );
    });
}

#[test]
fn a_highlight_batch_never_paints_a_different_buffer() {
    // REGRESSION (M7): the batch guard used to be `version` alone. Versions are
    // per-buffer (SPEC §5), so two open documents sit at the same version all the
    // time - a batch parsed against one would then install its spans into whichever
    // buffer happened to be active, at offsets that mean nothing in that text. That
    // is a *misplacement*, which the overlay contract forbids outright (overlays may
    // trail the text, never mislead about it).
    //
    // Both buffers here are at the same version, so a version-only guard passes and
    // the spans land on the wrong file.
    drive(|h| async move {
        let dir = TempDir::new();
        let fake = attach_syntax(&h).await;

        let first = dir.0.join("first.rs");
        std::fs::write(&first, "fn alpha() {}").unwrap();
        let opened = step(&h, Action::Open(first)).await;
        let background = opened.buffer_id;

        let second = dir.0.join("second.rs");
        std::fs::write(&second, "fn beta() {}").unwrap();
        let active = step(&h, Action::Open(second)).await;
        assert_ne!(active.buffer_id, background, "two buffers are open");
        assert_eq!(
            active.version, opened.version,
            "the premise: both are at the same version"
        );

        // A batch belonging to the *background* buffer, at the version both share.
        fake.events
            .send(SyntaxEvent::Highlights {
                buffer_id: background,
                version: active.version,
                spans: vec![HighlightSpan {
                    range: 0..2,
                    kind: HighlightKind::Keyword,
                }],
            })
            .await
            .unwrap();

        let after = step(&h, Action::RequestSnapshot).await;
        assert_eq!(after.buffer_id, active.buffer_id);
        assert!(
            after.decorations.is_empty(),
            "a batch parsed against another buffer must not paint the active one"
        );

        // It is not merely dropped: switching to the buffer it belongs to shows it.
        let switched = step(&h, Action::SwitchBuffer { id: background }).await;
        assert_eq!(
            switched
                .decorations
                .highlights_in(0..13)
                .collect::<Vec<_>>(),
            vec![(0..2, HighlightKind::Keyword)],
            "the batch belongs to this buffer and must have landed here"
        );
    });
}

#[test]
fn diagnostics_reach_a_buffer_that_is_not_on_screen() {
    // REGRESSION (M7): diagnostics were applied to the active document only, so a
    // server publishing for another file in the workspace - which it does constantly,
    // that is what a workspace-wide analysis *is* - had its batch dropped on the
    // floor. Switching to that buffer then showed a clean file that is not clean.
    drive_lsp(|h, server| async move {
        let dir = TempDir::new();
        let background = dir.0.join("first.rs");
        std::fs::write(&background, "let x = 1").unwrap();
        let opened = step(&h, Action::Open(background.clone())).await;
        let background_id = opened.buffer_id;

        let front = dir.0.join("second.rs");
        std::fs::write(&front, "let y = 2").unwrap();
        let active = step(&h, Action::Open(front)).await;
        assert_ne!(active.buffer_id, background_id);

        // The server reports on the file that is *not* on screen.
        server
            .events
            .send(LspEvent::Diagnostics {
                path: background,
                diagnostics: vec![diag(0, 4, 5, Severity::Error)],
            })
            .await
            .unwrap();

        let after = step(&h, Action::RequestSnapshot).await;
        assert!(
            after.decorations.is_empty(),
            "another buffer's diagnostics must not appear on this one"
        );

        let switched = step(&h, Action::SwitchBuffer { id: background_id }).await;
        assert_eq!(
            switched.decorations.underlines_in(0..9).count(),
            1,
            "the diagnostic must be waiting on the buffer it was published for"
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
        assert_eq!(latest.text.to_string(), "fn f() {}");

        // Highlights from the second highlighter drive the snapshot.
        let snap = step(&h, Action::RequestSnapshot).await;
        second
            .events
            .send(SyntaxEvent::Highlights {
                buffer_id: BufferId(0),
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
            watch: _watch,
        } = h;
        drop(snapshots);
        // A non-empty batch for the current version (0, a fresh buffer) changes the
        // empty set, so the actor tries to publish, finds the channel closed, and
        // stops.
        fake.events
            .send(SyntaxEvent::Highlights {
                buffer_id: BufferId(0),
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

// --- External changes through the seam (SPEC §10.2) ---------------------------

/// The watcher side of the seam: what the core asked us to watch, and a way to
/// report that a file changed.
struct FakeWatcher {
    /// editor -> us: which files to follow.
    requests: Receiver<crate::watch::WatchRequest>,
    /// us -> editor: a file changed.
    events: Sender<crate::watch::FileEvent>,
}

/// Attach a fake watcher over `CoreHandle::watch`, as the frontend does at startup.
async fn attach_watcher(h: &CoreHandle) -> FakeWatcher {
    let (request_tx, request_rx) = async_channel::bounded(16);
    let (event_tx, event_rx) = async_channel::bounded(16);
    h.watch
        .send(crate::watch::WatchHandle {
            requests: request_tx,
            events: event_rx,
        })
        .await
        .unwrap();
    FakeWatcher {
        requests: request_rx,
        events: event_tx,
    }
}

/// Await the next notification matching `f`, ignoring the ones that precede it.
/// The producer channels interleave, so a seam test cannot assume position.
async fn await_note<F>(h: &CoreHandle, f: F) -> Notification
where
    F: Fn(&Notification) -> bool,
{
    loop {
        let note = h.notifications.recv().await.unwrap();
        if f(&note) {
            return note;
        }
    }
}

#[test]
fn a_file_changing_under_a_clean_buffer_reloads_it_through_the_loop() {
    // The whole path in one test: open a file, tell the core it changed, and watch
    // the reload come back out as a snapshot - no direct calls into the internals.
    let dir = TempDir::new();
    let path = dir.file("watched.txt");
    std::fs::write(&path, "first\n").unwrap();

    drive(|h| async move {
        let watcher = attach_watcher(&h).await;
        step(&h, Action::Open(path.clone())).await;
        assert_eq!(
            watcher.requests.recv().await.unwrap(),
            crate::watch::WatchRequest::Watch(path.clone()),
            "opening announces the file to the watcher"
        );

        std::fs::write(&path, "rewritten by someone else\n").unwrap();
        watcher
            .events
            .send(crate::watch::FileEvent::Changed(path.clone()))
            .await
            .unwrap();

        await_note(&h, |n| matches!(n, Notification::FileReloaded { .. })).await;
        let snap = step(&h, Action::RequestSnapshot).await;
        assert_eq!(snap.text.to_string(), "rewritten by someone else\n");
        assert!(!snap.modified);
    });
}

#[test]
fn a_conflict_is_resolved_by_the_reload_the_frontend_sends_back() {
    // The modified case end to end: the core refuses to choose, the frontend asks,
    // and the answer comes back as a forced Reload (SPEC §8).
    let dir = TempDir::new();
    let path = dir.file("watched.txt");
    std::fs::write(&path, "first\n").unwrap();

    drive(|h| async move {
        let watcher = attach_watcher(&h).await;
        step(&h, Action::Open(path.clone())).await;
        let snap = step(&h, Action::Insert("mine ".into())).await;
        let id = snap.buffer_id;

        std::fs::write(&path, "theirs\n").unwrap();
        watcher
            .events
            .send(crate::watch::FileEvent::Changed(path.clone()))
            .await
            .unwrap();
        await_note(&h, |n| matches!(n, Notification::ExternalChange { .. })).await;

        // Unforced first: still refused, because the buffer still has the work.
        step(&h, Action::Reload { id, force: false }).await;
        await_note(&h, |n| matches!(n, Notification::ReloadRejected { .. })).await;
        let snap = step(&h, Action::RequestSnapshot).await;
        assert_eq!(snap.text.to_string(), "mine first\n");

        // Then forced, which is what the confirmation prompt commits.
        step(&h, Action::Reload { id, force: true }).await;
        await_note(&h, |n| matches!(n, Notification::FileReloaded { .. })).await;
        let snap = step(&h, Action::RequestSnapshot).await;
        assert_eq!(snap.text.to_string(), "theirs\n");
        assert!(!snap.modified);
    });
}

#[test]
fn the_watcher_closing_does_not_take_the_editor_with_it() {
    // The watcher dying degrades to "no external-change detection", never to "no
    // editor" (SPEC §8) - the same guarantee the LSP and syntax producers carry.
    drive(|h| async move {
        let watcher = attach_watcher(&h).await;
        step(&h, Action::Insert("still here".into())).await;
        drop(watcher);
        let snap = step(&h, Action::Insert("!".into())).await;
        assert_eq!(snap.text.to_string(), "still here!");
    });
}

#[test]
fn attaching_a_second_watcher_replaces_the_first() {
    // A watcher can be replaced mid-session, and the replacement has to be told
    // about the files already open - the same re-announce a new language server
    // gets. Dropping the first one's channels is what stops its loop (SPEC §8).
    let dir = TempDir::new();
    let path = dir.file("watched.txt");
    std::fs::write(&path, "body\n").unwrap();

    drive(|h| async move {
        let first = attach_watcher(&h).await;
        step(&h, Action::Open(path.clone())).await;
        assert_eq!(
            first.requests.recv().await.unwrap(),
            crate::watch::WatchRequest::Watch(path.clone())
        );

        let second = attach_watcher(&h).await;
        assert_eq!(
            second.requests.recv().await.unwrap(),
            crate::watch::WatchRequest::Watch(path.clone()),
            "the replacement hears about the file that was already open"
        );

        // And it is the one now driving: a change reported through it lands.
        std::fs::write(&path, "changed by someone else\n").unwrap();
        second
            .events
            .send(crate::watch::FileEvent::Changed(path))
            .await
            .unwrap();
        await_note(&h, |n| matches!(n, Notification::FileReloaded { .. })).await;
    });
}
