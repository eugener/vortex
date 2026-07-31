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

use std::path::{Path, PathBuf};

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

/// Reduce a path to the one form both sides of this seam agree on: symlinks followed,
/// directory canonicalized, file name appended. `None` when there is no file name, or
/// when even the directory cannot be resolved.
///
/// **This lives here because two crates have to compute it identically or events go
/// nowhere.** The frontend keys its watch set by it; the core matches an incoming event
/// against its open documents by it. The two are fed *different spellings of the same
/// file* by construction - argv passes what the user typed, the picker passes an
/// absolute path, and the platform reports events with a third - so the only thing that
/// makes an event find its buffer is that both sides reduce a path the same way. Two
/// copies of that reduction is one copy too many: they were briefly kept in step by a
/// pair of doc comments, and had already drifted on the broken-symlink case below.
///
/// Three steps, in order:
///
/// 1. **Canonicalize.** Answers the ordinary case, and follows a symlink to the file
///    whose bytes actually change - which is where every writer, this editor included,
///    lands, because a save resolves the link before writing.
/// 2. **Follow a broken link one hop.** A symlink whose target was just deleted cannot
///    be canonicalized, and resolving *it* through its own directory would name the
///    link rather than the file everything else is talking about. One hop, not a chain:
///    a link to a link to a deleted file is not worth a second `readlink`.
/// 3. **Resolve the directory and re-attach the name.** A file that does not exist -
///    deleted, or not yet created - has nothing to canonicalize, but its directory
///    usually does. This is what lets a watch outlive the rename that replaces the
///    file, which is the whole reason directories are what get watched.
pub fn resolve(path: &Path) -> Option<PathBuf> {
    if let Ok(real) = path.canonicalize() {
        return Some(real);
    }
    // A relative link target is relative to the link's own directory.
    let followed = path.read_link().ok().map(|target| {
        if target.is_absolute() {
            target
        } else {
            path.parent().unwrap_or(Path::new(".")).join(target)
        }
    });
    let path = followed.as_deref().unwrap_or(path);
    let name = path.file_name()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Some(parent.canonicalize().ok()?.join(name))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway directory, removed when the test ends.
    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("vortex-resolve-{tag}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn every_spelling_of_one_file_reduces_to_the_same_key() {
        // The property the whole seam rests on: the core and the frontend are handed
        // different spellings and must arrive at one answer, or an event never finds
        // the buffer holding the file it is about.
        let dir = Dir::new("spellings");
        let file = dir.0.join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        let canonical = resolve(&file).unwrap();

        let root = dir.0.canonicalize().unwrap();
        assert_eq!(resolve(&root.join("notes.txt")).unwrap(), canonical);
        assert_eq!(resolve(&dir.0.join("./notes.txt")).unwrap(), canonical);
        std::fs::create_dir(dir.0.join("sub")).unwrap();
        assert_eq!(resolve(&dir.0.join("sub/../notes.txt")).unwrap(), canonical);
    }

    #[test]
    fn a_deleted_file_still_reduces_through_its_directory() {
        // The case that made deletions unreportable: nothing to canonicalize, but the
        // directory is still there and the name is still the name.
        let dir = Dir::new("deleted");
        let file = dir.0.join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        let before = resolve(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(resolve(&file).unwrap(), before, "the key outlives the file");
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_link_reduces_to_what_it_pointed_at() {
        // Step 2, and the one the two copies of this used to disagree about. While the
        // target exists, canonicalizing gets there; once it is deleted, only reading
        // the link does - and everything else in the system is talking about the
        // target, because that is where writers write.
        let dir = Dir::new("broken-link");
        let target = dir.0.join("real.txt");
        std::fs::write(&target, "x").unwrap();
        let link = dir.0.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let through_link = resolve(&link).unwrap();
        assert_eq!(through_link, resolve(&target).unwrap(), "follows the link");
        std::fs::remove_file(&target).unwrap();
        assert_eq!(
            resolve(&link).unwrap(),
            through_link,
            "and keeps naming the target once it is gone"
        );
    }

    #[test]
    fn a_path_with_nothing_to_resolve_answers_nothing() {
        assert_eq!(
            resolve(Path::new("/no-such-dir-here/x/y.txt")),
            None,
            "no file to canonicalize and no directory to resolve through"
        );
        // A root path has no file name, but it does canonicalize, so it answers.
        assert!(resolve(Path::new("/")).is_some());
    }
}
