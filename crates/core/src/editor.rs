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
    /// The file this buffer is bound to (`Open`/`Save`), or `None` if unnamed.
    path: Option<PathBuf>,
    /// Whether the buffer differs from its on-disk file. Set by any applied edit,
    /// cleared by a successful save or a fresh open (SPEC §8, §10). Independent of
    /// `version`, which never resets.
    modified: bool,
}

impl Editor {
    fn new() -> Self {
        Self {
            id: BufferId(0),
            buffer: RopeBuffer::new(),
            selections: SelectionSet::at_origin(),
            version: 0,
            path: None,
            modified: false,
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
            path: self.path.clone(),
            modified: self.modified,
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

/// What the actor loop must do for one action: apply a text edit, republish the
/// current state (a motion or snapshot request), or a file op (open/save). Each
/// path returns "is the frontend still alive?"; `Quit` breaks before this.
enum Step {
    Edit(EditKind),
    Republish,
    Open(PathBuf),
    Save,
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
            Action::Open(path) => Step::Open(path),
            Action::Save => Step::Save,
            Action::Quit => break,
        };

        let alive = match step {
            Step::Edit(kind) => {
                apply_edit(&mut editor, kind, &deltas, &snapshots, &notifications).await
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
    let mut applied = 0usize;

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
        applied += 1;
        dirty = Some(match dirty {
            None => range.start..range.start + new_text.len(),
            Some(d) => d.start.min(range.start)..d.end.max(range.start + new_text.len()),
        });
    }

    // If every planned edit was rejected, nothing changed: do not bump the
    // version or remap selections (a version bump with no delta would diverge a
    // remote frontend replaying the delta stream, SPEC §5). Republish current
    // state so the frontend's view stays current, matching the empty-plan case.
    if applied == 0 {
        return snapshots.publish(editor.snapshot(None));
    }

    // Remap selections to sit at the end of each applied edit (a cursor after the
    // inserted text / at the deletion point). Recomputed from the edits so the
    // set stays disjoint and sorted.
    editor.selections = selections_after_edits(&edits);
    editor.version += 1;
    // The buffer now differs from its on-disk file (if any) until the next save.
    editor.modified = true;
    snapshots.publish(editor.snapshot(dirty))
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

    // A freshly opened buffer starts at the origin and matches disk.
    editor.selections = SelectionSet::at_origin();
    editor.path = Some(path.clone());
    editor.modified = false;

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

    editor.modified = false;
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
    // Whether a real file exists at the resolved target (false for a first save
    // through a dangling symlink): governs whether there is a mode to preserve.
    let target_exists = fs::metadata(&target).is_ok();

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
    // The target's current mode (if it exists), so the temp is *created* no wider
    // than the destination - a 0600 file's contents must never touch disk in a
    // 0644 temp, even briefly, before being narrowed (that window would expose
    // e.g. an SSH key to any local user for the length of the write + fsync).
    let target_mode = if target_exists {
        fs::metadata(&target).ok().map(|m| m.permissions())
    } else {
        None
    };
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
#[path = "editor_tests.rs"]
mod tests;
