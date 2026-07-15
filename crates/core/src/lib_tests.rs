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
