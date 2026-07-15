//! The single-owner editor actor (SPEC §2.3).
//!
//! One task owns all editor state. Frontends and (later) LSP/FS tasks talk to it
//! only by message - no shared `Arc<RwLock<Editor>>`, so there are no locks and
//! no data races. M0 owns almost no state yet; the loop shape is what matters,
//! because M1+ grows it in place (add `select!` over LSP/FS channels then).
//!
//! The core does not spawn itself: [`new`] returns the actor loop as a `Future`,
//! and the frontend spawns it on whatever executor it owns. This keeps
//! `vortex-core` executor-agnostic (no `smol`/`tokio` in its public API), the
//! same way it stays terminal-agnostic.

use std::future::Future;

use async_channel::{Receiver, Sender};

use crate::action::Action;
use crate::view::{BufferId, Notification, ViewSnapshot};

/// Channels the frontend uses to talk to a running core.
///
/// - `actions`: frontend -> core, bounded (back-pressure on floods, SPEC §6).
/// - `snapshots`: core -> frontend render state.
/// - `notifications`: core -> frontend discrete events.
///
/// Per SPEC §6 these three streams have distinct delivery semantics; M0 realizes
/// them as bounded `async-channel`s with independent bounds (see [`new`]). M1
/// swaps `snapshots` for a latest-wins single-slot cell; that change is isolated
/// to this handle and the frontend's receive path.
pub struct CoreHandle {
    pub actions: Sender<Action>,
    pub snapshots: Receiver<ViewSnapshot>,
    pub notifications: Receiver<Notification>,
}

/// Owns all editor state. Never shared; lives inside the actor loop.
struct Editor {
    /// The document version, per buffer (SPEC §2.1, §5). Advances only on an
    /// edit, so anchors and LSP `didChange` can key off it. M0 has no edit
    /// action yet, so it stays at its initial value - requesting a snapshot must
    /// not change it (that would desync the version from actual edits).
    version: u64,
    // M1+: buffers behind a `Buffer` trait, SelectionSet, undo tree, syntax.
}

impl Editor {
    fn new() -> Self {
        Self { version: 0 }
    }

    /// Capture a snapshot of current state. A pure read: it reflects the current
    /// document version without advancing it. O(1)-ish by design (SPEC §5) - in
    /// M1 the fields become `Arc` bumps of text/selections/styles.
    fn snapshot(&self) -> ViewSnapshot {
        ViewSnapshot {
            buffer_id: BufferId(0),
            version: self.version,
            text: String::new(),
        }
    }
}

/// Handle to the core plus its actor loop.
pub struct Core {
    /// Channels to drive the running core.
    pub handle: CoreHandle,
    /// The actor loop. The frontend must spawn this on its executor; the core
    /// does nothing until it is polled.
    pub run: BoxFuture,
}

/// The actor loop's type. Boxed so `vortex-core` exposes no executor type.
pub type BoxFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Snapshot channel bound. M0's request/response flow never queues more than one
/// snapshot; M1 replaces this with a latest-wins single-slot cell (SPEC §6).
const SNAPSHOT_CAP: usize = 1;
/// Notification channel bound. Discrete, low-volume events (SPEC §6).
const NOTIFICATION_CAP: usize = 64;

/// Create a core. Returns a [`CoreHandle`] and the actor loop to spawn.
///
/// `action_capacity` bounds the frontend -> core action channel - the
/// back-pressure-critical stream (SPEC §6). The snapshot and notification
/// channels get their own fixed bounds so sizing the action queue does not
/// inflate them.
///
/// The loop runs until it receives [`Action::Quit`] or the action channel closes
/// (frontend dropped), then makes a best-effort [`Notification::ShuttingDown`]
/// and stops.
///
/// # Panics
/// Panics if `action_capacity` is 0 (a bounded channel needs capacity >= 1).
/// `action_capacity` is a code constant today; if it ever comes from user config,
/// validate it there before calling this.
pub fn new(action_capacity: usize) -> Core {
    assert!(action_capacity > 0, "action_capacity must be >= 1");

    let (action_tx, action_rx) = async_channel::bounded::<Action>(action_capacity);
    let (snapshot_tx, snapshot_rx) = async_channel::bounded::<ViewSnapshot>(SNAPSHOT_CAP);
    let (note_tx, note_rx) = async_channel::bounded::<Notification>(NOTIFICATION_CAP);

    Core {
        handle: CoreHandle {
            actions: action_tx,
            snapshots: snapshot_rx,
            notifications: note_rx,
        },
        run: Box::pin(run(action_rx, snapshot_tx, note_tx)),
    }
}

/// The actor loop. M0 handles only `RequestSnapshot` and `Quit`; M1+ adds a
/// `select!` over LSP/FS channels alongside this `recv`.
async fn run(
    actions: Receiver<Action>,
    snapshots: Sender<ViewSnapshot>,
    notifications: Sender<Notification>,
) {
    let editor = Editor::new();

    while let Ok(action) = actions.recv().await {
        match action {
            Action::RequestSnapshot => {
                // If the frontend has hung up, stop; nothing to render for.
                if snapshots.send(editor.snapshot()).await.is_err() {
                    break;
                }
            }
            Action::Quit => break,
        }
    }

    // Best-effort and non-blocking: the frontend may be gone (channel closed) or
    // not draining (channel full) - either way we are shutting down, so never
    // await here or shutdown could stall on a full notifications channel.
    let _ = notifications.try_send(Notification::ShuttingDown);
}
