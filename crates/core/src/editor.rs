//! The single-owner editor actor (SPEC §2.3).
//!
//! One task owns all editor state - buffer, selection set, version. Frontends and
//! (later) LSP/FS tasks talk to it only by message: no shared `Arc<RwLock<Editor>>`,
//! so there are no locks and no data races. The loop shape is what M1+ grows in
//! place (add a `select!` over LSP/FS channels alongside the action `recv`).
//!
//! The core does not spawn itself: [`new`] returns the actor loop as a `Future`
//! and the frontend spawns it on whatever executor it owns, keeping `vortex-core`
//! executor-agnostic (no `smol`/`tokio` in its public API) the same way it stays
//! terminal-agnostic.
//!
//! **Channels (SPEC §6):**
//! - `actions` (frontend -> core): bounded, back-pressure on floods.
//! - `deltas` (core -> frontend): bounded, lossless, ordered - the authoritative
//!   change log and future remote wire (a dropped delta diverges a remote buffer).
//! - `snapshots` (core -> frontend): **latest-wins single slot** - a derived
//!   convenience; the frontend only wants the newest, so intermediates during a
//!   fast paste are safely dropped.
//! - `notifications` (core -> frontend): bounded, discrete events.

use std::future::Future;
use std::sync::Arc;

use async_channel::{Receiver, Sender};

use crate::action::Action;
use crate::buffer::{Buffer, RopeBuffer};
use crate::selection::{Selection, SelectionSet};
use crate::view::{BufferId, Delta, Notification, ViewSnapshot};

/// Channels the frontend uses to talk to a running core (SPEC §6).
pub struct CoreHandle {
    /// frontend -> core, bounded (back-pressure on floods).
    pub actions: Sender<Action>,
    /// core -> frontend, lossless ordered change log (remote wire, journal).
    pub deltas: Receiver<Delta>,
    /// core -> frontend, latest-wins render state (see [`SnapshotCell`]).
    pub snapshots: SnapshotCell,
    /// core -> frontend, discrete events.
    pub notifications: Receiver<Notification>,
}

/// A latest-wins single-slot snapshot channel (SPEC §6 "watch-style cell").
///
/// Backed by a `bounded(1)` `async-channel`: the core *overwrites* rather than
/// blocks (drains the stale value, then sends the fresh one), so a burst of edits
/// leaves only the newest snapshot for the frontend to paint. `async-channel` has
/// no native watch type; this thin wrapper gives the semantics without a new
/// dependency.
#[derive(Clone)]
pub struct SnapshotCell {
    rx: Receiver<ViewSnapshot>,
}

impl SnapshotCell {
    /// Await the next snapshot. Errors only once the core has stopped and the
    /// channel is closed.
    pub async fn recv(&self) -> Result<ViewSnapshot, async_channel::RecvError> {
        self.rx.recv().await
    }

    /// The most recent snapshot without awaiting, if one is buffered. Returns
    /// `None` when the slot is empty (frontend already took it) - the caller then
    /// paints from the last snapshot it held.
    pub fn try_recv(&self) -> Option<ViewSnapshot> {
        self.rx.try_recv().ok()
    }
}

/// The sender half of the latest-wins cell, held by the core.
struct SnapshotSink {
    tx: Sender<ViewSnapshot>,
}

impl SnapshotSink {
    /// Publish `snapshot`, replacing any unread one (latest-wins, SPEC §6). Never
    /// blocks: `force_send` overwrites the slot's stale value when full, so a
    /// burst of edits leaves only the newest snapshot for the frontend. Returns
    /// `false` only if the frontend has hung up (channel closed), signaling
    /// shutdown.
    fn publish(&self, snapshot: ViewSnapshot) -> bool {
        // Ok(_) whether the slot was empty (None) or overwritten (Some(stale));
        // both are success. Err means the receiver is gone.
        self.tx.force_send(snapshot).is_ok()
    }
}

/// Owns all editor state. Never shared; lives inside the actor loop.
struct Editor {
    id: BufferId,
    buffer: RopeBuffer,
    selections: SelectionSet,
    /// The document version (SPEC §2.1, §5). Advances only on an applied edit, so
    /// anchors and LSP `didChange` can key off it; a snapshot request does not
    /// change it.
    version: u64,
}

impl Editor {
    fn new() -> Self {
        Self {
            id: BufferId(0),
            buffer: RopeBuffer::new(),
            selections: SelectionSet::at_origin(),
            version: 0,
        }
    }

    /// Build a snapshot of current state (SPEC §5). The `text` field is an
    /// `Arc`-clone of the rope handle (O(1), the load-bearing part). Selections
    /// are copied into a fresh `Arc<[Selection]>` here - O(selections), which is
    /// trivial for M1's single selection. When M3 adds many cursors, hold the
    /// selection set as an `Arc<[Selection]>` internally so this becomes an `Arc`
    /// bump too, matching the SPEC §5 O(1)-snapshot goal for every field.
    fn snapshot(&self, dirty: Option<std::ops::Range<usize>>) -> ViewSnapshot {
        ViewSnapshot {
            buffer_id: self.id,
            version: self.version,
            text: self.buffer.text(),
            selections: Arc::from(self.selections.all()),
            primary: self.selections.primary_index(),
            dirty,
        }
    }

    /// Apply `motion` to the selection set. Pure state change, no delta - motion
    /// does not alter buffer text, so no version bump and no delta emission.
    fn move_cursor(&mut self, motion: crate::selection::Motion, extend: bool) {
        let text = self.buffer.text();
        self.selections.move_all(&text, motion, extend);
    }

    /// Compute the edits an `Insert`/`Delete` action produces over the selection
    /// set, as `(range, new_text)` pairs in the current buffer's coordinates.
    ///
    /// Returned **sorted by start, descending** so the caller can apply them
    /// back-to-front: applying a later edit first keeps earlier ranges' offsets
    /// valid (an edit shifts everything after it). One user action fans into N
    /// edits over N cursors but remains one logical action (SPEC §2.4).
    fn plan_edit(&self, kind: EditKind) -> Vec<(std::ops::Range<usize>, String)> {
        let text = self.buffer.text();
        let mut edits: Vec<(std::ops::Range<usize>, String)> = self
            .selections
            .all()
            .iter()
            .filter_map(|sel| edit_for_selection(&text, sel, &kind))
            .collect();
        // Descending by start so back-to-front application is offset-stable.
        edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
        edits
    }
}

/// The kind of text edit an action requests, resolved against each selection.
enum EditKind {
    /// Insert this text (replacing a non-empty selection).
    Insert(String),
    /// Delete backward one grapheme (or the selection if non-empty).
    DeleteBackward,
    /// Delete forward one grapheme (or the selection if non-empty).
    DeleteForward,
}

/// What the actor loop must do for one action: apply a text edit, or just
/// republish the current state (a motion or an explicit snapshot request).
/// Both paths return "is the frontend still alive?"; `Quit` breaks before this.
enum Step {
    Edit(EditKind),
    Republish,
}

/// The concrete `(range, new_text)` a single selection contributes for `kind`,
/// or `None` if it is a no-op (e.g. backspace at buffer start).
fn edit_for_selection(
    text: &crate::buffer::Text,
    sel: &Selection,
    kind: &EditKind,
) -> Option<(std::ops::Range<usize>, String)> {
    match kind {
        EditKind::Insert(s) => Some((sel.start()..sel.end(), s.clone())),
        EditKind::DeleteBackward => {
            if sel.is_cursor() {
                let from = crate::selection::grapheme_before(text, sel.head);
                (from < sel.head).then(|| (from..sel.head, String::new()))
            } else {
                Some((sel.start()..sel.end(), String::new()))
            }
        }
        EditKind::DeleteForward => {
            if sel.is_cursor() {
                let to = crate::selection::grapheme_after(text, sel.head);
                (to > sel.head).then(|| (sel.head..to, String::new()))
            } else {
                Some((sel.start()..sel.end(), String::new()))
            }
        }
    }
}

/// Handle to the core plus its actor loop.
pub struct Core {
    pub handle: CoreHandle,
    /// The actor loop. The frontend must spawn this on its executor; the core
    /// does nothing until it is polled.
    pub run: BoxFuture,
}

/// The actor loop's type. Boxed so `vortex-core` exposes no executor type.
pub type BoxFuture = std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Latest-wins snapshot slot: capacity 1 (SPEC §6).
const SNAPSHOT_CAP: usize = 1;
/// Delta channel bound: lossless ordered log; sized to absorb bursts (SPEC §6).
const DELTA_CAP: usize = 1024;
/// Notification channel bound: discrete, low-volume events (SPEC §6).
const NOTIFICATION_CAP: usize = 64;

/// Create a core. Returns a [`CoreHandle`] and the actor loop to spawn.
///
/// `action_capacity` bounds the frontend -> core action channel, the
/// back-pressure-critical stream (SPEC §6). Other channels get their own fixed
/// bounds so sizing the action queue does not inflate them.
///
/// # Panics
/// Panics if `action_capacity` is 0 (a bounded channel needs capacity >= 1).
pub fn new(action_capacity: usize) -> Core {
    assert!(action_capacity > 0, "action_capacity must be >= 1");

    let (action_tx, action_rx) = async_channel::bounded::<Action>(action_capacity);
    let (delta_tx, delta_rx) = async_channel::bounded::<Delta>(DELTA_CAP);
    let (snapshot_tx, snapshot_rx) = async_channel::bounded::<ViewSnapshot>(SNAPSHOT_CAP);
    let (note_tx, note_rx) = async_channel::bounded::<Notification>(NOTIFICATION_CAP);

    Core {
        handle: CoreHandle {
            actions: action_tx,
            deltas: delta_rx,
            snapshots: SnapshotCell { rx: snapshot_rx },
            notifications: note_rx,
        },
        run: Box::pin(run(
            action_rx,
            delta_tx,
            SnapshotSink { tx: snapshot_tx },
            note_tx,
        )),
    }
}

/// The actor loop. M1 handles motion + edit + snapshot + quit; M1+ adds a
/// `select!` over LSP/FS channels alongside this `recv`.
async fn run(
    actions: Receiver<Action>,
    deltas: Sender<Delta>,
    snapshots: SnapshotSink,
    notifications: Sender<Notification>,
) {
    let mut editor = Editor::new();

    while let Ok(action) = actions.recv().await {
        // Map each action to what the loop must do: an edit to apply, a pure
        // republish (motion / snapshot request), or a stop. The three text-edit
        // actions then share one apply_edit call instead of repeating the
        // apply/break plumbing per variant.
        let step = match action {
            Action::Insert(text) => Step::Edit(EditKind::Insert(text)),
            Action::DeleteBackward => Step::Edit(EditKind::DeleteBackward),
            Action::DeleteForward => Step::Edit(EditKind::DeleteForward),
            Action::MoveCursor { motion, extend } => {
                editor.move_cursor(motion, extend);
                Step::Republish
            }
            Action::RequestSnapshot => Step::Republish,
            Action::Quit => break,
        };

        let alive = match step {
            Step::Edit(kind) => {
                apply_edit(&mut editor, kind, &deltas, &snapshots, &notifications).await
            }
            Step::Republish => snapshots.publish(editor.snapshot(None)),
        };
        if !alive {
            break;
        }
    }

    // Best-effort, non-blocking: the frontend may be gone or not draining - either
    // way we are shutting down, so never await here (a full channel must not stall
    // shutdown).
    let _ = notifications.try_send(Notification::ShuttingDown);
}

/// Apply an edit action end to end: plan the per-selection edits, apply them
/// back-to-front to the buffer, emit each as a `Delta`, remap selections, bump the
/// version, and publish a snapshot. Returns `false` if the frontend has hung up
/// (caller then breaks the loop).
///
/// A rejected edit (bad range) is surfaced as a `Notification` and skipped without
/// bumping the version - the buffer never silently changes (SPEC §8). Because
/// ranges come from the current selection set and the buffer they are validated
/// against, rejection is not expected in M1, but the path is handled not panicked.
async fn apply_edit(
    editor: &mut Editor,
    kind: EditKind,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let edits = editor.plan_edit(kind);
    if edits.is_empty() {
        // No-op (e.g. backspace at buffer start): republish so the frontend's
        // view stays current, but do not bump the version or emit a delta.
        return snapshots.publish(editor.snapshot(None));
    }

    let base_version = editor.version;
    let mut dirty: Option<std::ops::Range<usize>> = None;

    // Edits are sorted descending by start, so applying in order is offset-stable.
    for (range, new_text) in &edits {
        if let Err(err) = editor.buffer.replace(range.clone(), new_text) {
            // Surface and skip this one edit; keep the buffer intact (SPEC §8).
            // The notification is self-contained (carries buffer + version).
            let _ = notifications.try_send(Notification::EditRejected {
                buffer_id: editor.id,
                version: editor.version,
                message: err.to_string(),
            });
            continue;
        }
        // A Delta is expressed against the base (pre-edit) version. Emitting one
        // per sub-edit keeps the lossless log exact for a remote frontend.
        let delta = Delta {
            buffer_id: editor.id,
            base_version,
            range: range.clone(),
            new_text: new_text.clone(),
        };
        if deltas.send(delta).await.is_err() {
            return false; // frontend gone
        }
        dirty = Some(match dirty {
            None => range.start..range.start + new_text.len(),
            Some(d) => d.start.min(range.start)..d.end.max(range.start + new_text.len()),
        });
    }

    // Remap selections to sit at the end of each applied edit (a cursor after the
    // inserted text / at the deletion point). Recomputed from the edits so the
    // set stays disjoint and sorted.
    editor.selections = selections_after_edits(&edits);
    editor.version += 1;
    snapshots.publish(editor.snapshot(dirty))
}

/// Cursor positions after applying `edits` (sorted descending by start). Each edit
/// leaves a cursor just past its inserted text. Rebuilt as a fresh set so the
/// disjoint+sorted invariant holds.
fn selections_after_edits(edits: &[(std::ops::Range<usize>, String)]) -> SelectionSet {
    // `edits` is descending; walk ascending so cumulative offset shifts compose.
    let mut ascending: Vec<&(std::ops::Range<usize>, String)> = edits.iter().collect();
    ascending.sort_by_key(|(r, _)| r.start);

    let mut shift: isize = 0;
    let mut cursors: Vec<Selection> = Vec::with_capacity(ascending.len());
    for (range, new_text) in ascending {
        let start = (range.start as isize + shift) as usize;
        let caret = start + new_text.len();
        cursors.push(Selection::cursor(caret));
        // This edit removed `range.len()` bytes and added `new_text.len()`.
        shift += new_text.len() as isize - (range.end - range.start) as isize;
    }

    // Build the set directly; positions are already sorted ascending and an edit's
    // caret cannot overlap the next edit's shifted range.
    SelectionSet::from_sorted_cursors(cursors)
}

#[cfg(test)]
mod tests {
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
        let set =
            SelectionSet::from_sorted_cursors(vec![Selection::cursor(1), Selection::cursor(4)]);
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
        let set =
            SelectionSet::from_sorted_cursors(vec![Selection::cursor(2), Selection::cursor(5)]);
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
}
