//! `vortex-core` - the headless editor core.
//!
//! The core owns buffer state, selections, undo, syntax, and LSP; frontends talk
//! to it only by message (SPEC §1). This crate MUST NOT depend on any terminal
//! crate - that boundary is enforced by its `Cargo.toml` and is what lets other
//! frontends (GUI, web, remote) attach later without touching core logic.

pub mod action;
pub mod editor;
pub mod view;

pub use action::Action;
pub use editor::{Core, CoreHandle, new};
pub use view::{BufferId, Notification, ViewSnapshot};

#[cfg(test)]
mod tests {
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

    #[test]
    fn request_snapshot_round_trips() {
        drive(|h| async move {
            h.actions.send(Action::RequestSnapshot).await.unwrap();
            let snap = h.snapshots.recv().await.unwrap();
            assert_eq!(snap.buffer_id, BufferId(0));
            // No edits yet, so the document version is its initial 0.
            assert_eq!(snap.version, 0);
            assert!(snap.text.is_empty());
        });
    }

    #[test]
    fn snapshot_version_is_stable_without_edits() {
        // `version` is the document version (SPEC §2.1, §5): it advances on
        // edits, NOT on snapshot requests. With no edit action in M0, repeated
        // requests must report the same version - otherwise anchors/LSP keyed on
        // it would desync from actual edits.
        drive(|h| async move {
            h.actions.send(Action::RequestSnapshot).await.unwrap();
            let first = h.snapshots.recv().await.unwrap();
            h.actions.send(Action::RequestSnapshot).await.unwrap();
            let second = h.snapshots.recv().await.unwrap();
            assert_eq!(first.version, 0);
            assert_eq!(second.version, first.version);
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
        // down cleanly rather than looping (editor.rs:104).
        drive(|h| async move {
            let CoreHandle {
                actions,
                snapshots,
                notifications,
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
}
