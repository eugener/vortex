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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_channel::{Receiver, Sender};
use futures::future::Either;

use crate::action::Action;
use crate::anchor::{Anchor, Edit};
use crate::buffer::{Buffer, RopeBuffer};
use crate::decoration::DecorationSet;
use crate::history::{Change, History, Reverted};
use crate::lsp::{Diagnostic, DocumentSync, LspEvent, LspHandle, convert};
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
    /// frontend -> core, a language server to attach at runtime (SPEC §2.3: other
    /// subsystems reach the single owner by message, never a shared handle). The
    /// frontend spawns the client loop on its own executor and sends the resulting
    /// [`LspHandle`] here; the core swaps it in and re-announces the current buffer.
    /// A later handle replaces an earlier one, so opening a file in a different
    /// workspace re-roots the server. Bounded and low-volume (one per attach).
    pub lsp: Sender<LspHandle>,
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
    /// The file this buffer is bound to (`Open`/`Save`), or `None` if unnamed.
    path: Option<PathBuf>,
    /// The undo tree (SPEC §2.4). Owns the reversible change history and the
    /// coalescing state; reset on a file open (undo does not cross a load).
    history: History,
    /// The clipboard register: one entry per selection copied/cut, in selection
    /// order (SPEC §11). The core owns this state so a multi-cursor copy round-trips
    /// per-cursor on paste; the frontend mirrors a flattened form to the OS clipboard
    /// via `Notification::SetClipboard`. Survives file opens (a yank is not tied to a
    /// buffer). Empty until the first copy/cut.
    register: Vec<String>,
    /// Everything the frontend paints at a position (SPEC §5): LSP diagnostics
    /// now, syntax highlights and git signs later. Held behind an `Arc` so
    /// publishing a snapshot is a ref-count bump rather than a deep clone of
    /// every span, and transformed through each edit so overlays keep pointing at
    /// the right text between a producer's refreshes.
    decorations: Arc<DecorationSet>,
    /// editor -> language server, or `None` when no server is attached (the
    /// common case). Everything LSP-related is an `Option` rather than a separate
    /// code path so a buffer with no server pays nothing and behaves identically.
    lsp_sync: Option<Sender<DocumentSync>>,
    /// The buffer changed but the server has not been told yet - either because
    /// the sync channel was momentarily full, or because an edit just landed.
    ///
    /// A flag rather than a queued message, and this is the payoff of full-text
    /// sync (see [`DocumentSync`]): re-sending the *current* buffer subsumes every
    /// missed intermediate state, so a dropped sync can never desync the server.
    /// It also means the actor never awaits the sync channel, which would deadlock
    /// against the server task awaiting the event channel.
    lsp_dirty: bool,
    /// Whether the server has been told this file exists (`didOpen`). A change
    /// notification for an unopened document is a protocol error.
    lsp_opened: bool,
}

impl Editor {
    fn new() -> Self {
        Self {
            id: BufferId(0),
            buffer: RopeBuffer::new(),
            selections: SelectionSet::at_origin(),
            version: 0,
            path: None,
            history: History::new(),
            register: Vec::new(),
            decorations: Arc::new(DecorationSet::new()),
            lsp_sync: None,
            lsp_dirty: false,
            lsp_opened: false,
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
            decorations: Arc::clone(&self.decorations),
            path: self.path.clone(),
            modified: self.modified(),
        }
    }

    /// Whether the buffer differs from its on-disk file. Derived from `history`'s
    /// save point - never stored - so no edit/undo/open/save path can forget to
    /// sync a cached copy, and undoing back to the saved state clears it
    /// (SPEC §8, §10).
    fn modified(&self) -> bool {
        !self.history.at_saved()
    }

    /// Apply `motion` to the selection set. Pure state change, no delta - motion
    /// does not alter buffer text, so no version bump and no delta emission.
    fn move_cursor(&mut self, motion: crate::selection::Motion, extend: bool) {
        let text = self.buffer.text();
        self.selections.move_all(&text, motion, extend);
    }

    /// Place the caret at byte `offset` (a pointer click). Like [`Self::move_cursor`]
    /// this only moves the selection set - no text change, so no delta or version
    /// bump.
    fn place_cursor(&mut self, offset: usize, extend: bool) {
        let text = self.buffer.text();
        self.selections.place(&text, offset, extend);
    }

    /// Add a cursor above (or below) the current set (SPEC §2.2). Pure selection
    /// change, like [`Self::move_cursor`]: no delta, no version bump.
    fn add_cursor_vertical(&mut self, above: bool) {
        let text = self.buffer.text();
        if above {
            self.selections.add_cursor_above(&text);
        } else {
            self.selections.add_cursor_below(&text);
        }
    }

    /// Add a cursor at byte `offset` (a modifier-click, SPEC §2.2), keeping the
    /// existing cursors. Pure selection change.
    fn add_cursor_at(&mut self, offset: usize) {
        let text = self.buffer.text();
        self.selections.add_cursor(&text, offset);
    }

    /// Collapse a multi-cursor set back to the primary selection alone (Escape,
    /// SPEC §2.2). Pure selection change; no buffer access needed.
    fn collapse_selections(&mut self) {
        self.selections.collapse_to_primary();
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

    /// Copy every non-empty selection's text into the register (SPEC §11), one
    /// entry per selection in selection order (the set is sorted, so this is the
    /// on-screen top-to-bottom order). Returns `true` if anything was copied - a set
    /// of bare cursors selects nothing, leaves the register untouched, and returns
    /// `false` so the caller emits no clipboard notification. Reads text via
    /// [`Text::slice`], which is bounded to the selected bytes, never the whole file.
    fn fill_register(&mut self) -> bool {
        let text = self.buffer.text();
        let slices: Vec<String> = self
            .selections
            .all()
            .iter()
            .filter(|sel| !sel.is_cursor())
            .map(|sel| text.slice(sel.start()..sel.end()))
            .collect();
        if slices.is_empty() {
            return false;
        }
        self.register = slices;
        true
    }

    /// Move every decoration across the applied `changes` (SPEC §5). Skips the
    /// `Arc` clone entirely when nothing is decorated - the overwhelmingly common
    /// case of a buffer with no LSP attached, which must not pay for this at all.
    fn transform_decorations(&mut self, changes: &[Change]) {
        if self.decorations.is_empty() || changes.is_empty() {
            return;
        }
        let edits = edits_from_changes(changes);
        // `make_mut` clones only while a published snapshot still shares the set;
        // once the frontend drops that snapshot this mutates in place.
        Arc::make_mut(&mut self.decorations).transform_through(&edits);
    }

    /// Tell the language server about the buffer's current contents, if one is
    /// attached and anything is outstanding (SPEC §5 full-text sync).
    ///
    /// Never awaits: a full sync channel leaves `lsp_dirty` set and the next call
    /// re-sends the newest text, which is why dropping the attempt is safe.
    fn sync_lsp(&mut self) {
        let (Some(sync), Some(path)) = (&self.lsp_sync, &self.path) else {
            return;
        };
        if !self.lsp_dirty {
            return;
        }
        let message = if self.lsp_opened {
            DocumentSync::Changed {
                path: path.clone(),
                version: self.version,
                text: self.buffer.text().to_string(),
            }
        } else {
            DocumentSync::Opened {
                path: path.clone(),
                language_id: language_id(path),
                text: self.buffer.text().to_string(),
            }
        };
        if sync.try_send(message).is_ok() {
            self.lsp_dirty = false;
            self.lsp_opened = true;
        }
    }

    /// Replace the LSP's decorations with `diagnostics`, resolved against the
    /// current buffer (SPEC §5). Ignores batches for a file this buffer is not
    /// showing - a server analyzes a whole workspace and publishes for any file
    /// in it, not just the open one.
    ///
    /// Returns whether anything changed, so the caller republishes only when the
    /// screen would actually differ.
    fn apply_diagnostics(&mut self, path: &Path, diagnostics: &[Diagnostic]) -> bool {
        if self.path.as_deref() != Some(path) {
            return false;
        }
        let decorations = convert::decorations_for(&self.buffer.text(), diagnostics);
        let mut updated = (*self.decorations).clone();
        updated.replace(crate::decoration::DecorationSource::Lsp, decorations);
        if updated == *self.decorations {
            return false;
        }
        self.decorations = Arc::new(updated);
        true
    }

    /// The register flattened for the OS clipboard: entries joined with `\n` (SPEC
    /// §11). The OS clipboard is a single string, so the per-selection structure is
    /// collapsed here while the structured register stays in the core for paste.
    fn register_flattened(&self) -> String {
        self.register.join("\n")
    }

    /// Plan the per-cursor edits a `Paste` produces: each selection's span is
    /// replaced by the register text assigned to it (SPEC §11 distribution rule).
    /// With one register entry it goes to every cursor; with exactly as many entries
    /// as selections the i-th entry goes to the i-th selection (the multi-cursor
    /// round-trip); otherwise every cursor gets the whole register joined with `\n`.
    /// Returns edits sorted DESCENDING by start (as [`Self::plan_edit`]) so
    /// back-to-front application is offset-stable, or empty for an empty register.
    fn plan_paste(&self) -> Vec<(std::ops::Range<usize>, String)> {
        if self.register.is_empty() {
            return Vec::new();
        }
        let selections = self.selections.all();
        // The joined fallback applies only when counts are neither 1 nor equal;
        // build it once then, and not at all on the common paths.
        let joined = (self.register.len() != 1 && self.register.len() != selections.len())
            .then(|| self.register_flattened());
        let mut edits: Vec<(std::ops::Range<usize>, String)> = selections
            .iter()
            .enumerate()
            .map(|(i, sel)| {
                let insert = match &joined {
                    Some(j) => j.clone(),
                    None if self.register.len() == 1 => self.register[0].clone(),
                    None => self.register[i].clone(),
                };
                (sel.start()..sel.end(), insert)
            })
            .collect();
        edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
        edits
    }
}

/// The applied `changes` as anchor-transform edits: base coordinates, ascending
/// by start - the contract [`Anchor::transform_through`] takes. `changes` arrive
/// descending (the back-to-front application order), so this sorts a fresh copy.
/// Shared by selection remapping and decoration remapping, which must see the
/// same batch or a caret and the squiggle under it would drift apart.
fn edits_from_changes(changes: &[Change]) -> Vec<Edit> {
    let mut edits: Vec<Edit> = changes
        .iter()
        .map(|c| Edit {
            start: c.start,
            old_end: c.start + c.removed.len(),
            insert_len: c.inserted.len(),
        })
        .collect();
    edits.sort_by_key(|e| e.start);
    edits
}

/// The kind of text edit an action requests, resolved against each selection.
enum EditKind {
    /// Insert this text (replacing a non-empty selection).
    Insert(String),
    /// Delete backward one grapheme (or the selection if non-empty).
    DeleteBackward,
    /// Delete forward one grapheme (or the selection if non-empty).
    DeleteForward,
    /// Delete only non-empty selections (the cut edit). A bare cursor is a no-op,
    /// so cutting with nothing selected changes nothing - unlike backspace/delete,
    /// which step over a grapheme at a bare cursor.
    DeleteSelection,
}

/// What the actor loop must do for one action: apply a text edit, republish the
/// current state (a motion or snapshot request), or a file op (open/save). Each
/// path returns "is the frontend still alive?"; `Quit` breaks before this.
enum Step {
    /// Apply these pre-planned `(range, replacement)` edits (sorted descending by
    /// start). The dispatch arm plans them - from an `EditKind` for insert/delete/cut,
    /// or from the register for paste - so one apply path serves every text change.
    Edit(Vec<(std::ops::Range<usize>, String)>),
    Undo,
    Redo,
    Republish,
    Open(PathBuf),
    Save,
}

/// What the actor loop woke up for. The LSP arms exist so a server can push work
/// in without the user touching the keyboard (SPEC §2.3: other subsystems send
/// messages to the single owner rather than sharing its state).
enum Incoming {
    Action(Action),
    Lsp(LspEvent),
    /// A language server to attach (the frontend spawned its client and sent the
    /// handle). Replaces any current server and re-announces the buffer.
    Attach(LspHandle),
    /// The LSP *event* channel closed - the server or its task is gone.
    LspClosed,
    /// The frontend hung up; the loop should stop.
    Stopped,
}

/// Await whichever arrives first: a frontend action, a language-server event, or a
/// request to attach a (new) server.
///
/// `lsp` is `None` until a server attaches: selecting against a not-yet-connected
/// channel is impossible, and once one *does* attach, a closed event channel
/// returns ready forever, so it is dropped to `None` on close rather than spun on.
/// The attach channel is always selected - that is how the first server arrives.
/// Every `recv` future is cancel-safe, so the losers of a race are dropped and
/// re-created next iteration without losing a message.
async fn next_incoming(
    actions: &Receiver<Action>,
    lsp: Option<&Receiver<LspEvent>>,
    attach: &Receiver<LspHandle>,
) -> Incoming {
    // Race the frontend action against the LSP side (an attach, plus events once a
    // server is connected). Two nested two-way `select`s rather than the `select!`
    // macro: the macro's fused-future handling misbehaves for this loop, while
    // `future::select` is the same cancel-safe primitive already used elsewhere -
    // the loser is dropped and its `recv` re-created next call, losing no message.
    //
    // **Liveness is the ACTIONS channel alone.** A closed attach channel means the
    // frontend will simply never attach a server (a valid mode - and the one every
    // no-LSP core is in), so it must never stop the loop: `recv_attach` pends
    // forever once closed instead of resolving, leaving `actions` closing as the
    // one shutdown signal. Without this, a frontend that holds `actions` but drops
    // `lsp` (e.g. Rust 2021 disjoint capture never moving the unused sender in) kills
    // the editor the moment it next idles.
    let action = std::pin::pin!(actions.recv());
    let lsp_side = std::pin::pin!(async {
        match lsp {
            Some(events) => {
                let event = std::pin::pin!(events.recv());
                let attach = std::pin::pin!(recv_attach(attach));
                match futures::future::select(event, attach).await {
                    Either::Left((e, _)) => e.map_or(Incoming::LspClosed, Incoming::Lsp),
                    Either::Right((h, _)) => Incoming::Attach(h),
                }
            }
            None => Incoming::Attach(recv_attach(attach).await),
        }
    });
    match futures::future::select(action, lsp_side).await {
        Either::Left((a, _)) => a.map_or(Incoming::Stopped, Incoming::Action),
        Either::Right((incoming, _)) => incoming,
    }
}

/// Await the next server to attach, or pend forever if the attach channel has
/// closed (see [`next_incoming`]: a closed attach channel is not a shutdown, so it
/// must never resolve the select and stop the loop).
async fn recv_attach(attach: &Receiver<LspHandle>) -> LspHandle {
    match attach.recv().await {
        Ok(handle) => handle,
        Err(_) => std::future::pending().await,
    }
}

/// The LSP `languageId` for a path (the protocol's own identifiers, which are
/// not simply the file extension). An unknown extension falls back to the
/// extension itself: servers ignore documents they do not claim, so guessing
/// costs nothing, while refusing to guess would mean no server ever sees a file
/// type this list has not been taught.
fn language_id(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    match ext {
        "rs" => "rust",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" | "cxx" => "cpp",
        "md" => "markdown",
        other => other,
    }
    .to_string()
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
        // Cut deletes only what is selected; a bare cursor contributes nothing.
        EditKind::DeleteSelection => {
            (!sel.is_cursor()).then(|| (sel.start()..sel.end(), String::new()))
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

/// A loop the frontend must spawn, boxed so `vortex-core` exposes no executor
/// type. Defaults to `()` for the actor loop; the LSP client uses it to hand back
/// why it stopped (SPEC §8).
pub type BoxFuture<T = ()> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Latest-wins snapshot slot: capacity 1 (SPEC §6).
const SNAPSHOT_CAP: usize = 1;
/// Delta channel bound: lossless ordered log; sized to absorb bursts (SPEC §6).
const DELTA_CAP: usize = 1024;
/// Notification channel bound: discrete, low-volume events (SPEC §6).
const NOTIFICATION_CAP: usize = 64;
/// LSP-attach channel bound: a language server is attached rarely (once per file
/// type, on demand), so a small bound is plenty.
const LSP_ATTACH_CAP: usize = 4;

/// Create a core. Returns a [`CoreHandle`] and the actor loop to spawn.
///
/// `action_capacity` bounds the frontend -> core action channel, the
/// back-pressure-critical stream (SPEC §6). Other channels get their own fixed
/// bounds so sizing the action queue does not inflate them.
///
/// A language server can be attached later at runtime via [`CoreHandle::lsp`]
/// (the lazy, on-demand path a file open takes); [`with_lsp`] seeds one up front.
///
/// # Panics
/// Panics if `action_capacity` is 0 (a bounded channel needs capacity >= 1).
pub fn new(action_capacity: usize) -> Core {
    build(action_capacity, None)
}

/// Create a core with a language server already attached (SPEC §3, M2). `lsp`
/// comes from [`crate::lsp::client`], whose loop the frontend spawns alongside
/// this one.
///
/// Sugar for [`new`] followed by sending `lsp` on [`CoreHandle::lsp`]: the core
/// attaches it on its first loop turn, so behavior is identical to a runtime
/// attach - there is one attach path, not two.
pub fn with_lsp(action_capacity: usize, lsp: LspHandle) -> Core {
    build(action_capacity, Some(lsp))
}

fn build(action_capacity: usize, lsp: Option<LspHandle>) -> Core {
    assert!(action_capacity > 0, "action_capacity must be >= 1");

    let (action_tx, action_rx) = async_channel::bounded::<Action>(action_capacity);
    let (delta_tx, delta_rx) = async_channel::bounded::<Delta>(DELTA_CAP);
    let (snapshot_tx, snapshot_rx) = async_channel::bounded::<ViewSnapshot>(SNAPSHOT_CAP);
    let (note_tx, note_rx) = async_channel::bounded::<Notification>(NOTIFICATION_CAP);
    let (lsp_tx, lsp_rx) = async_channel::bounded::<LspHandle>(LSP_ATTACH_CAP);

    // Seed an initial server, if given, down the same channel a runtime attach
    // uses. `try_send` cannot fail: the channel is fresh and bounded >= 1.
    if let Some(handle) = lsp {
        let _ = lsp_tx.try_send(handle);
    }

    Core {
        handle: CoreHandle {
            actions: action_tx,
            deltas: delta_rx,
            snapshots: SnapshotCell { rx: snapshot_rx },
            notifications: note_rx,
            lsp: lsp_tx,
        },
        run: Box::pin(run(
            action_rx,
            delta_tx,
            SnapshotSink { tx: snapshot_tx },
            note_tx,
            lsp_rx,
        )),
    }
}

/// Mirror the register to the OS clipboard: fill it from the selections and, if
/// anything was copied, emit `SetClipboard`. Shared by Copy and Cut, which differ
/// only in their follow-up step. Lives in the actor (not on `Editor`) so the
/// notifications channel stays a transport concern, not core state.
fn mirror_register(editor: &mut Editor, notifications: &Sender<Notification>) {
    if editor.fill_register() {
        let _ = notifications.try_send(Notification::SetClipboard {
            text: editor.register_flattened(),
        });
    }
}

/// The actor loop. M1 handles motion + edit + snapshot + quit; M1+ adds a
/// `select!` over LSP/FS channels alongside this `recv`.
async fn run(
    actions: Receiver<Action>,
    deltas: Sender<Delta>,
    snapshots: SnapshotSink,
    notifications: Sender<Notification>,
    lsp_attach: Receiver<LspHandle>,
) {
    let mut editor = Editor::new();
    // The event side of the attached server, or `None` until one attaches. The
    // send side lives on `editor.lsp_sync`, so both are swapped together on attach.
    let mut lsp_events: Option<Receiver<LspEvent>> = None;

    loop {
        // Flush any outstanding document sync before parking on input, so the
        // server sees the newest text while the user is idle rather than only
        // once they press another key.
        editor.sync_lsp();

        // Bound the borrow of `lsp_events` to this statement so the arms below
        // can clear it.
        let incoming = next_incoming(&actions, lsp_events.as_ref(), &lsp_attach).await;
        let action = match incoming {
            Incoming::Stopped => break,
            // A (new) server attached: swap in both channel ends and re-announce
            // the current buffer to it (a fresh `didOpen`), so a file already open
            // when the server arrives is analyzed too. Replacing an earlier server
            // drops its channels, which stops its client loop (SPEC §8).
            Incoming::Attach(handle) => {
                editor.lsp_sync = Some(handle.sync);
                lsp_events = Some(handle.events);
                editor.lsp_opened = false;
                editor.lsp_dirty = true;
                continue;
            }
            // The server died, or its task ended. That must never take the editor
            // with it (SPEC §8): fall back to the no-LSP path, which is also what
            // keeps `select` from spinning on a permanently-ready closed channel.
            // Drop the send side too, so a stale sync never targets a dead server.
            Incoming::LspClosed => {
                lsp_events = None;
                editor.lsp_sync = None;
                editor.lsp_opened = false;
                continue;
            }
            Incoming::Lsp(LspEvent::Diagnostics { path, diagnostics }) => {
                // Republish only when the screen would actually differ: a server
                // re-sending an identical batch (common while indexing) must not
                // cost a frame.
                if editor.apply_diagnostics(&path, &diagnostics)
                    && !snapshots.publish(editor.snapshot(None))
                {
                    break;
                }
                continue;
            }
            Incoming::Action(action) => action,
        };

        // Map each action to what the loop must do: an edit to apply, a pure
        // republish (motion / snapshot request), or a stop. The three text-edit
        // actions then share one apply_edit call instead of repeating the
        // apply/break plumbing per variant.
        let step = match action {
            Action::Insert(text) => Step::Edit(editor.plan_edit(EditKind::Insert(text))),
            Action::DeleteBackward => Step::Edit(editor.plan_edit(EditKind::DeleteBackward)),
            Action::DeleteForward => Step::Edit(editor.plan_edit(EditKind::DeleteForward)),
            // Copy fills the register but touches no text: emit the clipboard mirror
            // (if anything was selected) and republish, no delta or version bump.
            Action::Copy => {
                mirror_register(&mut editor, &notifications);
                Step::Republish
            }
            // Cut = copy + delete the selections, as one edit / one undo unit. Fill
            // the register and emit the mirror first, then plan the deletion; a set
            // of bare cursors selects nothing, so `plan_edit` returns no edits and
            // the apply path treats it as a no-op.
            Action::Cut => {
                mirror_register(&mut editor, &notifications);
                Step::Edit(editor.plan_edit(EditKind::DeleteSelection))
            }
            // Paste distributes the register over the cursors (SPEC §11); an empty
            // register plans no edits and is a clean no-op. A paste is a distinct
            // action, not a keystroke, so it ends any typing-coalescing run - the one
            // break `History` cannot infer, since a paste leaves the carets exactly
            // where typing would and a single-character payload is indistinguishable
            // from a keystroke at the `Change` level (SPEC §2.4).
            Action::Paste => {
                editor.history.break_coalescing();
                Step::Edit(editor.plan_paste())
            }
            // The selection-changing actions need no coalescing bookkeeping: every
            // edit carries the selection set it started from, so `History` sees the
            // caret moved and ends the typing run itself (SPEC §2.4 break rule (d)).
            // A new selection action added here inherits that for free.
            Action::MoveCursor { motion, extend } => {
                editor.move_cursor(motion, extend);
                Step::Republish
            }
            Action::PlaceCursor { offset, extend } => {
                editor.place_cursor(offset, extend);
                Step::Republish
            }
            Action::AddCursorAbove => {
                editor.add_cursor_vertical(true);
                Step::Republish
            }
            Action::AddCursorBelow => {
                editor.add_cursor_vertical(false);
                Step::Republish
            }
            Action::AddCursorAt { offset } => {
                editor.add_cursor_at(offset);
                Step::Republish
            }
            Action::CollapseSelections => {
                editor.collapse_selections();
                Step::Republish
            }
            Action::Undo => Step::Undo,
            Action::Redo => Step::Redo,
            Action::RequestSnapshot => Step::Republish,
            Action::Open(path) => Step::Open(path),
            Action::Save => Step::Save,
            Action::Quit => break,
        };

        let alive = match step {
            Step::Edit(edits) => {
                apply_edit(&mut editor, edits, &deltas, &snapshots, &notifications).await
            }
            Step::Undo => {
                let reverted = editor.history.undo();
                reapply(&mut editor, reverted, &deltas, &snapshots, &notifications).await
            }
            Step::Redo => {
                let reverted = editor.history.redo();
                reapply(&mut editor, reverted, &deltas, &snapshots, &notifications).await
            }
            Step::Republish => snapshots.publish(editor.snapshot(None)),
            Step::Open(path) => {
                open_file(&mut editor, path, &deltas, &snapshots, &notifications).await
            }
            Step::Save => save_file(&mut editor, &snapshots, &notifications).await,
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

/// Apply an edit action end to end: given the pre-planned per-selection edits, apply
/// them, record the reversible revision for undo (SPEC §2.4), remap selections, bump
/// the version, and publish a snapshot. The dispatch arm plans `edits` (from an
/// `EditKind` for insert/delete/cut, or from the register for paste), so this one
/// path serves every text change. Returns `false` if the frontend has hung up (caller
/// then breaks the loop).
///
/// A rejected edit (bad range) is surfaced as a `Notification` and skipped without
/// bumping the version - the buffer never silently changes (SPEC §8). Because
/// ranges come from the current selection set and the buffer they are validated
/// against, rejection is not expected in M1, but the path is handled not panicked.
async fn apply_edit(
    editor: &mut Editor,
    edits: Vec<(std::ops::Range<usize>, String)>,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    if edits.is_empty() {
        // No-op (e.g. backspace at buffer start): republish so the frontend's
        // view stays current, but do not bump the version or emit a delta.
        return snapshots.publish(editor.snapshot(None));
    }

    // Snapshot the selections *before* the edit so undo can restore them.
    let before = editor.selections.clone();
    let Some((changes, dirty)) = apply_change_list(editor, &edits, deltas, notifications).await
    else {
        return false; // frontend gone mid-stream
    };

    // If every planned edit was rejected (or was a true no-op), nothing changed:
    // do not bump the version or record history (a version bump with no delta
    // would diverge a remote frontend replaying the delta stream, SPEC §5).
    if changes.is_empty() {
        return snapshots.publish(editor.snapshot(None));
    }

    // Remap selections by transforming each pre-edit caret through the applied
    // edits (SPEC §2.1 anchors): a cursor lands after its own inserted text / at its
    // deletion point, and every other cursor shifts by the edits around it.
    editor.selections = selections_after_edits(&before, &changes);
    // Decorations ride the same batch, so a squiggle stays under the token it
    // flagged while the producer catches up (SPEC §5).
    editor.transform_decorations(&changes);
    editor.version += 1;
    // One user action is one undo unit, even when it fanned across N cursors
    // (SPEC §2.4). Coalescing (single-caret typing) is decided inside `record`.
    editor
        .history
        .record(changes, before, editor.selections.clone());
    // The server's copy is now stale; `sync_lsp` re-sends before the next park.
    editor.lsp_dirty = true;
    snapshots.publish(editor.snapshot(dirty))
}

/// Apply an undo or redo, sharing the "apply edits + restore selections + publish"
/// tail. `reverted` is the move the history already produced (`History::undo` /
/// `History::redo`): the edits to apply against the current buffer plus the
/// selections to restore, or `None` at a branch end (nothing to undo/redo), a clean
/// no-op. Undo/redo *are* edits on the wire: they emit deltas and bump the version
/// like any change, so a remote frontend replaying the log converges (SPEC §5) - it
/// has no notion of "undo", only more buffer edits moving forward in version time.
async fn reapply(
    editor: &mut Editor,
    reverted: Option<Reverted>,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let Some(reverted) = reverted else {
        // Nothing to undo/redo: republish so the view stays current, no version bump.
        return snapshots.publish(editor.snapshot(None));
    };

    let Some((changes, dirty)) =
        apply_change_list(editor, &reverted.edits, deltas, notifications).await
    else {
        return false; // frontend gone
    };
    // Inverse/forward edits derived from a consistent history over this buffer
    // always apply cleanly, so `changes` is non-empty here; guard the version bump
    // anyway so a would-be no-op never advances the version without a delta.
    if !changes.is_empty() {
        editor.version += 1;
    }
    editor.transform_decorations(&changes);
    editor.lsp_dirty = true;
    editor.selections = reverted.selections;
    snapshots.publish(editor.snapshot(dirty))
}

/// Apply `edits` (each `(range, replacement)`, pre-sorted DESCENDING by start so
/// back-to-front application is offset-stable) to the buffer, emitting one `Delta`
/// per applied edit and capturing the removed text so the caller can build an undo
/// revision. Returns the applied [`Change`]s and the merged dirty range, or `None`
/// if the frontend hung up. A rejected edit is surfaced and skipped (SPEC §8); a
/// true no-op edit (empty range and empty text) is dropped so it never produces a
/// degenerate delta or revision. Version and selection updates are the caller's job
/// - `apply_edit` remaps to the edit ends, undo/redo restore saved selections.
async fn apply_change_list(
    editor: &mut Editor,
    edits: &[(std::ops::Range<usize>, String)],
    deltas: &Sender<Delta>,
    notifications: &Sender<Notification>,
) -> Option<(Vec<Change>, Option<std::ops::Range<usize>>)> {
    // Deltas are expressed against the pre-edit version; no edit here bumps it
    // (the caller does, once, after this returns), so read it once up front.
    let base_version = editor.version;
    let mut changes: Vec<Change> = Vec::with_capacity(edits.len());
    let mut dirty: Option<std::ops::Range<usize>> = None;

    for (range, new_text) in edits {
        // Drop a pure no-op (replace nothing with nothing): it would emit an empty
        // delta and record an empty revision, both meaningless.
        if range.is_empty() && new_text.is_empty() {
            continue;
        }
        let removed = match editor.buffer.replace(range.clone(), new_text) {
            Ok(removed) => removed,
            Err(err) => {
                // Surface and skip this one edit; keep the buffer intact (SPEC §8).
                let _ = notifications.try_send(Notification::EditRejected {
                    buffer_id: editor.id,
                    version: editor.version,
                    message: err.to_string(),
                });
                continue;
            }
        };
        // A Delta is expressed against the base (pre-edit) version. Emitting one
        // per sub-edit keeps the lossless log exact for a remote frontend.
        let delta = Delta {
            buffer_id: editor.id,
            base_version,
            range: range.clone(),
            new_text: new_text.clone(),
        };
        if deltas.send(delta).await.is_err() {
            return None; // frontend gone
        }
        changes.push(Change {
            start: range.start,
            removed,
            inserted: new_text.clone(),
        });
        dirty = Some(match dirty {
            None => range.start..range.start + new_text.len(),
            Some(d) => d.start.min(range.start)..d.end.max(range.start + new_text.len()),
        });
    }

    Some((changes, dirty))
}

/// Load `path` into the buffer, replacing its contents (SPEC §12.2 file
/// lifecycle). Expressed as one whole-buffer `Delta` so the delta stream still
/// reproduces the snapshot (SPEC §5). A missing file is not an error: it binds
/// the path to a fresh empty buffer, created on the first `Save` (Vim's
/// behavior). Any other read failure (permissions, non-UTF-8) is surfaced as a
/// `Notification` and leaves state unchanged (SPEC §8). Returns `false` if the
/// frontend has hung up.
///
/// File I/O is blocking `std::fs` on the actor thread: acceptable for a discrete
/// user action (not the per-keystroke hot path). Moving large loads off the
/// critical path via a background read (SPEC §2.3) is an M5 refinement.
async fn open_file(
    editor: &mut Editor,
    path: PathBuf,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    // `read_to_string` folds read + UTF-8 decode into one step: it errors with
    // `InvalidData` ("stream did not contain valid UTF-8") on non-text input, so a
    // single match covers missing / unreadable / non-UTF-8 without a nested one.
    let (contents, existed) = match std::fs::read_to_string(&path) {
        Ok(text) => (text, true),
        // Missing file: open an empty buffer bound to the path (created on save).
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (String::new(), false),
        Err(err) => {
            return report_file_error(
                editor,
                Some(path),
                &err.to_string(),
                snapshots,
                notifications,
            );
        }
    };

    // Replace the whole buffer as one Delta. Skip the delta/version bump when
    // nothing actually changes (empty buffer, empty file) so `version` still
    // advances iff a delta was emitted - the invariant the property test guards.
    // The load builds a fresh buffer rather than calling the fallible `replace`:
    // a whole-buffer swap has no range to reject, so there is no error path to
    // handle here (the delta still records the change for SPEC §5 replay).
    let old_len = editor.buffer.byte_len();
    let changed = old_len != 0 || !contents.is_empty();
    if changed {
        let base_version = editor.version;
        editor.buffer = RopeBuffer::from(contents.as_str());
        let delta = Delta {
            buffer_id: editor.id,
            base_version,
            range: 0..old_len,
            new_text: contents,
        };
        if deltas.send(delta).await.is_err() {
            return false; // frontend gone
        }
        editor.version += 1;
    }

    // A freshly opened buffer starts at the origin and matches disk. Undo does not
    // cross a load, so the history is reset to a fresh tree rooted at the loaded
    // content, which is the saved state (SPEC §2.4).
    editor.selections = SelectionSet::at_origin();
    editor.path = Some(path.clone());
    editor.history = History::new();
    // Decorations describe the *previous* file's text; keeping them would paint
    // squiggles at meaningless offsets until a producer refreshes.
    editor.decorations = Arc::new(DecorationSet::new());
    // A different file is a different document to the server: announce it fresh
    // rather than sending a change against the old one's identity.
    editor.lsp_dirty = true;
    editor.lsp_opened = false;

    let _ = notifications.try_send(Notification::FileOpened {
        buffer_id: editor.id,
        path,
        existed,
    });
    // `dirty` is a "what changed" repaint hint, so it is `None` when no delta was
    // emitted (a missing/empty file); reporting a spurious `Some(0..0)` would tell
    // a frontend an edit happened where none did (view.rs contract).
    let dirty = changed.then(|| 0..editor.buffer.byte_len());
    snapshots.publish(editor.snapshot(dirty))
}

/// Write the buffer to its bound file atomically (SPEC §8). Fails with a
/// `Notification` if no path is bound (save-as arrives with the prompt UI) or the
/// write fails; on failure the buffer stays dirty so no work is lost. Returns
/// `false` if the frontend has hung up.
async fn save_file(
    editor: &mut Editor,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let Some(path) = editor.path.clone() else {
        return report_file_error(
            editor,
            None,
            "no file name (save-as not available yet)",
            snapshots,
            notifications,
        );
    };

    let contents = editor.buffer.text().to_string();
    if let Err(message) = write_atomic(&path, contents.as_bytes()) {
        return report_file_error(editor, Some(path), &message, snapshots, notifications);
    }

    // Mark the current history node as the on-disk state, so undoing back to it
    // later clears the modified marker (SPEC §2.4, §8).
    editor.history.mark_saved();
    let _ = notifications.try_send(Notification::FileSaved {
        buffer_id: editor.id,
        path,
    });
    snapshots.publish(editor.snapshot(None))
}

/// Emit a `FileError` and republish current state, leaving the buffer untouched
/// (SPEC §8: a failed file op never loses work). Returns the publish's liveness so
/// callers can `return report_file_error(...)` directly.
fn report_file_error(
    editor: &Editor,
    path: Option<PathBuf>,
    message: &str,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let _ = notifications.try_send(Notification::FileError {
        buffer_id: editor.id,
        path,
        message: message.to_string(),
    });
    snapshots.publish(editor.snapshot(None))
}

/// Write `bytes` to `path` atomically: write a sibling temp file, flush it, then
/// rename it over the target (SPEC §8). A rename within a directory is atomic on
/// POSIX, so a reader never sees a half-written file and a failed write leaves the
/// original intact. Returns a human-readable error string on any I/O failure.
///
/// Preserving what a naive temp+rename would destroy:
/// - **Symlinks:** if `path` exists it is `canonicalize`d first, so we write
///   *through* a symlink to its real target and rename over that - a symlinked
///   dotfile stays a symlink pointing at the updated file, rather than being
///   replaced by a standalone regular file.
/// - **Permissions:** the existing file's mode is copied onto the temp before the
///   rename, so a save never silently widens a `0600` file to `0644` or drops an
///   executable bit. A brand-new file keeps `File::create`'s default mode.
/// - **Durability:** the containing directory is fsync'd after the rename so the
///   directory-entry change survives a crash, not just the file's data.
///
/// **Known limitation (M5):** a *hard-linked* file is still detached by the rename
/// (the other links stop reflecting edits). Truly preserving hard links needs
/// in-place copy-write, which trades away the crash-atomicity above - a deliberate
/// M5 `backupcopy`-style trade-off, not handled here.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::fs;
    use std::io::Write;

    // Resolve symlinks so the write lands on the real file and the rename replaces
    // *it*, not the link. A not-yet-existing file has no link to follow, so keep
    // the path as given (its parent dir must already exist to hold the temp).
    let existed = fs::symlink_metadata(path).is_ok();
    let target = if existed {
        match fs::canonicalize(path) {
            Ok(real) => real,
            // `path` exists (symlink_metadata succeeded) but a component of the
            // resolved path does not: a symlink whose target has not been created
            // yet (e.g. `~/.vimrc -> dotfiles/vimrc` before the first save).
            // Resolve the link by hand and write *through* it so the target is
            // created and the link stays intact, matching vim's behavior.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let link_target = fs::read_link(path).map_err(|e| e.to_string())?;
                if link_target.is_absolute() {
                    link_target
                } else {
                    // A relative link resolves against the link's own directory.
                    path.parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(link_target)
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    } else {
        path.to_path_buf()
    };
    // Temp file must sit in the target's directory so the rename stays on one
    // filesystem (a cross-device rename is not atomic and errors). A bare file
    // name has an empty parent, meaning the current directory.
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .ok_or_else(|| "path has no file name".to_string())?;

    // Unique temp name (pid + a per-process counter) so two vortex processes - or
    // a stale temp from a crashed prior save - never collide on the same sibling.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp = dir.to_path_buf();
    tmp.push(format!(
        ".{}.vortex-tmp-{}-{}",
        file_name.to_string_lossy(),
        std::process::id(),
        n
    ));

    // Write + flush, copy the existing mode, then rename over the target. The
    // inner block drops the file handle before the rename (renaming an open file
    // fails on Windows). Any failure shares one cleanup: remove the temp, leaving
    // the original intact (SPEC §8).
    // The target's current mode (if it exists; `None` for a new file or a first
    // save through a dangling symlink), so the temp is *created* no wider than
    // the destination - a 0600 file's contents must never touch disk in a 0644
    // temp, even briefly, before being narrowed (that window would expose e.g.
    // an SSH key to any local user for the length of the write + fsync).
    let target_mode = fs::metadata(&target).ok().map(|m| m.permissions());
    let result = (|| -> std::io::Result<()> {
        {
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create_new(true);
            // On Unix, create the temp at the target's mode up front. umask can only
            // *remove* bits, so the temp is always <= the target mode during the
            // write; the explicit set_permissions below then restores the exact
            // bits. A new file gets OpenOptions' default (0o666 & ~umask), matching
            // the prior `File::create` behavior.
            #[cfg(unix)]
            if let Some(mode) = &target_mode {
                use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
                opts.mode(mode.mode());
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        // Restore the target's exact permission bits (best-effort: a failure here
        // should not abort an otherwise-good save). Needed because umask may have
        // stripped bits the target legitimately had at create time.
        if let Some(mode) = &target_mode {
            let _ = fs::set_permissions(&tmp, mode.clone());
        }
        fs::rename(&tmp, &target)
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&tmp); // best-effort cleanup
        return Err(err.to_string());
    }

    // fsync the directory so the rename is durable across a crash. Best-effort:
    // opening a directory as a file is not portable (fails on Windows), and the
    // save already succeeded logically once the rename returned.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Cursor positions after `changes` apply to the buffer they were computed against.
/// Each pre-edit selection's caret (its `head`) is an [`Anchor::after`] - it rides to
/// the right of inserted text - transformed through the applied edits (SPEC §2.1). So
/// one keystroke over N cursors lands N carets at once, and a cursor whose own edit
/// was a no-op (e.g. backspace at buffer start) still shifts with its neighbors'
/// edits instead of being dropped. Rebuilt as a fresh set so the disjoint+sorted
/// invariant holds: the pre-edit heads are ascending and the transform is monotonic,
/// so the results stay ordered (coincident carets merge).
fn selections_after_edits(before: &SelectionSet, changes: &[Change]) -> SelectionSet {
    let edits = edits_from_changes(changes);
    let cursors: Vec<Selection> = before
        .all()
        .iter()
        .map(|sel| Selection::cursor(Anchor::after(sel.head).transform_through(&edits).offset()))
        .collect();
    let mut set = SelectionSet::from_sorted_cursors(cursors);
    // Carry the primary across the edit: transform its caret the same way and keep
    // whichever surviving cursor lands there as primary, so the viewport follows the
    // cursor the user was on instead of snapping to the topmost caret.
    let primary_head = Anchor::after(before.primary().head)
        .transform_through(&edits)
        .offset();
    set.retarget_primary(primary_head);
    set
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;
