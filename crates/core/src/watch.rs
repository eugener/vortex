//! Noticing that an open file changed outside the editor (SPEC §8, §10.2).
//!
//! The seam here is the same one the LSP and syntax producers use (SPEC §2.3): the
//! frontend owns the watcher and its OS threads, and talks to the core's single
//! owner by message. The core sends [`WatchRequest`]s naming the files it has open
//! and receives [`FileEvent`]s back; it never touches `notify`, which keeps
//! `vortex-core` free of another platform-specific dependency and lets a remote
//! frontend watch files on the machine they actually live on.
//!
//! **The policy stays in the core**, because it is about buffers, not files: a
//! clean buffer is reloaded, a modified one raises the conflict for the user to
//! resolve, and neither decision is one a frontend should be able to get wrong
//! (SPEC §8).
//!
//! Deliberately not debounced here. An editor's own save is the noisiest source of
//! these events, and the core already ignores changes whose timestamp matches what
//! it last wrote - which handles the duplicates a debouncer would, without a second
//! dependency and its added latency.

use std::path::PathBuf;

use async_channel::{Receiver, Sender};

/// Something happened to a watched file.
///
/// One variant on purpose: "changed" is all a watcher can report that the core
/// cannot work out better itself. Whether the file was edited, replaced by a
/// rename, or deleted is answered by looking at it, and the core has to look
/// anyway - so distinguishing them here would only be a second, staler source of
/// truth to disagree with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// `path` was created, written, renamed over, or removed.
    Changed(PathBuf),
}

/// Which files the core wants watched. Sent as documents bind and release paths -
/// on open, on a save-as that renames, and on close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchRequest {
    Watch(PathBuf),
    Unwatch(PathBuf),
}

/// The frontend's watcher, handed to the core the same way an [`LspHandle`] is
/// (`CoreHandle::watch`). The core swaps in a later handle over an earlier one and
/// re-announces every open file to it, so attaching mid-session needs no special
/// case.
///
/// [`LspHandle`]: crate::lsp::LspHandle
pub struct WatchHandle {
    /// watcher -> core.
    pub events: Receiver<FileEvent>,
    /// core -> watcher.
    pub requests: Sender<WatchRequest>,
}
