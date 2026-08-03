//! Reading the repository a buffer lives in (SPEC §3, §7.5) - the head bar's branch
//! segment and the gutter's diff signs.
//!
//! **Frontend-owned, like the file watcher and the project walk**, and for the same
//! reason: a repository is filesystem state, so the core stays headless and a future
//! remote frontend reads the machine the files are actually on. Only the *vocabulary*
//! crosses the seam - [`GitSign`] is a core enum, the way [`vortex_core::Severity`]
//! is, so a theme maps it to a glyph and a color and `git2` never appears in a
//! signature the core can see.
//!
//! Two questions and no more, which is why this module is small:
//!
//! - **What branch am I on** ([`Repo::head`]). The one identity fact a full-screen
//!   editor hides from you, and a wrong-branch edit is expensive (§7.5).
//! - **Which of this buffer's lines differ from HEAD** ([`Repo::signs`]). Against the
//!   **buffer**, not the file on disk: a sign that appeared only on save would be
//!   telling you what you already knew.
//!
//! **Every entry point returns `Option`/`Result` and none of them panic.** Not being
//! in a repository is the ordinary case, not an error - a scratch buffer, a file under
//! `/tmp`, a fresh directory - so it reads as `None` all the way through rather than
//! as something to report.

use std::path::Path;

use vortex_core::GitSign;

/// A discovered repository, held open across queries.
///
/// Opening is a filesystem walk up to the root, so it is done once per buffer rather
/// than once per diff. `git2::Repository` is not `Sync`, which is a constraint worth
/// naming rather than working around: this type stays on whichever thread opened it,
/// and the git-diff task owns its own.
pub struct Repo {
    inner: git2::Repository,
}

/// A line's difference from HEAD, as the gutter paints it: 0-based line index and
/// what happened there.
pub type Sign = (usize, GitSign);

impl Repo {
    /// Discover the repository containing `path`, if there is one.
    ///
    /// `None` covers every ordinary not-a-repository case - an unnamed buffer, a file
    /// outside any working tree - and also a repository we cannot read. The gutter
    /// shows nothing either way, which is the honest rendering of "no answer".
    pub fn discover(path: &Path) -> Option<Self> {
        let inner = git2::Repository::discover(path).ok()?;
        Some(Self { inner })
    }

    /// The short name of the checked-out branch (`main`), or `None` on a detached
    /// HEAD or an unborn one.
    ///
    /// An unborn HEAD - a repository with no commit yet - is `None` rather than the
    /// branch name git would create on the first commit: the head bar reports state,
    /// and "the branch you would be on" is not state.
    pub fn head(&self) -> Option<String> {
        let head = self.inner.head().ok()?;
        // A detached HEAD is not a branch, and reporting the commit it points at
        // would answer a question the segment is not asking.
        if !head.is_branch() {
            return None;
        }
        // `shorthand` is a `Result` here, not an `Option`: it fails on a ref name
        // that is not UTF-8, which is possible and is simply "no answer" for a bar.
        head.shorthand().ok().map(str::to_owned)
    }

    /// Whether the working tree has changes at all - the `*` beside the branch.
    ///
    /// Asked of the whole repository rather than of the open buffer, because that is
    /// the question the mark answers: *this checkout* has uncommitted work, including
    /// in files you do not have open. Untracked files are included for the same
    /// reason - a new file nobody has staged is uncommitted work.
    pub fn is_dirty(&self) -> bool {
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true).include_ignored(false);
        self.inner
            .statuses(Some(&mut options))
            .is_ok_and(|statuses| !statuses.is_empty())
    }

    /// How `buffer`'s lines differ from the committed version of `path`.
    ///
    /// **The buffer is the new side**, so a line is marked as you type it rather than
    /// once you save - which is the whole point of a sign in the gutter. `path` is
    /// only used to find the blob and to let git's own attributes apply; its contents
    /// on disk are never read.
    ///
    /// A file with no committed version yet (newly added, or untracked) reports every
    /// line [`GitSign::Added`], which is what it is. A file that has not changed
    /// reports nothing.
    ///
    /// Line indices are **0-based**, matching everything else the frontend passes
    /// around; git counts from 1 and the conversion happens here so no caller has to
    /// remember it.
    pub fn signs(&self, path: &Path, buffer: &[u8]) -> Vec<Sign> {
        let Some(relative) = self.relative(path) else {
            return Vec::new();
        };
        let Some(blob) = self.committed_blob(&relative) else {
            // No committed version: the whole file is new.
            return (0..line_count(buffer))
                .map(|line| (line, GitSign::Added))
                .collect();
        };
        let patch = git2::Patch::from_blob_and_buffer(
            &blob,
            Some(&relative),
            buffer,
            Some(&relative),
            None,
        );
        let Ok(patch) = patch else {
            return Vec::new();
        };
        signs_from_patch(&patch)
    }

    /// `path` relative to the working tree, which is the form git indexes by.
    /// `None` for a path outside the tree entirely.
    ///
    /// The **parent** is canonicalized, never the path itself: a buffer can name a
    /// file that does not exist yet (a save-as target, a new file being typed into),
    /// and canonicalizing that fails outright. Resolving the directory still fixes
    /// the case this is here for - a workdir reached through a symlink, which is the
    /// normal state of `/tmp` on macOS - since it is the directories that are linked.
    fn relative(&self, path: &Path) -> Option<std::path::PathBuf> {
        let workdir = self.inner.workdir()?.canonicalize().ok()?;
        let name = path.file_name()?;
        let parent = path.parent()?.canonicalize().ok()?;
        parent
            .join(name)
            .strip_prefix(workdir)
            .ok()
            .map(Path::to_path_buf)
    }

    /// The blob HEAD holds for `relative`, or `None` when HEAD has no such file.
    fn committed_blob(&self, relative: &Path) -> Option<git2::Blob<'_>> {
        let tree = self.inner.head().ok()?.peel_to_tree().ok()?;
        let entry = tree.get_path(relative).ok()?;
        entry.to_object(&self.inner).ok()?.into_blob().ok()
    }
}

/// Lines in `buffer`, counting the way the gutter numbers them: a trailing newline
/// ends the last line rather than starting an empty one.
fn line_count(buffer: &[u8]) -> usize {
    if buffer.is_empty() {
        return 0;
    }
    let lines = buffer.iter().filter(|&&b| b == b'\n').count();
    lines + usize::from(!buffer.ends_with(b"\n"))
}

/// Turn a patch's hunks into one sign per affected line of the **new** side.
///
/// Split out from [`Repo::signs`] so the mapping is testable without a repository:
/// this is where the two off-by-one hazards live - git's 1-based line numbers, and a
/// deletion having no line of its own to sit on.
fn signs_from_patch(patch: &git2::Patch<'_>) -> Vec<Sign> {
    let mut signs = Vec::new();
    for index in 0..patch.num_hunks() {
        let Ok(count) = patch.num_lines_in_hunk(index) else {
            continue;
        };
        // **Walked line by line, never from the hunk's own counts.** `new_lines()`
        // spans the hunk's *context* as well as its changes - three unchanged lines
        // either side by default - so sizing the marks from it paints a whole small
        // file as modified. Only a line git labels `+` or `-` is a change.
        let mut added: Vec<usize> = Vec::new();
        let mut deleted = 0usize;
        // Where a deletion belongs on the new side: the last line that still exists
        // above it. A deletion has no line of its own, so it marks the survivor.
        let mut anchor = 0usize;
        let mut deletion_anchor = None;
        for line in 0..count {
            let Ok(line) = patch.line_in_hunk(index, line) else {
                continue;
            };
            match line.origin() {
                '+' => {
                    let at = line.new_lineno().unwrap_or(1).saturating_sub(1) as usize;
                    anchor = at;
                    added.push(at);
                }
                '-' => {
                    deleted += 1;
                    deletion_anchor.get_or_insert(anchor);
                }
                // Context: it did not change, but it moves the anchor a deletion
                // below it would hang from.
                _ => {
                    if let Some(at) = line.new_lineno() {
                        anchor = at.saturating_sub(1) as usize;
                    }
                }
            }
        }
        // A line standing where a deleted one was is a modification; anything past
        // the lines it replaced is new.
        for (offset, &at) in added.iter().enumerate() {
            let sign = if offset < deleted {
                GitSign::Modified
            } else {
                GitSign::Added
            };
            signs.push((at, sign));
        }
        // Only a hunk that deleted *and added nothing* leaves a removal to report -
        // otherwise the replacement lines above already carry the change, and a
        // second mark would say it twice.
        if added.is_empty() && deleted > 0 {
            signs.push((deletion_anchor.unwrap_or(0), GitSign::Removed));
        }
    }
    signs
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod tests;
