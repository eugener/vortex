//! Tests for the repository read layer, against real repositories built in a temp
//! directory - `git2` all the way down, so a hunk this asserts on is a hunk libgit2
//! actually produced rather than one a stub invented.

use super::*;
use crate::testutil::TempDir;

/// A repository with one commit containing `body` at `file`.
///
/// Signature and identity are set on the repo rather than read from the machine's
/// git config: a test that depends on the developer having `user.email` set is a
/// test that fails on a fresh CI image for a reason that has nothing to do with it.
fn repo_with(file: &str, body: &str) -> (TempDir, Repo) {
    let dir = TempDir::new();
    let repository = git2::Repository::init(&dir.path).unwrap();
    dir.file(file, body);
    commit_all(&repository, "initial");
    let repo = Repo::discover(&dir.path.join(file)).unwrap();
    (dir, repo)
}

fn commit_all(repository: &git2::Repository, message: &str) {
    let mut index = repository.index().unwrap();
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap();
    index.write().unwrap();
    let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
    let who = git2::Signature::now("Test", "test@example.com").unwrap();
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repository
        .commit(Some("HEAD"), &who, &who, message, &tree, &parents)
        .unwrap();
}

#[test]
fn a_path_outside_any_repository_reads_as_no_repository() {
    // The ordinary case, not an error: a scratch file under the temp root has no
    // working tree above it, and the gutter shows nothing rather than reporting it.
    let dir = TempDir::new();
    dir.file("loose.txt", "hello\n");
    // Only meaningful if the temp root is genuinely not inside a checkout.
    if Repo::discover(&dir.path).is_some() {
        return;
    }
    assert!(Repo::discover(&dir.path.join("loose.txt")).is_none());
}

#[test]
fn the_branch_is_reported_by_its_short_name() {
    let (_dir, repo) = repo_with("a.txt", "one\ntwo\n");
    let branch = repo.head().expect("a committed repository has a branch");
    // Whatever this git installation calls its first branch - the test pins the
    // *shape* (a short name, not `refs/heads/...`), never the name itself.
    assert!(!branch.contains('/'), "not shorthand: {branch}");
    assert!(!branch.is_empty());
}

#[test]
fn an_unborn_head_has_no_branch_to_report() {
    // A repository with no commit yet: git would create a branch on the first
    // commit, but "the branch you would be on" is not state the head bar reports.
    let dir = TempDir::new();
    git2::Repository::init(&dir.path).unwrap();
    dir.file("a.txt", "one\n");
    let repo = Repo::discover(&dir.path.join("a.txt")).unwrap();
    assert_eq!(repo.head(), None);
}

#[test]
fn a_clean_checkout_is_not_dirty_and_an_edited_one_is() {
    let (dir, repo) = repo_with("a.txt", "one\ntwo\n");
    assert!(!repo.is_dirty(), "nothing has been touched yet");
    dir.file("a.txt", "one\nchanged\n");
    assert!(repo.is_dirty(), "an edited working tree is dirty");
}

#[test]
fn an_untracked_file_counts_as_uncommitted_work() {
    // The mark says "this checkout has work you have not committed", and a file
    // nobody has staged is exactly that.
    let (dir, repo) = repo_with("a.txt", "one\n");
    assert!(!repo.is_dirty());
    dir.file("new.txt", "fresh\n");
    assert!(repo.is_dirty());
}

#[test]
fn an_unchanged_buffer_earns_no_signs() {
    let (dir, repo) = repo_with("a.txt", "one\ntwo\nthree\n");
    let signs = repo.signs(&dir.path.join("a.txt"), b"one\ntwo\nthree\n");
    assert!(signs.is_empty(), "{signs:?}");
}

#[test]
fn a_changed_line_is_marked_where_it_changed_and_nowhere_else() {
    // The core claim of the feature, and the 0-based conversion with it: git counts
    // lines from 1, every other frontend index counts from 0.
    let (dir, repo) = repo_with("a.txt", "one\ntwo\nthree\n");
    let signs = repo.signs(&dir.path.join("a.txt"), b"one\nCHANGED\nthree\n");
    assert_eq!(signs, vec![(1, GitSign::Modified)]);
}

#[test]
fn the_signs_follow_the_buffer_rather_than_the_file_on_disk() {
    // The whole point of a sign: it appears as you type, not when you save. The file
    // on disk still holds the committed text here, and the buffer does not.
    let (dir, repo) = repo_with("a.txt", "one\ntwo\n");
    let path = dir.path.join("a.txt");
    assert!(repo.signs(&path, b"one\ntwo\n").is_empty(), "disk is clean");
    let signs = repo.signs(&path, b"one\nedited in the buffer\n");
    assert_eq!(signs, vec![(1, GitSign::Modified)]);
    // …and the file itself was never touched, which is what makes that meaningful.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
}

#[test]
fn added_lines_read_as_added_and_replaced_ones_as_modified() {
    // A hunk that both replaces and grows: the lines standing in for old ones are
    // modifications, and the surplus past them is new.
    let (dir, repo) = repo_with("a.txt", "one\ntwo\n");
    let signs = repo.signs(&dir.path.join("a.txt"), b"one\nTWO\nTHREE\nFOUR\n");
    assert_eq!(
        signs,
        vec![
            (1, GitSign::Modified),
            (2, GitSign::Added),
            (3, GitSign::Added)
        ]
    );
}

#[test]
fn a_deletion_marks_the_line_that_survived_it() {
    // A deletion has no line of its own to sit on, so it marks the survivor - the
    // convention every editor's gutter uses, and the reason `Removed` exists at all.
    let (dir, repo) = repo_with("a.txt", "one\ntwo\nthree\n");
    let signs = repo.signs(&dir.path.join("a.txt"), b"one\nthree\n");
    assert_eq!(signs.len(), 1, "{signs:?}");
    assert_eq!(signs[0].1, GitSign::Removed);
}

#[test]
fn a_file_with_no_committed_version_is_new_all_through() {
    let (dir, repo) = repo_with("a.txt", "one\n");
    let signs = repo.signs(&dir.path.join("fresh.txt"), b"alpha\nbeta\n");
    assert_eq!(
        signs,
        vec![(0, GitSign::Added), (1, GitSign::Added)],
        "an untracked file is entirely new"
    );
    // An empty new file has no lines, so it has nothing to mark.
    assert!(repo.signs(&dir.path.join("empty.txt"), b"").is_empty());
}

#[test]
fn a_path_outside_the_working_tree_is_not_diffed_against_it() {
    // Asking about someone else's file must not report the repository's own lines.
    let (_dir, repo) = repo_with("a.txt", "one\n");
    let elsewhere = TempDir::new();
    elsewhere.file("other.txt", "unrelated\n");
    let signs = repo.signs(&elsewhere.path.join("other.txt"), b"unrelated\n");
    assert!(signs.is_empty(), "{signs:?}");
}

#[test]
fn a_buffer_without_a_trailing_newline_counts_its_last_line() {
    assert_eq!(line_count(b""), 0);
    assert_eq!(line_count(b"one\n"), 1);
    assert_eq!(line_count(b"one"), 1);
    assert_eq!(line_count(b"one\ntwo"), 2);
    assert_eq!(line_count(b"one\ntwo\n"), 2);
}
