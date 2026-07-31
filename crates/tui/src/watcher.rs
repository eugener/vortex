//! The frontend half of external-change detection (SPEC §10.2).
//!
//! The core decides what a change *means* - reload a clean buffer, raise a
//! conflict for a modified one - and this side is only the machinery that notices
//! one. It owns `notify` and the OS threads it starts, and talks to the core over
//! [`WatchHandle`], the same producer seam the LSP client and the highlighter use
//! (SPEC §2.3).
//!
//! **Directories are watched, not files.** Every careful writer - this editor
//! included - saves by writing a temp file and renaming it over the target, which
//! replaces the file rather than modifying it. A per-file watch is a watch on the
//! *old* file after that: it survives the save that matters and then reports
//! nothing ever again. Watching the containing directory non-recursively sees the
//! rename and keeps working, at the cost of hearing about the directory's other
//! files, which [`WatchSet::resolve`] filters back out.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use notify::{RecursiveMode, Watcher};
use vortex_core::watch::{FileEvent, WatchHandle, WatchRequest};

/// Raw paths queued from `notify`'s own thread to the watcher loop. Generous, and
/// dropped-on-full rather than blocked: the OS thread must never wait on us, and a
/// dropped event costs nothing when the queue is already thousands deep - the core
/// re-reads the file's current state per event, so a hundred queued reports of one
/// rewrite say exactly what one of them says.
const RAW_CAP: usize = 1024;
/// Filtered events out to the core. A change the user made outside the editor is a
/// human-speed occurrence; this only needs to absorb the burst of one save.
const EVENT_CAP: usize = 64;

/// Which files are watched, and the directories that requires (see the module
/// note for why those differ).
///
/// Files are keyed by their directory's *canonical* path plus their file name, so
/// the several spellings one file reaches this by - `notes.md` from argv,
/// `/home/me/notes.md` from the picker, and whatever absolute path the platform
/// reports events with - all land on one entry. The directory is canonicalized
/// rather than the file itself because the file may not exist: a watch outlives
/// the moment a rename replaces it, which is the entire point.
#[derive(Debug, Default)]
pub struct WatchSet {
    files: HashSet<PathBuf>,
    /// How many watched files are in each directory, so the last one leaving is
    /// what stops the watch. Two files open from one directory is the common case,
    /// not a corner one.
    dirs: HashMap<PathBuf, usize>,
    /// The spelling each watch was *requested* with, mapped to the key it resolved to.
    ///
    /// Resolution needs the file's directory to exist, and a directory can be removed
    /// while its file is open (`rm -rf` on a checkout). After that the close which
    /// follows could no longer name what it was releasing, and the entry plus the
    /// `notify` watch under it leaked for the rest of the session. The core asks to
    /// unwatch with the same path it asked to watch, so remembering that spelling
    /// answers without the filesystem.
    requested: HashMap<PathBuf, PathBuf>,
}

impl WatchSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start watching `file`. Returns the directory the caller must hand to
    /// `notify`, or `None` when it is already watched (or the directory does not
    /// exist, in which case there is nothing to watch yet).
    pub fn watch(&mut self, file: &Path) -> Option<PathBuf> {
        let key = resolve_key(file)?;
        let dir = key.parent()?.to_path_buf();
        // Recorded before the dedup check, so a second spelling of an already-watched
        // file can still name it later.
        self.requested.insert(file.to_path_buf(), key.clone());
        if !self.files.insert(key) {
            return None; // already watched; the directory already is too
        }
        let count = self.dirs.entry(dir.clone()).or_insert(0);
        *count += 1;
        (*count == 1).then_some(dir)
    }

    /// Stop watching `file`. Returns the directory the caller must release, or
    /// `None` while other watched files still live in it.
    ///
    /// Falls back to the key this exact path resolved to *when it was watched* (see
    /// [`Self::requested`]) whenever resolving it now does not name something watched -
    /// a directory removed while its file was open, which leaves nothing on disk to
    /// resolve through. Tested as "does the key name something watched" rather than
    /// "did it resolve", because a resolution can succeed and still be the wrong
    /// answer, and a leaked watch is silent either way.
    pub fn unwatch(&mut self, file: &Path) -> Option<PathBuf> {
        let remembered = self.requested.remove(file);
        let key = match resolve_key(file) {
            Some(key) if self.files.contains(&key) => key,
            _ => remembered?,
        };
        let dir = key.parent()?.to_path_buf();
        if !self.files.remove(&key) {
            return None;
        }
        let count = self.dirs.get_mut(&dir)?;
        *count -= 1;
        if *count == 0 {
            self.dirs.remove(&dir);
            return Some(dir);
        }
        None
    }

    /// The watched file an event path refers to, or `None` for the neighbours that
    /// come with watching a whole directory. Returns the resolved form, which is
    /// what the core will match its own open paths against.
    pub fn resolve(&self, path: &Path) -> Option<PathBuf> {
        let key = resolve_key(path)?;
        self.files.contains(&key).then_some(key)
    }

    /// Whether anything is watched, for the loop's own bookkeeping.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// A file's watch key. [`vortex_core::watch::resolve`] does the work; this exists only
/// to name why the watcher wants that particular reduction.
///
/// **Following the link is the whole point.** A save resolves symlinks before it writes
/// (`write_atomic`, so a link stays a link), and so does every other editor and dotfile
/// manager - which means the write lands in the *target's* directory. Watching the
/// link's own directory instead left `~/.vimrc -> ~/dotfiles/vimrc` watching `~/` while
/// every writer touched `~/dotfiles/`, so an external change to a symlinked file was
/// never reported. The core matches events to buffers by the same reduction, so
/// reporting the resolved path still finds the document opened under the link.
fn resolve_key(file: &Path) -> Option<PathBuf> {
    vortex_core::watch::resolve(file)
}

/// Whether an event is worth forwarding. Access events (a file being opened or
/// read) are noise on the backends that report them at all, and `Other` is
/// notify's own meta-channel; everything that creates, changes, or removes a file
/// is kept, including the kinds a future backend might add.
fn is_interesting(kind: notify::EventKind) -> bool {
    !kind.is_access() && !kind.is_other()
}

/// Queue an event's paths for the loop to filter. Runs on `notify`'s own thread,
/// so it does the least possible and never blocks - whatever it is doing, the OS
/// is waiting for it. A full queue drops rather than waits, which is safe because
/// the core re-reads the file's current state per event: a hundred queued reports
/// of one rewrite say exactly what one of them says.
///
/// Named rather than left inline in the closure so it can be tested without
/// standing up a real backend and provoking the OS into sending something.
fn forward(result: notify::Result<notify::Event>, raw: &smol::channel::Sender<PathBuf>) {
    if let Ok(event) = result
        && is_interesting(event.kind)
    {
        for path in event.paths {
            let _ = raw.try_send(path);
        }
    }
}

/// Build the watcher: the handle to hand the core, and the loop the frontend runs
/// on its own thread (the shape [`vortex_core::lsp::client`] uses).
///
/// Fails only if `notify` cannot start its backend at all, which the caller treats
/// as "no external-change detection this session" rather than as fatal - the state
/// every session was in before this existed (SPEC §8).
pub fn watcher() -> notify::Result<(WatchHandle, impl Future<Output = ()>)> {
    let (request_tx, request_rx) = smol::channel::bounded::<WatchRequest>(EVENT_CAP);
    let (event_tx, event_rx) = smol::channel::bounded::<FileEvent>(EVENT_CAP);
    let (raw_tx, raw_rx) = smol::channel::bounded::<PathBuf>(RAW_CAP);

    let watcher = notify::RecommendedWatcher::new(
        move |result| forward(result, &raw_tx),
        notify::Config::default(),
    )?;

    Ok((
        WatchHandle {
            events: event_rx,
            requests: request_tx,
        },
        run(watcher, request_rx, raw_rx, event_tx),
    ))
}

/// What the watcher loop woke up for.
enum Incoming {
    /// The core wants a file watched or released.
    Request(WatchRequest),
    /// `notify` reported a path.
    Raw(PathBuf),
    /// A channel closed: the core is gone, so the watcher has no one to tell.
    Stopped,
}

/// Serve watch requests and forward the events that match them. Ends when the core
/// drops its side, which also drops the `notify` watcher and its threads.
///
/// Generic over the watcher rather than fixed to `RecommendedWatcher` so its
/// behavior can be tested against a stand-in: driving the real one means writing
/// files and waiting on the OS to feel like mentioning it, which is a slow and
/// flaky thing to put in a test suite. What is worth testing here is the routing -
/// which directory gets watched, which events reach the core - and that is exactly
/// what the trait boundary exposes.
async fn run<W: Watcher>(
    mut watcher: W,
    requests: smol::channel::Receiver<WatchRequest>,
    raw: smol::channel::Receiver<PathBuf>,
    events: smol::channel::Sender<FileEvent>,
) {
    let mut set = WatchSet::new();
    loop {
        // `or` is biased to the first future, which is the right bias: applying a
        // pending watch request before draining a burst of paths means a file the
        // core just opened starts being reported a moment sooner, and the raw
        // queue is what has slack to spare.
        let incoming = smol::future::or(
            async {
                requests
                    .recv()
                    .await
                    .map_or(Incoming::Stopped, Incoming::Request)
            },
            async { raw.recv().await.map_or(Incoming::Stopped, Incoming::Raw) },
        )
        .await;

        match incoming {
            Incoming::Stopped => break,
            Incoming::Request(WatchRequest::Watch(path)) => {
                if let Some(dir) = set.watch(&path) {
                    // A directory that cannot be watched is not fatal: that file
                    // simply goes unwatched, and every other one still works.
                    let _ = watcher.watch(&dir, RecursiveMode::NonRecursive);
                }
            }
            Incoming::Request(WatchRequest::Unwatch(path)) => {
                if let Some(dir) = set.unwatch(&path) {
                    let _ = watcher.unwatch(&dir);
                }
            }
            Incoming::Raw(path) => {
                if let Some(watched) = set.resolve(&path)
                    && events.send(FileEvent::Changed(watched)).await.is_err()
                {
                    break; // the core is gone
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
