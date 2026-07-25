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
use crate::file::FileFormat;
use crate::history::{Change, History, Reverted};
use crate::lsp::{Diagnostic, DocumentSync, LspEvent, LspHandle, convert};
use crate::selection::{Selection, SelectionSet};
use crate::syntax::{HighlightSpan, SyntaxEvent, SyntaxHandle, SyntaxSync};
use crate::view::{BufferId, BufferInfo, Delta, Notification, ViewSnapshot};
use crate::watch::{FileEvent, WatchHandle, WatchRequest};

/// Whether a save appends a trailing newline to a buffer that lacks one - SPEC
/// §10.1's POSIX-style default. The buffer itself is never touched, so this can
/// never surface as a spurious unsaved change.
///
/// A constant rather than a setting because configuration is frontend-owned and its
/// loader is still to come (§10.5); this is the default that loader will fall back
/// to when the file says nothing.
const ENSURE_FINAL_NEWLINE: bool = true;

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
    /// frontend -> core, a file watcher to attach at runtime (SPEC §10.2). The
    /// third of the same shape as [`Self::lsp`] and [`Self::syntax`]: the frontend
    /// owns `notify` and its OS threads and sends the [`WatchHandle`] here; the
    /// core swaps it in and announces every open file to it, so attaching partway
    /// through a session needs no special case. Bounded and low-volume.
    pub watch: Sender<WatchHandle>,
    /// frontend -> core, a syntax highlighter to attach at runtime (M4). The exact
    /// twin of [`Self::lsp`]: the frontend loads a grammar, spawns the highlighter
    /// loop on its own executor, and sends the [`SyntaxHandle`] here; the core
    /// swaps it in and re-announces the current buffer for a first parse. A later
    /// handle replaces an earlier one, so reopening as a different file type
    /// re-highlights. Bounded and low-volume (one per attach).
    pub syntax: Sender<SyntaxHandle>,
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

/// One open document - everything that is *per-buffer* state. Versions, anchors
/// and history are per-buffer (SPEC §5), so an edit in one document never
/// invalidates another's. What deliberately does not live here is the state that
/// spans documents - the clipboard register and the producer channels - which
/// belongs to the owning [`Session`].
struct Document {
    id: BufferId,
    buffer: RopeBuffer,
    selections: SelectionSet,
    /// The document version (SPEC §2.1, §5). Advances only on an applied edit, so
    /// anchors and LSP `didChange` can key off it; a snapshot request does not
    /// change it.
    version: u64,
    /// The file this buffer is bound to (`Open`/`Save`), or `None` if unnamed.
    path: Option<PathBuf>,
    /// How this buffer's file is laid out on disk - encoding, BOM, line
    /// terminator (SPEC §10.1). Detected on load and applied on save, so a
    /// CRLF latin-1 file is written back as one rather than silently converted
    /// to LF UTF-8. An unnamed buffer carries the default (UTF-8, LF).
    format: FileFormat,
    /// Why this buffer refuses edits, or `None` if it does not (SPEC §10.3).
    /// Decided at load time and never inferred later, so the reason is available
    /// to explain the refusal rather than only the fact of it.
    read_only: Option<ReadOnly>,
    /// The state of the file the last time this document accounted for it - after
    /// a load, a save, or a reload (SPEC §10.2). `None` when no file is bound or
    /// it does not exist.
    ///
    /// This is what makes external-change detection usable rather than maddening:
    /// the editor's own save is by far the loudest source of change events, and a
    /// notification whose stamp matches what we just wrote is our own echo. It is
    /// also updated when a conflict is *reported*, so one external write raises one
    /// prompt however many events the platform decides to send for it.
    disk: Option<DiskStamp>,
    /// The undo tree (SPEC §2.4). Owns the reversible change history and the
    /// coalescing state; reset on a file open (undo does not cross a load).
    history: History,
    /// Everything the frontend paints at a position (SPEC §5): LSP diagnostics
    /// now, syntax highlights and git signs later. Held behind an `Arc` so
    /// publishing a snapshot is a ref-count bump rather than a deep clone of
    /// every span, and transformed through each edit so overlays keep pointing at
    /// the right text between a producer's refreshes.
    decorations: Arc<DecorationSet>,
    /// The buffer changed but the server has not been told yet - either because
    /// the sync channel was momentarily full, or because an edit just landed.
    ///
    /// A flag rather than a queued message, and this is the payoff of full-text
    /// sync (see [`DocumentSync`]): re-sending the *current* buffer subsumes every
    /// missed intermediate state, so a dropped sync can never desync the server.
    /// It also means the actor never awaits the sync channel, which would deadlock
    /// against the server task awaiting the event channel.
    lsp_dirty: bool,
    /// The buffer changed but the highlighter has not been told yet. Same role and
    /// same full-document-sync payoff as [`Self::lsp_dirty`]: re-sending the
    /// current buffer subsumes every missed state, so a dropped sync can never
    /// mis-color, and the actor never awaits the sync channel.
    syntax_dirty: bool,
}

/// What a file looked like the last time a document accounted for it: modification
/// time and length (SPEC §10.2).
///
/// Two cheap fields rather than a hash of the contents, which would mean reading
/// the whole file on every change notification. Length catches the same-second
/// rewrite that mtime alone would miss; a same-second rewrite of exactly the same
/// length still slips through, which is the residue of not hashing and is why the
/// stamp decides only whether to *ask*, never whether the buffer is correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskStamp {
    modified: std::time::SystemTime,
    len: u64,
}

impl DiskStamp {
    /// The stamp of an already-stat'd file, or `None` where the platform does not
    /// report a modification time - in which case nothing is ever suppressed,
    /// which costs a redundant reload of an unchanged buffer and no correctness.
    fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        Some(Self {
            modified: metadata.modified().ok()?,
            len: metadata.len(),
        })
    }

    /// Stat `path` and stamp it. `None` if it cannot be stat'd at all (it is gone).
    fn read(path: &Path) -> Option<Self> {
        Self::of(&std::fs::metadata(path).ok()?)
    }
}

/// Why a buffer will not accept edits (SPEC §10.3). Both reasons are decided when
/// the file is loaded; neither can be overridden yet - a `:set noreadonly`
/// equivalent is keymap vocabulary, not core state, and would need a deliberate
/// `Action` rather than an implicit one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadOnly {
    /// The file exists but this process cannot write it. Refusing up front beats
    /// discovering it after an hour of typing, which is what a save-time-only
    /// check would give (the save still fails safely - this is about telling the
    /// user first).
    Permissions,
    /// The decode replaced bytes it could not interpret, so the buffer is not a
    /// faithful copy of the file. Writing it back would replace those bytes with
    /// U+FFFD - a silent corruption of data the user never touched (SPEC §8), and
    /// the one case where "you may look but not save" is exactly right.
    Undecodable,
}

impl ReadOnly {
    /// Why the edit was refused, for the notification the frontend shows.
    fn message(self) -> &'static str {
        match self {
            ReadOnly::Permissions => "file is read-only",
            ReadOnly::Undecodable => {
                "read-only: parts of this file could not be decoded, and saving would overwrite them"
            }
        }
    }
}

/// A live connection to a language server, and which documents *this* connection
/// has been told about.
///
/// The set lives here rather than as a flag on each [`Document`] because "has been
/// announced" is a fact about the **connection**, not about the buffer: replacing the
/// server invalidates every one of them at once. Holding it here makes that true by
/// construction - a new connection starts with an empty set, so nothing has to
/// remember to walk the documents and clear a flag. Getting that walk wrong is
/// exactly how a buffer ended up silently unanalyzed once multi-buffer landed.
struct LspConnection {
    /// editor -> server (SPEC §5 full-text sync).
    sync: Sender<DocumentSync>,
    /// Documents this connection has had a `didOpen` for. A change notification for
    /// a document a server has not opened is a protocol error, and the client drops
    /// it, so this decides which of the two a sync sends.
    opened: std::collections::HashSet<BufferId>,
}

impl LspConnection {
    fn new(sync: Sender<DocumentSync>) -> Self {
        Self {
            sync,
            opened: std::collections::HashSet::new(),
        }
    }

    /// Forget that `id` was announced, so its next sync opens it afresh. Used when a
    /// document's *identity* changes under a stable id (a scratch buffer adopting a
    /// path, a save-as onto a new name) and when it closes.
    fn forget(&mut self, id: BufferId) {
        self.opened.remove(&id);
    }
}

/// Owns all editor state: every open [`Document`] plus the state that spans them.
/// Never shared; lives inside the actor loop (SPEC §2.3 single owner).
struct Session {
    /// Every open document, in the order a frontend lists them. A `Vec` rather
    /// than a map because the order *is* the bufferline order and lookups are over
    /// a handful of entries. Never empty: the session always holds at least one
    /// document, so [`Self::active`] is always a valid index.
    docs: Vec<Document>,
    /// Index into [`Self::docs`] of the document actions apply to.
    active: usize,
    /// The clipboard register: one entry per selection copied/cut, in selection
    /// order (SPEC §11). The core owns this state so a multi-cursor copy round-trips
    /// per-cursor on paste; the frontend mirrors a flattened form to the OS clipboard
    /// via `Notification::SetClipboard`. Session-wide rather than per-document - a
    /// yank is not tied to a buffer, so it survives file opens and (once several
    /// documents are live) copying in one and pasting in another. Empty until the
    /// first copy/cut.
    register: Vec<String>,
    /// The attached language server, or `None` when none is (the common case).
    /// Everything LSP-related is an `Option` rather than a separate code path so a
    /// session with no server pays nothing and behaves identically.
    lsp: Option<LspConnection>,
    /// core -> file watcher, or `None` when none is attached (SPEC §10.2). Carries
    /// which files the session wants watched; the events come back on the channel
    /// held by the actor loop, the same split the LSP connection uses.
    watch: Option<Sender<WatchRequest>>,
    /// editor -> syntax highlighter, or `None` when none is attached (M4). The
    /// twin of [`Self::lsp_sync`] and, like it, an `Option` so a session with no
    /// highlighter pays nothing. The highlighter has no `didOpen`/`didChange`
    /// distinction - every message is the full text - so it needs no `opened`
    /// flag, only the per-document dirty flag.
    syntax_sync: Option<Sender<SyntaxSync>>,
    /// The id the next document will get. Monotonic and never reused, so a stale
    /// `BufferId` held by a frontend (a picker entry for a buffer closed meanwhile)
    /// resolves to nothing rather than silently to a different buffer.
    next_id: u64,
    /// The buffer list carried on every snapshot, cached (SPEC §5). Rebuilt only
    /// when it would actually differ - see [`Self::buffer_list`] - because building
    /// it clones a `PathBuf` per open buffer, which must not happen per keystroke.
    buffers: Arc<[BufferInfo]>,
    /// Set when the *set* of documents changed (open, close, switch). Changes to a
    /// single document's own entry are caught by comparison instead, so nothing has
    /// to remember to raise this on every edit.
    buffers_stale: bool,
}

impl Session {
    fn new() -> Self {
        Self {
            docs: vec![Document::new(BufferId(0))],
            active: 0,
            register: Vec::new(),
            lsp: None,
            watch: None,
            syntax_sync: None,
            next_id: 1,
            buffers: Arc::from(Vec::new()),
            buffers_stale: true,
        }
    }

    /// The document actions apply to. Infallible: `docs` is never empty and
    /// `active` is only ever set to a valid index.
    fn active(&self) -> &Document {
        &self.docs[self.active]
    }

    fn active_mut(&mut self) -> &mut Document {
        &mut self.docs[self.active]
    }

    /// Allocate the next buffer id.
    fn next_buffer_id(&mut self) -> BufferId {
        let id = BufferId(self.next_id);
        self.next_id += 1;
        id
    }

    /// The index of the document with `id`, or `None` if it is not open (a stale id
    /// from a frontend that has not seen a close yet).
    fn index_of(&self, id: BufferId) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    /// The index of the document bound to `path`, comparing files rather than
    /// spellings (see [`file_identity`]).
    ///
    /// Resolves each open document's path per call rather than caching one on the
    /// document: this runs on `Open`/`SaveAs` - discrete user actions over a handful
    /// of buffers - never on the keystroke path, and a cached identity would go stale
    /// the moment a file is moved or a symlink is repointed underneath us.
    fn index_of_path(&self, path: &Path) -> Option<usize> {
        let identity = file_identity(path);
        self.docs.iter().position(|d| {
            d.path
                .as_deref()
                .is_some_and(|p| file_identity(p) == identity)
        })
    }

    /// The cached buffer list, rebuilt only when it would differ.
    ///
    /// The cheap check is sound because **only the active document can change
    /// between two publishes**: nothing edits, saves, renames or reorders a
    /// background buffer, so comparing the active document against its cached entry
    /// catches every change the `buffers_stale` flag does not (that flag covers
    /// membership and order). One comparison per publish, versus a `PathBuf` clone
    /// per open buffer per keystroke.
    fn buffer_list(&mut self) -> Arc<[BufferInfo]> {
        if !self.buffers_stale {
            let doc = &self.docs[self.active];
            let cached = &self.buffers[self.active];
            if cached.id == doc.id && cached.modified == doc.modified() && cached.path == doc.path {
                return Arc::clone(&self.buffers);
            }
        }
        self.buffers = self
            .docs
            .iter()
            .map(|d| BufferInfo {
                id: d.id,
                path: d.path.clone(),
                modified: d.modified(),
            })
            .collect();
        self.buffers_stale = false;
        Arc::clone(&self.buffers)
    }

    /// Ask the watcher to start or stop watching `path` (SPEC §10.2).
    ///
    /// `try_send` and drop on failure, never await: the actor must not block on a
    /// producer's channel (the deadlock [`Document::lsp_dirty`] exists to avoid).
    /// A dropped request costs one file's change detection, not correctness -
    /// nothing else depends on the watcher being up to date, and the request
    /// channel is sized so a full one means the watcher has stopped anyway.
    fn watch_request(&self, request: WatchRequest) {
        if let Some(watch) = &self.watch {
            let _ = watch.try_send(request);
        }
    }

    /// Announce every open file to a newly attached watcher. A fresh watcher knows
    /// nothing, and the documents it needs to hear about were opened before it
    /// arrived - the same re-announce a new language server gets.
    fn announce_watched(&self) {
        for doc in &self.docs {
            if let Some(path) = &doc.path {
                self.watch_request(WatchRequest::Watch(path.clone()));
            }
        }
    }

    /// A snapshot of the active document, carrying the session's buffer list.
    fn snapshot(&mut self, dirty: Option<std::ops::Range<usize>>) -> ViewSnapshot {
        let buffers = self.buffer_list();
        self.active().snapshot(dirty, buffers)
    }
}

impl Document {
    fn new(id: BufferId) -> Self {
        Self {
            id,
            buffer: RopeBuffer::new(),
            selections: SelectionSet::at_origin(),
            version: 0,
            path: None,
            format: FileFormat::default(),
            read_only: None,
            disk: None,
            history: History::new(),
            decorations: Arc::new(DecorationSet::new()),
            lsp_dirty: false,
            syntax_dirty: false,
        }
    }

    /// Build a snapshot of current state (SPEC §5). The `text` field is an
    /// `Arc`-clone of the rope handle (O(1), the load-bearing part). Selections
    /// are copied into a fresh `Arc<[Selection]>` here - O(selections), which is
    /// trivial for M1's single selection. When M3 adds many cursors, hold the
    /// selection set as an `Arc<[Selection]>` internally so this becomes an `Arc`
    /// bump too, matching the SPEC §5 O(1)-snapshot goal for every field.
    fn snapshot(
        &self,
        dirty: Option<std::ops::Range<usize>>,
        buffers: Arc<[BufferInfo]>,
    ) -> ViewSnapshot {
        ViewSnapshot {
            buffer_id: self.id,
            version: self.version,
            text: self.buffer.text(),
            selections: self.selections.shared(),
            primary: self.selections.primary_index(),
            dirty,
            decorations: Arc::clone(&self.decorations),
            path: self.path.clone(),
            modified: self.modified(),
            format: self.format,
            read_only: self.read_only.is_some(),
            buffers,
        }
    }

    /// The bytes to write for this buffer: the rope's UTF-8/LF text converted back
    /// into the file's own encoding and line terminator (SPEC §10.1).
    ///
    /// Fails when the text holds a character the file's encoding cannot represent;
    /// the caller surfaces that as a `FileError` and leaves the buffer dirty, so the
    /// work is still there to save elsewhere (SPEC §8).
    fn encode_for_save(&self) -> Result<Vec<u8>, String> {
        self.format
            .encode(&self.buffer.text().to_string(), ENSURE_FINAL_NEWLINE)
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

    /// Move every decoration across the applied `changes` (SPEC §5). Skips the
    /// `Arc` clone entirely when nothing is decorated - the overwhelmingly common
    /// case of a buffer with no LSP attached, which must not pay for this at all.
    fn transform_decorations(&mut self, edits: &[Edit]) {
        if self.decorations.is_empty() || edits.is_empty() {
            return;
        }
        // `make_mut` clones only while a published snapshot still shares the set;
        // once the frontend drops that snapshot this mutates in place.
        Arc::make_mut(&mut self.decorations).transform_through(edits);
    }

    /// Replace the syntax highlights with `spans` (M4). Mirrors
    /// [`Self::apply_diagnostics`]: a full replacement of the [`Syntax`] bucket,
    /// leaving other producers' buckets (the LSP's diagnostics) untouched.
    ///
    /// The spans are byte offsets the highlighter resolved against the version it
    /// parsed; they land unchanged and ongoing edits transform them via the
    /// decoration channel until the next reparse (SPEC §5: overlays trail text by
    /// a frame). Returns whether anything changed, so the caller republishes only
    /// when the screen would differ - a reparse that yields the identical set
    /// (common when an edit did not alter tokens) costs no frame.
    ///
    /// [`Syntax`]: crate::decoration::DecorationSource::Syntax
    fn apply_highlights(&mut self, spans: Vec<HighlightSpan>) -> bool {
        use crate::decoration::{Decoration, DecorationSet, DecorationSource};
        let candidate = DecorationSet::sorted_bucket(
            spans
                .into_iter()
                .map(|s| Decoration::Highlight {
                    range: s.range,
                    kind: s.kind,
                })
                .collect(),
        );
        // Compare only the syntax bucket, without cloning the set - a reparse that
        // yields the identical spans (an edit that left tokens intact) allocates
        // nothing. Only a real change takes the copy-on-write mutable borrow, which
        // clones the set once, and only if a published snapshot still shares it.
        if !self
            .decorations
            .bucket_differs(DecorationSource::Syntax, &candidate)
        {
            return false;
        }
        Arc::make_mut(&mut self.decorations).set_bucket(DecorationSource::Syntax, candidate);
        true
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
        use crate::decoration::{DecorationSet, DecorationSource};
        let candidate = DecorationSet::sorted_bucket(convert::decorations_for(
            &self.buffer.text(),
            diagnostics,
        ));
        // Compare only the LSP bucket, no whole-set clone (see `apply_highlights`).
        if !self
            .decorations
            .bucket_differs(DecorationSource::Lsp, &candidate)
        {
            return false;
        }
        Arc::make_mut(&mut self.decorations).set_bucket(DecorationSource::Lsp, candidate);
        true
    }
}

impl Session {
    /// Copy every non-empty selection's text into the register (SPEC §11), one
    /// entry per selection in selection order (the set is sorted, so this is the
    /// on-screen top-to-bottom order). Returns `true` if anything was copied - a set
    /// of bare cursors selects nothing, leaves the register untouched, and returns
    /// `false` so the caller emits no clipboard notification. Reads text via
    /// [`Text::slice`], which is bounded to the selected bytes, never the whole file.
    fn fill_register(&mut self) -> bool {
        let doc = self.active();
        let text = doc.buffer.text();
        let slices: Vec<String> = doc
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

    /// The register flattened for the OS clipboard: entries joined with `\n` (SPEC
    /// §11). The OS clipboard is a single string, so the per-selection structure is
    /// collapsed here while the structured register stays in the core for paste.
    fn register_flattened(&self) -> String {
        self.register.join("\n")
    }

    /// Plan the per-cursor edits a `Paste` produces over the active document: each
    /// selection's span is replaced by the register text assigned to it (SPEC §11
    /// distribution rule). With one register entry it goes to every cursor; with
    /// exactly as many entries as selections the i-th entry goes to the i-th
    /// selection (the multi-cursor round-trip); otherwise every cursor gets the whole
    /// register joined with `\n`. Returns edits sorted DESCENDING by start (as
    /// [`Document::plan_edit`]) so back-to-front application is offset-stable, or
    /// empty for an empty register.
    fn plan_paste(&self) -> Vec<(std::ops::Range<usize>, String)> {
        if self.register.is_empty() {
            return Vec::new();
        }
        let selections = self.active().selections.all();
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

    /// Tell the language server about the active document's current contents, if a
    /// server is attached and anything is outstanding (SPEC §5 full-text sync).
    ///
    /// Never awaits: a full sync channel leaves `lsp_dirty` set and the next call
    /// re-sends the newest text, which is why dropping the attempt is safe.
    fn sync_lsp(&mut self) {
        let Some(lsp) = &mut self.lsp else {
            return;
        };
        let doc = &mut self.docs[self.active];
        let Some(path) = &doc.path else {
            return;
        };
        // A document this connection has never opened is outstanding even when its
        // text has not changed since: the *server* has not seen it. That is what
        // makes a replacement connection announce every buffer without anyone
        // walking the list to re-flag them.
        let announced = lsp.opened.contains(&doc.id);
        if announced && !doc.lsp_dirty {
            return;
        }
        let message = if announced {
            DocumentSync::Changed {
                path: path.clone(),
                version: doc.version,
                text: doc.buffer.text(),
            }
        } else {
            DocumentSync::Opened {
                path: path.clone(),
                language_id: language_id(path),
                text: doc.buffer.text(),
                version: doc.version,
            }
        };
        if lsp.sync.try_send(message).is_ok() {
            doc.lsp_dirty = false;
            lsp.opened.insert(doc.id);
        }
    }

    /// Send the active document to the highlighter if it has changed since the last
    /// send (M4). The twin of [`Self::sync_lsp`], minus the `didOpen`/`didChange`
    /// split the highlighter does not have: every message is the full text.
    ///
    /// Never awaits: a full sync channel leaves `syntax_dirty` set and the next
    /// call re-sends the newest text (the highlighter also coalesces to the newest
    /// on its side), so a dropped attempt is safe and the actor cannot deadlock
    /// against the highlighter awaiting the event channel.
    fn sync_syntax(&mut self) {
        let Some(sync) = &self.syntax_sync else {
            return;
        };
        let doc = &mut self.docs[self.active];
        if !doc.syntax_dirty {
            return;
        }
        let message = SyntaxSync {
            buffer_id: doc.id,
            version: doc.version,
            text: doc.buffer.text(),
        };
        if sync.try_send(message).is_ok() {
            doc.syntax_dirty = false;
        }
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
    SaveAs(PathBuf),
    Switch(BufferId),
    Close {
        id: BufferId,
        force: bool,
    },
    Reload {
        id: BufferId,
        force: bool,
    },
}

impl Step {
    /// Whether this step would change the active buffer's text, and so must be
    /// refused on a read-only buffer (SPEC §10.3).
    ///
    /// Matched exhaustively rather than with a `_` arm on purpose: a new step that
    /// edits the buffer has to state which side it is on here, instead of silently
    /// defaulting to "allowed" and becoming the one way to write to a read-only
    /// file. Saving is not listed - it changes the *file*, not the buffer, and
    /// [`save_file`] holds that guard, where the refusal is a `FileError`.
    fn edits_buffer(&self) -> bool {
        match self {
            Step::Edit(_) | Step::Undo | Step::Redo => true,
            // A reload replaces the buffer wholesale, but it is not blocked here:
            // it is how a read-only buffer catches up with its file, and it can
            // only ever move the buffer *towards* what is on disk.
            Step::Reload { .. }
            | Step::Republish
            | Step::Open(_)
            | Step::Save
            | Step::SaveAs(_)
            | Step::Switch(_)
            | Step::Close { .. } => false,
        }
    }
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
    /// A watched file changed on disk (SPEC §10.2).
    Watch(FileEvent),
    /// A file watcher to attach. Replaces any current one, which drops its
    /// channels and stops it.
    WatchAttach(WatchHandle),
    /// The watcher's *event* channel closed - it or its task is gone.
    WatchClosed,
    /// A highlight batch from the syntax producer (M4).
    Syntax(SyntaxEvent),
    /// A syntax highlighter to attach (the frontend loaded a grammar and spawned
    /// its loop). Replaces any current highlighter and re-announces the buffer.
    SyntaxAttach(SyntaxHandle),
    /// The syntax *event* channel closed - the highlighter or its task is gone.
    SyntaxClosed,
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
#[allow(clippy::too_many_arguments)]
async fn next_incoming(
    actions: &Receiver<Action>,
    lsp: Option<&Receiver<LspEvent>>,
    lsp_attach: &Receiver<LspHandle>,
    syntax: Option<&Receiver<SyntaxEvent>>,
    syntax_attach: &Receiver<SyntaxHandle>,
    watch: Option<&Receiver<FileEvent>>,
    watch_attach: &Receiver<WatchHandle>,
    syntax_first: bool,
) -> Incoming {
    // Race the frontend action against the two producer sides (LSP and syntax),
    // each of which is itself an attach plus events-once-connected. Nested
    // two-way `future::select`s rather than the `select!` macro: the macro's
    // fused-future handling misbehaves for this loop, while `future::select` is
    // the same cancel-safe primitive used elsewhere - the loser is dropped and its
    // `recv` re-created next call, losing no message.
    //
    // **Liveness is the ACTIONS channel alone.** A closed attach channel means the
    // frontend will simply never attach that producer (a valid mode - the one
    // every no-LSP, no-highlighter core is in), so it must never stop the loop:
    // `recv_attach` pends forever once closed instead of resolving, leaving
    // `actions` closing as the one shutdown signal. Without this, a frontend that
    // holds `actions` but drops an attach sender (Rust 2021 disjoint capture never
    // moving an unused sender in) would kill the editor the moment it next idles.
    let action = std::pin::pin!(actions.recv());
    // The two producer sides are boxed to a common type so their poll order can be
    // swapped: `future::select` is biased to its first argument, so a fixed order
    // would let a *burst* on one producer starve the other. A rust-analyzer indexing
    // its workspace floods the LSP side for a second or two; with a fixed order the
    // one pending syntax-highlight batch would lose that race every call and paint
    // visibly late. `syntax_first` alternates each loop turn, so each producer is
    // polled first every other call and neither can be starved for more than a turn.
    type Side<'a> = std::pin::Pin<Box<dyn Future<Output = Incoming> + Send + 'a>>;
    let lsp_side: Side = Box::pin(async {
        match lsp {
            Some(events) => {
                let event = std::pin::pin!(events.recv());
                let attach = std::pin::pin!(recv_attach(lsp_attach));
                match futures::future::select(event, attach).await {
                    Either::Left((e, _)) => e.map_or(Incoming::LspClosed, Incoming::Lsp),
                    Either::Right((h, _)) => Incoming::Attach(h),
                }
            }
            None => Incoming::Attach(recv_attach(lsp_attach).await),
        }
    });
    let syntax_side: Side = Box::pin(async {
        match syntax {
            Some(events) => {
                let event = std::pin::pin!(events.recv());
                let attach = std::pin::pin!(recv_attach(syntax_attach));
                match futures::future::select(event, attach).await {
                    Either::Left((e, _)) => e.map_or(Incoming::SyntaxClosed, Incoming::Syntax),
                    Either::Right((h, _)) => Incoming::SyntaxAttach(h),
                }
            }
            None => Incoming::SyntaxAttach(recv_attach(syntax_attach).await),
        }
    });
    // The watcher is outside the fairness rotation, polled after both. It is the
    // one producer that cannot burst in a way that matters - a file changes on
    // disk at human speed, not at indexing speed - and a reload that lands a
    // moment late is not a defect, where a highlight batch that does is visible.
    // It is only ever *delayed* by a busy producer, never dropped: whichever
    // future loses is re-created next call with its message still queued.
    let watch_side: Side = Box::pin(async {
        match watch {
            Some(events) => {
                let event = std::pin::pin!(events.recv());
                let attach = std::pin::pin!(recv_attach(watch_attach));
                match futures::future::select(event, attach).await {
                    Either::Left((e, _)) => e.map_or(Incoming::WatchClosed, Incoming::Watch),
                    Either::Right((h, _)) => Incoming::WatchAttach(h),
                }
            }
            None => Incoming::WatchAttach(recv_attach(watch_attach).await),
        }
    });
    // Poll the two producers in the turn's order; the action channel still wins any
    // tie (input stays responsive), and both inner arms yield an `Incoming` so the
    // order only affects *fairness*, never the result.
    let (first, second) = if syntax_first {
        (syntax_side, lsp_side)
    } else {
        (lsp_side, syntax_side)
    };
    let producers = futures::future::select(futures::future::select(first, second), watch_side);
    match futures::future::select(action, producers).await {
        Either::Left((a, _)) => a.map_or(Incoming::Stopped, Incoming::Action),
        Either::Right((Either::Left((Either::Left((incoming, _)), _)), _)) => incoming,
        Either::Right((Either::Left((Either::Right((incoming, _)), _)), _)) => incoming,
        Either::Right((Either::Right((incoming, _)), _)) => incoming,
    }
}

/// Await the next producer handle to attach, or pend forever if the attach channel
/// has closed (see [`next_incoming`]: a closed attach channel is not a shutdown, so
/// it must never resolve the select and stop the loop). Generic over the handle
/// type so the LSP and syntax attach paths share one definition.
async fn recv_attach<H>(attach: &Receiver<H>) -> H {
    match attach.recv().await {
        Ok(handle) => handle,
        Err(_) => std::future::pending().await,
    }
}

/// The form a path is compared in to decide whether two buffers are the same *file*
/// rather than the same spelling of one (SPEC §12.2).
///
/// Canonicalized when the file exists, so `notes.md`, `./notes.md` and
/// `/abs/notes.md` all resolve to one identity - and a symlink resolves to its
/// target. This matters because the two ways a path reaches the core disagree by
/// construction: argv passes what the user typed, while the file picker always sends
/// an absolute path. Without this, launching on a relative path and then opening the
/// same file from the picker yields two buffers over one file, each with its own
/// history, and whichever is saved last silently discards the other's edits.
///
/// A path that does not exist yet has nothing to resolve against, so it falls back to
/// itself: two different spellings of the same *not-yet-created* file can still
/// duplicate. That is the residue of doing this without touching the filesystem for
/// files that are not on it, and it self-corrects at the first save.
fn file_identity(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
/// Syntax-attach channel bound: a highlighter is attached rarely (once per file
/// type, on demand), the same profile as the LSP attach.
const SYNTAX_ATTACH_CAP: usize = 4;
/// Watch-attach channel bound: one watcher per session in practice, so the same
/// small bound as the other two attach channels.
const WATCH_ATTACH_CAP: usize = 4;

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
    let (syntax_tx, syntax_rx) = async_channel::bounded::<SyntaxHandle>(SYNTAX_ATTACH_CAP);
    let (watch_tx, watch_rx) = async_channel::bounded::<WatchHandle>(WATCH_ATTACH_CAP);

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
            watch: watch_tx,
            syntax: syntax_tx,
        },
        run: Box::pin(run(
            action_rx,
            delta_tx,
            SnapshotSink { tx: snapshot_tx },
            note_tx,
            lsp_rx,
            syntax_rx,
            watch_rx,
        )),
    }
}

/// Mirror the register to the OS clipboard: fill it from the selections and, if
/// anything was copied, emit `SetClipboard`. Shared by Copy and Cut, which differ
/// only in their follow-up step. Lives in the actor (not on `Session`) so the
/// notifications channel stays a transport concern, not core state.
fn mirror_register(session: &mut Session, notifications: &Sender<Notification>) {
    if session.fill_register() {
        let _ = notifications.try_send(Notification::SetClipboard {
            text: session.register_flattened(),
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
    syntax_attach: Receiver<SyntaxHandle>,
    watch_attach: Receiver<WatchHandle>,
) {
    let mut session = Session::new();
    // The event side of the attached server, or `None` until one attaches. The
    // send side lives on `session.lsp`, so both are swapped together on attach.
    let mut lsp_events: Option<Receiver<LspEvent>> = None;
    // The syntax producer's event side, swapped in the same way (M4).
    let mut syntax_events: Option<Receiver<SyntaxEvent>> = None;
    // The file watcher's event side; the request side lives on `session.watch`.
    let mut watch_events: Option<Receiver<FileEvent>> = None;
    // Alternates the LSP/syntax poll order each turn so neither producer starves the
    // other during a burst (see `next_incoming`).
    let mut syntax_first = false;

    loop {
        syntax_first = !syntax_first;
        // Flush any outstanding document sync before parking on input, so the
        // server and highlighter see the newest text while the user is idle rather
        // than only once they press another key.
        session.sync_lsp();
        session.sync_syntax();

        // Bound the borrow of `*_events` to this statement so the arms below can
        // clear them.
        let incoming = next_incoming(
            &actions,
            lsp_events.as_ref(),
            &lsp_attach,
            syntax_events.as_ref(),
            &syntax_attach,
            watch_events.as_ref(),
            &watch_attach,
            syntax_first,
        )
        .await;
        let action = match incoming {
            Incoming::Stopped => break,
            // A (new) server attached: swap in both channel ends and re-announce the
            // open buffers to it (a fresh `didOpen`), so a file already open when the
            // server arrives is analyzed too. Replacing an earlier server drops its
            // channels, which stops its client loop (SPEC §8).
            //
            // A fresh connection has been told about nothing, which its empty
            // `opened` set says on its own - no document has to be walked and
            // re-flagged, and none can be missed.
            Incoming::Attach(handle) => {
                session.lsp = Some(LspConnection::new(handle.sync));
                lsp_events = Some(handle.events);
                continue;
            }
            // The server died, or its task ended. That must never take the editor
            // with it (SPEC §8): fall back to the no-LSP path, which is also what
            // keeps `select` from spinning on a permanently-ready closed channel.
            // Dropping the connection drops what it had been told with it, so
            // whatever attaches next starts from nothing.
            Incoming::LspClosed => {
                lsp_events = None;
                session.lsp = None;
                continue;
            }
            Incoming::Lsp(LspEvent::Diagnostics { path, diagnostics }) => {
                // Route by path across every open document, not just the active one:
                // a server analyzes the whole workspace and publishes for any file in
                // it, so a batch routinely belongs to a buffer sitting in the
                // background. Delivering it there means switching to that buffer shows
                // its diagnostics immediately instead of waiting for a republish.
                // `apply_diagnostics` self-guards on the path, so it is the routing
                // predicate as well as the application.
                let active = session.active;
                let mut repaint = false;
                for (index, doc) in session.docs.iter_mut().enumerate() {
                    if doc.apply_diagnostics(&path, &diagnostics) && index == active {
                        repaint = true;
                    }
                }
                // Republish only when the screen would actually differ: a server
                // re-sending an identical batch (common while indexing), or one for an
                // off-screen buffer, must not cost a frame.
                if repaint && !snapshots.publish(session.snapshot(None)) {
                    break;
                }
                continue;
            }
            // A watcher attached: swap in both ends and tell it about every file
            // already open, since it starts knowing nothing and those files were
            // opened before it existed (SPEC §10.2).
            Incoming::WatchAttach(handle) => {
                session.watch = Some(handle.requests);
                watch_events = Some(handle.events);
                session.announce_watched();
                continue;
            }
            // The watcher died or its task ended. As with the other producers that
            // must never take the editor with it (SPEC §8): drop both ends, which
            // also stops `select` spinning on a permanently-ready closed channel.
            // External changes simply go unnoticed from here, which is the state
            // every session was in before a watcher was attachable at all.
            Incoming::WatchClosed => {
                watch_events = None;
                session.watch = None;
                continue;
            }
            Incoming::Watch(FileEvent::Changed(path)) => {
                if !external_change(&mut session, &path, &deltas, &snapshots, &notifications).await
                {
                    break;
                }
                continue;
            }
            // A highlighter attached: swap in both channel ends and mark the buffer
            // dirty so the next loop turn sends it for a first parse (M4). Replacing
            // an earlier highlighter drops its channels, stopping its loop (SPEC §8).
            Incoming::SyntaxAttach(handle) => {
                session.syntax_sync = Some(handle.sync);
                syntax_events = Some(handle.events);
                session.active_mut().syntax_dirty = true;
                continue;
            }
            // The highlighter died or its task ended. As with LSP, that must never
            // take the editor with it (SPEC §8): drop both ends and fall back to the
            // no-highlighter path, which also stops `select` spinning on a
            // permanently-ready closed channel.
            Incoming::SyntaxClosed => {
                syntax_events = None;
                session.syntax_sync = None;
                continue;
            }
            Incoming::Syntax(SyntaxEvent::Highlights {
                buffer_id,
                version,
                spans,
            }) => {
                // Apply only a batch computed against the buffer and version it
                // actually parsed. The spans are byte offsets in that text; if the
                // buffer has advanced since (an edit landed while the parse was in
                // flight), installing them verbatim would place them at stale
                // offsets - misplaced, not merely a frame behind. A stale batch is
                // dropped: the resident highlights keep their positions as edits
                // transform them (`transform_decorations`), and `sync_syntax` keeps
                // re-sending the newest text, so a fresh matching batch lands the
                // moment typing pauses. This is the "overlays trail text, never
                // misplace it" contract (SPEC §5).
                //
                // **Both halves of the identity are required.** Versions are
                // per-buffer, so two open documents sit at the same version all the
                // time; matching on version alone would paint one buffer's tokens onto
                // another's text the moment the user switches mid-parse. The batch is
                // applied to whichever document it belongs to, background or not - its
                // highlights are correct for that buffer, and it repaints when focused.
                // Parsed for a buffer that has since been closed: nothing to apply.
                let Some(index) = session.index_of(buffer_id) else {
                    continue;
                };
                let active = session.active;
                let doc = &mut session.docs[index];
                // Republish only when the set actually changed *and* it is on screen,
                // so an identical reparse (an edit that left tokens intact) or one for
                // a background buffer costs no frame.
                if version == doc.version
                    && doc.apply_highlights(spans)
                    && index == active
                    && !snapshots.publish(session.snapshot(None))
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
            Action::Insert(text) => Step::Edit(session.active().plan_edit(EditKind::Insert(text))),
            Action::DeleteBackward => {
                Step::Edit(session.active().plan_edit(EditKind::DeleteBackward))
            }
            Action::DeleteForward => {
                Step::Edit(session.active().plan_edit(EditKind::DeleteForward))
            }
            // Copy fills the register but touches no text: emit the clipboard mirror
            // (if anything was selected) and republish, no delta or version bump.
            Action::Copy => {
                mirror_register(&mut session, &notifications);
                Step::Republish
            }
            // Cut = copy + delete the selections, as one edit / one undo unit. Fill
            // the register and emit the mirror first, then plan the deletion; a set
            // of bare cursors selects nothing, so `plan_edit` returns no edits and
            // the apply path treats it as a no-op.
            Action::Cut => {
                mirror_register(&mut session, &notifications);
                Step::Edit(session.active().plan_edit(EditKind::DeleteSelection))
            }
            // Paste distributes the register over the cursors (SPEC §11); an empty
            // register plans no edits and is a clean no-op. A paste is a distinct
            // action, not a keystroke, so it ends any typing-coalescing run - the one
            // break `History` cannot infer, since a paste leaves the carets exactly
            // where typing would and a single-character payload is indistinguishable
            // from a keystroke at the `Change` level (SPEC §2.4).
            Action::Paste => {
                session.active_mut().history.break_coalescing();
                Step::Edit(session.plan_paste())
            }
            // The selection-changing actions need no coalescing bookkeeping: every
            // edit carries the selection set it started from, so `History` sees the
            // caret moved and ends the typing run itself (SPEC §2.4 break rule (d)).
            // A new selection action added here inherits that for free.
            Action::MoveCursor { motion, extend } => {
                session.active_mut().move_cursor(motion, extend);
                Step::Republish
            }
            Action::PlaceCursor { offset, extend } => {
                session.active_mut().place_cursor(offset, extend);
                Step::Republish
            }
            Action::AddCursorAbove => {
                session.active_mut().add_cursor_vertical(true);
                Step::Republish
            }
            Action::AddCursorBelow => {
                session.active_mut().add_cursor_vertical(false);
                Step::Republish
            }
            Action::AddCursorAt { offset } => {
                session.active_mut().add_cursor_at(offset);
                Step::Republish
            }
            Action::CollapseSelections => {
                session.active_mut().collapse_selections();
                Step::Republish
            }
            Action::Undo => Step::Undo,
            Action::Redo => Step::Redo,
            Action::RequestSnapshot => Step::Republish,
            Action::Open(path) => Step::Open(path),
            Action::Save => Step::Save,
            Action::SaveAs(path) => Step::SaveAs(path),
            Action::SwitchBuffer { id } => Step::Switch(id),
            Action::CloseBuffer { id, force } => Step::Close { id, force },
            Action::Reload { id, force } => Step::Reload { id, force },
            Action::Quit => break,
        };

        // One choke point for read-only (SPEC §10.3): every text change funnels
        // through a `Step`, so refusing here cannot be bypassed by an action the
        // guard forgot about. `Copy`/`Cut` have already filled the register above -
        // deliberately, since yanking out of a file you cannot write is not a
        // mutation, and only `Cut`'s deletion half is refused.
        if let Some(reason) = session.active().read_only
            && step.edits_buffer()
        {
            let doc = session.active();
            let _ = notifications.try_send(Notification::EditRejected {
                buffer_id: doc.id,
                version: doc.version,
                message: reason.message().to_string(),
            });
            if !snapshots.publish(session.snapshot(None)) {
                break;
            }
            continue;
        }

        let alive = match step {
            Step::Edit(edits) => {
                apply_edit(&mut session, edits, &deltas, &snapshots, &notifications).await
            }
            Step::Undo => {
                let reverted = session.active_mut().history.undo();
                reapply(&mut session, reverted, &deltas, &snapshots, &notifications).await
            }
            Step::Redo => {
                let reverted = session.active_mut().history.redo();
                reapply(&mut session, reverted, &deltas, &snapshots, &notifications).await
            }
            Step::Republish => snapshots.publish(session.snapshot(None)),
            Step::Open(path) => {
                open_file(&mut session, path, &deltas, &snapshots, &notifications).await
            }
            Step::Save => save_file(&mut session, &snapshots, &notifications).await,
            Step::SaveAs(path) => {
                save_as_file(&mut session, path, &snapshots, &notifications).await
            }
            // An unknown id is a no-op republish: a frontend may hold a stale id from
            // a picker entry whose buffer was closed since.
            Step::Switch(id) => match session.index_of(id) {
                Some(index) => switch_to(&mut session, index, &snapshots, &notifications),
                None => snapshots.publish(session.snapshot(None)),
            },
            Step::Close { id, force } => {
                close_buffer(&mut session, id, force, &snapshots, &notifications)
            }
            Step::Reload { id, force } => {
                reload_buffer(&mut session, id, force, &deltas, &snapshots, &notifications).await
            }
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
    session: &mut Session,
    edits: Vec<(std::ops::Range<usize>, String)>,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    if edits.is_empty() {
        // No-op (e.g. backspace at buffer start): republish so the frontend's
        // view stays current, but do not bump the version or emit a delta.
        return snapshots.publish(session.snapshot(None));
    }

    // Snapshot the selections *before* the edit so undo can restore them.
    let before = session.active().selections.clone();
    let Some((changes, dirty)) = apply_change_list(session, &edits, deltas, notifications).await
    else {
        return false; // frontend gone mid-stream
    };

    // If every planned edit was rejected (or was a true no-op), nothing changed:
    // do not bump the version or record history (a version bump with no delta
    // would diverge a remote frontend replaying the delta stream, SPEC §5).
    if changes.is_empty() {
        return snapshots.publish(session.snapshot(None));
    }

    // Build the anchor-transform edit list once and share it: both the selection
    // remap and the decoration remap need the identical batch, so computing it twice
    // (a sort + allocation each) was pure waste on the keystroke path (#5).
    let edits = edits_from_changes(&changes);
    let doc = session.active_mut();
    // Remap selections by transforming each pre-edit caret through the applied
    // edits (SPEC §2.1 anchors): a cursor lands after its own inserted text / at its
    // deletion point, and every other cursor shifts by the edits around it.
    doc.selections = selections_after_edits(&before, &edits);
    // Decorations ride the same batch, so a squiggle stays under the token it
    // flagged while the producer catches up (SPEC §5).
    doc.transform_decorations(&edits);
    doc.version += 1;
    // One user action is one undo unit, even when it fanned across N cursors
    // (SPEC §2.4). Coalescing (single-caret typing) is decided inside `record`.
    doc.history.record(changes, before, doc.selections.clone());
    // The server's and highlighter's copies are now stale; `sync_lsp` /
    // `sync_syntax` re-send before the next park.
    doc.lsp_dirty = true;
    doc.syntax_dirty = true;
    snapshots.publish(session.snapshot(dirty))
}

/// Apply an undo or redo, sharing the "apply edits + restore selections + publish"
/// tail. `reverted` is the move the history already produced (`History::undo` /
/// `History::redo`): the edits to apply against the current buffer plus the
/// selections to restore, or `None` at a branch end (nothing to undo/redo), a clean
/// no-op. Undo/redo *are* edits on the wire: they emit deltas and bump the version
/// like any change, so a remote frontend replaying the log converges (SPEC §5) - it
/// has no notion of "undo", only more buffer edits moving forward in version time.
async fn reapply(
    session: &mut Session,
    reverted: Option<Reverted>,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let Some(reverted) = reverted else {
        // Nothing to undo/redo: republish so the view stays current, no version bump.
        return snapshots.publish(session.snapshot(None));
    };

    let Some((changes, dirty)) =
        apply_change_list(session, &reverted.edits, deltas, notifications).await
    else {
        return false; // frontend gone
    };
    let doc = session.active_mut();
    // Inverse/forward edits derived from a consistent history over this buffer
    // always apply cleanly, so `changes` is non-empty here; guard the version bump
    // anyway so a would-be no-op never advances the version without a delta.
    if !changes.is_empty() {
        doc.version += 1;
    }
    doc.transform_decorations(&edits_from_changes(&changes));
    doc.lsp_dirty = true;
    doc.syntax_dirty = true;
    doc.selections = reverted.selections;
    snapshots.publish(session.snapshot(dirty))
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
    session: &mut Session,
    edits: &[(std::ops::Range<usize>, String)],
    deltas: &Sender<Delta>,
    notifications: &Sender<Notification>,
) -> Option<(Vec<Change>, Option<std::ops::Range<usize>>)> {
    let doc = session.active_mut();
    // Deltas are expressed against the pre-edit version; no edit here bumps it
    // (the caller does, once, after this returns), so read it once up front.
    let base_version = doc.version;
    let mut changes: Vec<Change> = Vec::with_capacity(edits.len());
    let mut dirty: Option<std::ops::Range<usize>> = None;

    for (range, new_text) in edits {
        // Drop a pure no-op (replace nothing with nothing): it would emit an empty
        // delta and record an empty revision, both meaningless.
        if range.is_empty() && new_text.is_empty() {
            continue;
        }
        let removed = match doc.buffer.replace(range.clone(), new_text) {
            Ok(removed) => removed,
            Err(err) => {
                // Surface and skip this one edit; keep the buffer intact (SPEC §8).
                let _ = notifications.try_send(Notification::EditRejected {
                    buffer_id: doc.id,
                    version: doc.version,
                    message: err.to_string(),
                });
                continue;
            }
        };
        // A Delta is expressed against the base (pre-edit) version. Emitting one
        // per sub-edit keeps the lossless log exact for a remote frontend.
        let delta = Delta {
            buffer_id: doc.id,
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

/// Open `path` (SPEC §12.2 file lifecycle), which with several buffers live means
/// one of three things:
///
/// - **already open** - switch to that buffer rather than loading a second copy of
///   the file. Two documents over one path would each carry their own history and
///   could silently overwrite each other's saves.
/// - **the active buffer is an untouched scratch** (unnamed, empty, unmodified) -
///   load into it, so launching bare and then opening a file does not strand an
///   empty tab.
/// - otherwise - create a new buffer and make it active.
///
/// The load itself is expressed as one whole-buffer `Delta` so the delta stream
/// still reproduces the snapshot (SPEC §5). A missing file is not an error: it binds
/// the path to a fresh empty buffer, created on the first `Save` (Vim's behavior).
/// Any other read failure (permissions, non-UTF-8) is surfaced as a `Notification`
/// and creates no buffer at all - the read happens *before* anything is allocated,
/// so a failed open leaves the session exactly as it was (SPEC §8). Returns `false`
/// if the frontend has hung up.
///
/// File I/O is blocking `std::fs` on the actor thread: acceptable for a discrete
/// user action (not the per-keystroke hot path). Moving large loads off the
/// critical path via a background read (SPEC §2.3) is an M5 refinement.
async fn open_file(
    session: &mut Session,
    path: PathBuf,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    // Already open: focus it. No read, no delta - the buffer on screen is the file.
    if let Some(index) = session.index_of_path(&path) {
        return switch_to(session, index, snapshots, notifications);
    }

    // Stat before reading. A FIFO or a character device answers a read by blocking
    // until someone writes - on the actor thread, that is the whole editor hung with
    // no way out (SPEC §10.3). `metadata` follows symlinks, so a link to a regular
    // file is one; a dangling link reports `NotFound` and takes the missing-file
    // path below, where the first save writes through the link.
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return report_file_error(
                session,
                Some(path),
                &err.to_string(),
                snapshots,
                notifications,
            );
        }
    };
    if let Some(metadata) = &metadata
        && !metadata.is_file()
    {
        let kind = if metadata.is_dir() {
            "is a directory"
        } else {
            "not a regular file"
        };
        return report_file_error(session, Some(path), kind, snapshots, notifications);
    }

    // Read bytes, not a `String`: the file decides its own encoding, and
    // `read_to_string` would reject everything that is not UTF-8 outright
    // (SPEC §10.1). `file::load` decodes it and reports the format to write back.
    let (contents, format, read_only, existed) = match std::fs::read(&path) {
        Ok(bytes) if crate::file::is_binary(&bytes) => {
            // Refused rather than opened as mojibake: every byte would round-trip
            // until the first edit, which would corrupt the file (SPEC §10.3).
            return report_file_error(
                session,
                Some(path),
                "binary file (contains NUL bytes)",
                snapshots,
                notifications,
            );
        }
        Ok(bytes) => {
            let loaded = crate::file::load(&bytes);
            let read_only = read_only_reason(&loaded, &path);
            (loaded.text, loaded.format, read_only, true)
        }
        // Missing file: open an empty buffer bound to the path (created on save).
        // Nothing to be read-only about - there is no file yet, and whether its
        // directory will accept one is the save's answer to give.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            (String::new(), FileFormat::default(), None, false)
        }
        Err(err) => {
            return report_file_error(
                session,
                Some(path),
                &err.to_string(),
                snapshots,
                notifications,
            );
        }
    };

    // The read succeeded, so it is safe to commit to a buffer now. Reuse an
    // untouched scratch; otherwise open a new document beside the existing ones.
    let scratch = {
        let doc = session.active();
        doc.path.is_none() && doc.buffer.byte_len() == 0 && !doc.modified()
    };
    if !scratch {
        let id = session.next_buffer_id();
        session.docs.push(Document::new(id));
        session.active = session.docs.len() - 1;
        session.buffers_stale = true;
    }
    let doc = session.active_mut();

    // Replace the whole buffer as one Delta. Skip the delta/version bump when
    // nothing actually changes (empty buffer, empty file) so `version` still
    // advances iff a delta was emitted - the invariant the property test guards.
    // The load builds a fresh buffer rather than calling the fallible `replace`:
    // a whole-buffer swap has no range to reject, so there is no error path to
    // handle here (the delta still records the change for SPEC §5 replay).
    let old_len = doc.buffer.byte_len();
    let changed = old_len != 0 || !contents.is_empty();
    if changed {
        let base_version = doc.version;
        doc.buffer = RopeBuffer::from(contents.as_str());
        let delta = Delta {
            buffer_id: doc.id,
            base_version,
            range: 0..old_len,
            new_text: contents,
        };
        if deltas.send(delta).await.is_err() {
            return false; // frontend gone
        }
        doc.version += 1;
    }

    // A freshly opened buffer starts at the origin and matches disk. Undo does not
    // cross a load, so the history is reset to a fresh tree rooted at the loaded
    // content, which is the saved state (SPEC §2.4).
    doc.selections = SelectionSet::at_origin();
    doc.path = Some(path.clone());
    let path_for_watch = path.clone();
    // The buffer now *is* this file, so it saves in this file's form - including
    // when a scratch buffer was reused and had been carrying the default.
    doc.format = format;
    doc.read_only = read_only;
    // Stamped from the stat taken *before* the read, deliberately: a write that
    // lands between the two leaves us with an older stamp than the bytes we hold,
    // which costs a redundant reload. The other order would leave a newer stamp
    // than the bytes and miss the change entirely (SPEC §10.2).
    doc.disk = metadata.as_ref().and_then(DiskStamp::of);
    doc.history = History::new();
    // Decorations describe the *previous* file's text; keeping them would paint
    // squiggles at meaningless offsets until a producer refreshes.
    doc.decorations = Arc::new(DecorationSet::new());
    // A different file is a different document to the server: announce it fresh
    // rather than sending a change against the old one's identity. The highlighter
    // has no per-document identity, so a plain dirty flag re-parses the new text.
    doc.lsp_dirty = true;
    doc.syntax_dirty = true;
    let id = doc.id;
    // `dirty` is a "what changed" repaint hint, so it is `None` when no delta was
    // emitted (a missing/empty file); reporting a spurious `Some(0..0)` would tell
    // a frontend an edit happened where none did (view.rs contract).
    let dirty = changed.then(|| 0..doc.buffer.byte_len());

    let _ = notifications.try_send(Notification::FileOpened {
        buffer_id: id,
        path,
        existed,
    });
    // Reusing a scratch buffer keeps its id while changing what it *is*, so the
    // server has to be told about it again under the new name.
    if let Some(lsp) = &mut session.lsp {
        lsp.forget(id);
    }
    // The path changed, so the cached entry for this buffer is stale.
    session.buffers_stale = true;
    session.watch_request(WatchRequest::Watch(path_for_watch));
    snapshots.publish(session.snapshot(dirty))
}

/// Decide what a change to `path` on disk means for the buffer holding it
/// (SPEC §8, §10.2). Returns `false` if the frontend has hung up.
///
/// The rule is the one SPEC §8 states: **never silently overwrite either side.**
/// A clean buffer has no side of its own, so it follows the file. A modified one
/// does, so the two versions are the user's to reconcile and the core only reports
/// the collision.
///
/// Most events that arrive here are the editor's own save coming back around, and
/// the stamp is what tells them apart from a real change. The stamp is also
/// advanced when a conflict is *reported*, so a platform that sends three events
/// for one write raises one prompt rather than three.
async fn external_change(
    session: &mut Session,
    path: &Path,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    // A watcher watches whole directories (a rename-over-the-file save would
    // otherwise slip out from under a per-file watch), so events for paths no
    // buffer holds are routine and not an error.
    let Some(index) = session.index_of_path(path) else {
        return true;
    };

    let stamp = DiskStamp::read(path);
    let doc = &mut session.docs[index];
    if stamp == doc.disk {
        // Our own save echoing back, or an event about something that did not
        // change the file (a metadata touch, a sibling in the same directory).
        return true;
    }
    // Accounted for either way: whether this reloads or asks, the same disk state
    // must not come back a second time.
    doc.disk = stamp;
    let id = doc.id;

    if stamp.is_none() {
        // The file is gone. There is nothing to reload from and the buffer is now
        // the only copy - so hold onto it and say so, rather than helpfully
        // emptying the buffer to match (SPEC §8).
        let _ = notifications.try_send(Notification::ExternalChange {
            buffer_id: id,
            path: path.to_path_buf(),
            removed: true,
        });
        return snapshots.publish(session.snapshot(None));
    }

    if doc.modified() {
        let _ = notifications.try_send(Notification::ExternalChange {
            buffer_id: id,
            path: path.to_path_buf(),
            removed: false,
        });
        return snapshots.publish(session.snapshot(None));
    }
    reload(
        session,
        index,
        path.to_path_buf(),
        deltas,
        snapshots,
        notifications,
    )
    .await
}

/// Handle `Action::Reload` - the user taking the disk side of a conflict
/// (SPEC §10.2). Returns `false` if the frontend has hung up.
///
/// Refuses a modified buffer without `force`, exactly as [`close_buffer`] does: a
/// reload discards unsaved work as completely as a close, so the guard lives here
/// rather than in whichever frontend happens to remember to ask (SPEC §8).
async fn reload_buffer(
    session: &mut Session,
    id: BufferId,
    force: bool,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    // A stale id from a frontend that has not seen a close yet: a no-op, not an
    // error - the same treatment `close_buffer` gives it.
    let Some(index) = session.index_of(id) else {
        return snapshots.publish(session.snapshot(None));
    };
    // Nothing to re-read from. Not an error either: a scratch buffer is a
    // legitimate thing to have aimed a reload at by mistake.
    let Some(path) = session.docs[index].path.clone() else {
        return snapshots.publish(session.snapshot(None));
    };
    if session.docs[index].modified() && !force {
        let _ = notifications.try_send(Notification::ReloadRejected {
            buffer_id: id,
            path,
        });
        return snapshots.publish(session.snapshot(None));
    }
    reload(session, index, path, deltas, snapshots, notifications).await
}

/// Replace document `index`'s contents with its file's, as one whole-buffer
/// `Delta` (SPEC §5). Returns `false` if the frontend has hung up.
///
/// Nearly everything a fresh open does, and for the same reasons - the file's
/// encoding and read-only state are re-detected, because whoever rewrote it may
/// have changed either - with two deliberate differences:
///
/// - **Selections are clamped, not reset.** The change is usually somewhere other
///   than where the user is looking (a `git checkout`, a formatter), and throwing
///   the caret back to the top of a 4000-line file would be its own lost work.
/// - **An identical file is not a change.** Comparing before replacing costs one
///   string compare and buys idempotence, which is what makes the several events
///   a platform emits for one write harmless.
///
/// History is reset, as on any load: the undo tree describes edits to text that is
/// no longer there, and undoing across a reload would reconstruct a file that
/// never existed (SPEC §2.4).
async fn reload(
    session: &mut Session,
    index: usize,
    path: PathBuf,
    deltas: &Sender<Delta>,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let bytes = match std::fs::read(&path) {
        Ok(bytes) if crate::file::is_binary(&bytes) => {
            return report_file_error(
                session,
                Some(path),
                "binary file (contains NUL bytes)",
                snapshots,
                notifications,
            );
        }
        Ok(bytes) => bytes,
        Err(err) => {
            return report_file_error(
                session,
                Some(path),
                &err.to_string(),
                snapshots,
                notifications,
            );
        }
    };

    let loaded = crate::file::load(&bytes);
    let read_only = read_only_reason(&loaded, &path);
    let doc = &mut session.docs[index];
    doc.format = loaded.format;
    doc.read_only = read_only;

    let old_len = doc.buffer.byte_len();
    let changed = doc.buffer.text().to_string() != loaded.text;
    if changed {
        let base_version = doc.version;
        let new_len = loaded.text.len();
        doc.buffer = RopeBuffer::from(loaded.text.as_str());
        let delta = Delta {
            buffer_id: doc.id,
            base_version,
            range: 0..old_len,
            new_text: loaded.text,
        };
        if deltas.send(delta).await.is_err() {
            return false; // frontend gone
        }
        doc.version += 1;
        // Every caret survives, moved only as far as it has to be: a file that grew
        // leaves them exactly where they were.
        doc.selections.clamp_to(new_len);
        // The decorations describe the old text. Producers refresh them from the
        // flags below; until then, none beats misplaced ones (SPEC §5).
        doc.decorations = Arc::new(DecorationSet::new());
        doc.lsp_dirty = true;
        doc.syntax_dirty = true;
    }
    // The buffer now *is* the file, whether or not anything moved: a reload of a
    // buffer that had been edited back to matching disk is still clean afterwards.
    doc.history = History::new();

    let id = doc.id;
    let dirty = changed.then(|| 0..doc.buffer.byte_len());
    let _ = notifications.try_send(Notification::FileReloaded {
        buffer_id: id,
        path,
    });
    // The modified marker in the buffer list changes with the reload, and it is
    // this document's entry that may be stale even if it is not the active one.
    session.buffers_stale = true;
    snapshots.publish(session.snapshot(dirty))
}

/// Why a freshly loaded file refuses edits, or `None` if it does not (SPEC §10.3).
///
/// One definition for both the open and the reload paths: a file that changed on
/// disk gets exactly the judgement it would get if it were being opened now, and
/// the two cannot drift into disagreeing about what read-only means.
///
/// Order matters. A file that did not fully decode is read-only whatever its
/// permissions say, because the problem is that the *buffer* is not the file -
/// checking writability first would report a fixable-looking reason for something
/// no `chmod` will fix.
fn read_only_reason(loaded: &crate::file::Loaded, path: &Path) -> Option<ReadOnly> {
    if loaded.lossy {
        Some(ReadOnly::Undecodable)
    } else if !is_writable(path) {
        Some(ReadOnly::Permissions)
    } else {
        None
    }
}

/// Whether this process can actually write `path` (SPEC §10.3).
///
/// Opening for append is the probe rather than reading the permission bits,
/// because the bits are not the question: a mode-644 file owned by someone else is
/// unwritable to us while `Permissions::readonly()` reports it writable, and the
/// same goes for a read-only mount or a file the OS has locked. Opening with
/// `append` asks the kernel the real question and changes nothing - no truncation,
/// no write, not even an mtime bump - and the file is known to be regular by the
/// time this runs, so there is no device or FIFO here to block on.
///
/// Only a permission-shaped failure means read-only. Any other error (out of file
/// descriptors, a race with a delete) leaves the buffer editable and lets the save
/// report the real problem, which beats locking a file the user can in fact write.
fn is_writable(path: &Path) -> bool {
    match std::fs::OpenOptions::new().append(true).open(path) {
        Ok(_) => true,
        Err(err) => !matches!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
        ),
    }
}

/// Make `index` the active document and tell the frontend (SPEC §6). A switch
/// changes no text, so it emits no delta and bumps no version - but the snapshot
/// that follows describes a different buffer, with its own selections, decorations
/// and history, all of which were preserved while it sat in the background.
///
/// Deliberately does *not* re-sync the producers. The newly active document keeps
/// the decorations it had, and nothing edited it while it was inactive, so they are
/// still correct rather than merely stale. The frontend re-attaching a grammar for a
/// different language raises `syntax_dirty` on its own (the `SyntaxAttach` arm), so
/// forcing a reparse here would only duplicate work on same-language switches.
fn switch_to(
    session: &mut Session,
    index: usize,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    if index != session.active {
        session.active = index;
        session.buffers_stale = true;
    }
    // Announced even when the buffer was already active: the frontend uses this to
    // attach the right server and grammar, and an `Open` of the focused file is a
    // reasonable way to ask for exactly that.
    let doc = session.active();
    let _ = notifications.try_send(Notification::BufferSwitched {
        buffer_id: doc.id,
        path: doc.path.clone(),
    });
    snapshots.publish(session.snapshot(None))
}

/// Close buffer `id` (SPEC §8). A modified buffer is refused unless `force`, with a
/// `CloseRejected` the frontend turns into a confirmation prompt: the *core* holds
/// this guard so no frontend can discard work by forgetting to ask.
///
/// Closing the active buffer moves focus to the one that takes its index (or the new
/// last, if it was the last); closing the final buffer leaves a fresh empty one, so
/// the session always has somewhere to type and `active` always resolves.
fn close_buffer(
    session: &mut Session,
    id: BufferId,
    force: bool,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    // An unknown id is a stale reference from a frontend that has not seen an
    // earlier close yet - a no-op, not an error.
    let Some(index) = session.index_of(id) else {
        return snapshots.publish(session.snapshot(None));
    };
    if session.docs[index].modified() && !force {
        let _ = notifications.try_send(Notification::CloseRejected {
            buffer_id: id,
            path: session.docs[index].path.clone(),
        });
        return snapshots.publish(session.snapshot(None));
    }

    let closed = session.docs.remove(index);
    session.buffers_stale = true;
    // Nothing left to reload into, so stop hearing about it - otherwise a session
    // that opens and closes its way around a tree accumulates watches on files no
    // buffer holds.
    if let Some(path) = closed.path {
        session.watch_request(WatchRequest::Unwatch(path));
    }
    // Ids are never reused, so a leftover entry could not mis-target a later buffer -
    // but it would accumulate for the session's life, so drop it with the document.
    if let Some(lsp) = &mut session.lsp {
        lsp.forget(id);
    }
    let _ = notifications.try_send(Notification::BufferClosed { buffer_id: id });

    if session.docs.is_empty() {
        // Never leave the session with nowhere to type. This lands on the
        // "active buffer is gone" case below, with `active` already 0.
        let fresh = session.next_buffer_id();
        session.docs.push(Document::new(fresh));
    }
    if index < session.active {
        // Focus is unchanged, but every index after the hole shifted down.
        session.active -= 1;
        return snapshots.publish(session.snapshot(None));
    }
    if index > session.active {
        // Closed a buffer after the active one: focus and indices are untouched.
        return snapshots.publish(session.snapshot(None));
    }
    // The active buffer is gone: focus whatever slid into its place. `switch_to`
    // sets `active` itself, so there is nothing to assign here.
    let next = session.active.min(session.docs.len() - 1);
    switch_to(session, next, snapshots, notifications)
}

/// Write the buffer to its bound file atomically (SPEC §8). Fails with a
/// `Notification` if no path is bound (save-as arrives with the prompt UI) or the
/// write fails; on failure the buffer stays dirty so no work is lost. Returns
/// `false` if the frontend has hung up.
async fn save_file(
    session: &mut Session,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let Some(path) = session.active().path.clone() else {
        return report_file_error(
            session,
            None,
            "no file name (save-as not available yet)",
            snapshots,
            notifications,
        );
    };
    // A read-only buffer has nothing to save - it cannot have been edited - and for
    // an undecodable file writing it back would replace the bytes that did not
    // decode with U+FFFD (SPEC §8). `SaveAs` is deliberately still allowed: writing
    // the buffer somewhere else is the escape hatch, and it leaves the original
    // untouched.
    if let Some(reason) = session.active().read_only {
        return report_file_error(
            session,
            Some(path),
            reason.message(),
            snapshots,
            notifications,
        );
    }

    let bytes = match session.active().encode_for_save() {
        Ok(bytes) => bytes,
        Err(message) => {
            return report_file_error(session, Some(path), &message, snapshots, notifications);
        }
    };
    if let Err(message) = write_atomic(&path, &bytes) {
        return report_file_error(session, Some(path), &message, snapshots, notifications);
    }

    let doc = session.active_mut();
    // The file is now exactly what we just wrote. Recording that here is what
    // stops the watcher's report of *this* write from being read as someone
    // else's change (SPEC §10.2).
    doc.disk = DiskStamp::read(&path);
    // Mark the current history node as the on-disk state, so undoing back to it
    // later clears the modified marker (SPEC §2.4, §8).
    doc.history.mark_saved();
    let _ = notifications.try_send(Notification::FileSaved {
        buffer_id: doc.id,
        path,
    });
    snapshots.publish(session.snapshot(None))
}

/// Write the buffer to `path` and adopt it as the buffer's file (the save-as the
/// prompt commits, SPEC §7.5). The write is atomic like [`save_file`]; on failure
/// the buffer keeps its previous path and stays dirty, so a rejected save-as loses
/// neither the work nor the original association (SPEC §8). Returns `false` if the
/// frontend has hung up.
///
/// Adopting a path that differs from the current one changes the document's
/// *identity*, so the language server is re-announced: the connection forgets it and
/// `lsp_dirty` is set, and the next [`Session::sync_lsp`] sends a fresh `didOpen`
/// under the new path (whose extension may map to a different `languageId`). The old
/// document is not formally closed - the same re-announce shape [`open_file`] uses.
/// The highlighter is path-agnostic and the text is unchanged, so no reparse is
/// forced; a save-as that changes the *language* still needs the frontend to attach
/// the new grammar, deferred with per-buffer grammar (M7).
async fn save_as_file(
    session: &mut Session,
    path: PathBuf,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    // Saving *onto* a file another buffer holds would leave two documents claiming
    // one file, each with its own history - the same hazard `open_file` refuses to
    // create, arrived at from the other direction, and the one where the next save
    // discards the other buffer's work. Refused before the write, so nothing is
    // touched on disk (SPEC §8).
    if let Some(index) = session.index_of_path(&path) {
        if index != session.active {
            return report_file_error(
                session,
                Some(path),
                "already open in another buffer",
                snapshots,
                notifications,
            );
        }
        // Aimed at this buffer's own file, a save-as *is* a save, so it inherits
        // the read-only guard `save_file` holds. Without this, "save as" over the
        // same name would be the way around a read-only buffer - and for an
        // undecodable file that means writing U+FFFD over the bytes that did not
        // decode. Compared by file identity, so a different spelling of the same
        // path does not slip past (SPEC §10.3).
        if let Some(reason) = session.active().read_only {
            return report_file_error(
                session,
                Some(path),
                reason.message(),
                snapshots,
                notifications,
            );
        }
    }

    // A save-as keeps the source file's form: writing a CRLF latin-1 file out under
    // a new name should produce the same kind of file, not silently convert it.
    let bytes = match session.active().encode_for_save() {
        Ok(bytes) => bytes,
        Err(message) => {
            return report_file_error(session, Some(path), &message, snapshots, notifications);
        }
    };
    if let Err(message) = write_atomic(&path, &bytes) {
        return report_file_error(session, Some(path), &message, snapshots, notifications);
    }

    let doc = session.active_mut();
    // A different target is a different document to the server; re-announce it so
    // diagnostics track the new name rather than the old one.
    let renamed = doc.path.as_deref() != Some(path.as_path());
    let dropped = renamed.then(|| doc.path.clone()).flatten();
    let id = doc.id;
    doc.disk = DiskStamp::read(&path);
    if renamed {
        doc.lsp_dirty = true;
        // This is the way out of a read-only buffer (SPEC §10.3): the write just
        // proved the new file writable, and the buffer now matches it exactly - so
        // whichever of the two reasons applied, it no longer does.
        doc.read_only = None;
    }
    doc.path = Some(path.clone());
    let watched = path.clone();
    // The write is now the on-disk state (SPEC §2.4, §8): undoing back to this node
    // later clears the modified marker, exactly as a plain save.
    doc.history.mark_saved();
    let _ = notifications.try_send(Notification::FileSaved {
        buffer_id: id,
        path,
    });
    if renamed && let Some(lsp) = &mut session.lsp {
        lsp.forget(id);
    }
    if let Some(dropped) = dropped {
        session.watch_request(WatchRequest::Unwatch(dropped));
    }
    if renamed {
        session.watch_request(WatchRequest::Watch(watched));
    }
    snapshots.publish(session.snapshot(None))
}

/// Emit a `FileError` and republish current state, leaving the buffer untouched
/// (SPEC §8: a failed file op never loses work). Returns the publish's liveness so
/// callers can `return report_file_error(...)` directly.
fn report_file_error(
    session: &mut Session,
    path: Option<PathBuf>,
    message: &str,
    snapshots: &SnapshotSink,
    notifications: &Sender<Notification>,
) -> bool {
    let _ = notifications.try_send(Notification::FileError {
        buffer_id: session.active().id,
        path,
        message: message.to_string(),
    });
    snapshots.publish(session.snapshot(None))
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
fn selections_after_edits(before: &SelectionSet, edits: &[Edit]) -> SelectionSet {
    let cursors: Vec<Selection> = before
        .all()
        .iter()
        .map(|sel| Selection::cursor(Anchor::after(sel.head).transform_through(edits).offset()))
        .collect();
    let mut set = SelectionSet::from_sorted_cursors(cursors);
    // Carry the primary across the edit: transform its caret the same way and keep
    // whichever surviving cursor lands there as primary, so the viewport follows the
    // cursor the user was on instead of snapping to the topmost caret.
    let primary_head = Anchor::after(before.primary().head)
        .transform_through(edits)
        .offset();
    set.retarget_primary(primary_head);
    set
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;
