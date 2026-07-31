use super::*;

use crate::testutil::TempDir;

#[test]
fn the_first_file_in_a_directory_starts_the_watch_and_the_last_ends_it() {
    let dir = TempDir::new();
    let a = dir.path.join("a.txt");
    let b = dir.path.join("b.txt");
    let mut set = WatchSet::new();

    let started = set.watch(&a).expect("first file starts a watch");
    assert_eq!(started, dir.path.canonicalize().unwrap());
    // The second file in the same directory needs no second watch.
    assert_eq!(set.watch(&b), None);

    assert_eq!(set.unwatch(&a), None); // b is still there
    assert_eq!(
        set.unwatch(&b),
        Some(dir.path.canonicalize().unwrap()),
        "the last file out releases the directory"
    );
    assert!(set.is_empty());
}

#[test]
fn watching_the_same_file_twice_is_not_two_files() {
    // The core re-announces every open file when a watcher attaches, so a file
    // already watched arrives again as a matter of course. Counting it twice would
    // leave the directory watched forever after it closed.
    let dir = TempDir::new();
    let a = dir.path.join("a.txt");
    let mut set = WatchSet::new();

    assert!(set.watch(&a).is_some());
    assert_eq!(set.watch(&a), None);
    assert!(set.unwatch(&a).is_some(), "one close releases it");
}

#[test]
fn closing_a_file_whose_directory_is_gone_still_releases_the_watch() {
    // `rm -rf` on a checkout, then close the buffer. `resolve_key` canonicalizes the
    // parent, which no longer exists, so the unwatch used to return before touching
    // anything - leaving the entry and the OS watch under it for the session. That is
    // the accumulation the Unwatch request exists to prevent, in the one case that
    // reliably produces it.
    let dir = TempDir::new();
    let a = dir.path.join("a.txt");
    let mut set = WatchSet::new();
    let watched = set.watch(&a).expect("the first file starts a watch");

    std::fs::remove_dir_all(&dir.path).unwrap();
    assert_eq!(
        set.unwatch(&a),
        Some(watched),
        "the directory must still be released"
    );
    assert!(set.is_empty(), "and nothing may be left behind");
}

#[cfg(unix)]
#[test]
fn closing_a_symlink_whose_target_is_gone_still_releases_the_watch() {
    // The nastier half of the same leak. Here `resolve_key` *succeeds* - a broken link
    // resolves through its own directory - and hands back a path that was never
    // watched, so a fallback that only fires on failure never runs and the entry is
    // stranded. What matters is whether the resolved key names something watched, not
    // whether it resolved.
    let dir = TempDir::new();
    let target_dir = dir.path.join("dotfiles");
    std::fs::create_dir(&target_dir).unwrap();
    let target = target_dir.join("vimrc");
    std::fs::write(&target, "set nocompatible\n").unwrap();
    let link = dir.path.join(".vimrc");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let mut set = WatchSet::new();
    let watched = set.watch(&link).expect("the link's target starts a watch");
    assert_eq!(watched, target_dir.canonicalize().unwrap());

    std::fs::remove_file(&target).unwrap();
    assert_eq!(
        set.unwatch(&link),
        Some(watched),
        "a broken link must still release the directory it was watching"
    );
    assert!(set.is_empty(), "and nothing may be left behind");
}

#[test]
fn unwatching_something_never_watched_releases_nothing() {
    let dir = TempDir::new();
    let mut set = WatchSet::new();
    assert_eq!(set.unwatch(&dir.path.join("never.txt")), None);
    assert!(set.is_empty());
}

#[test]
fn events_are_matched_across_the_spellings_one_file_arrives_as() {
    // argv passes what the user typed, the picker sends an absolute path, and the
    // platform reports events with a third spelling of its own. All three are the
    // same file, and a set that missed that would watch a file and then ignore
    // every event about it.
    let dir = TempDir::new();
    let path = dir.path.join("notes.md");
    std::fs::write(&path, "hi").unwrap();

    let mut set = WatchSet::new();
    set.watch(&path).expect("watch starts");

    let canonical = path.canonicalize().unwrap();
    assert_eq!(set.resolve(&canonical), Some(canonical.clone()));
    // Same file reached through a redundant `.` component.
    let indirect = dir.path.join(".").join("notes.md");
    assert_eq!(set.resolve(&indirect), Some(canonical));
}

#[test]
fn a_neighbour_in_a_watched_directory_is_not_forwarded() {
    // Watching directories is what makes rename-based saves detectable; the price
    // is hearing about every other file beside it, which stops here rather than
    // waking the core for every build artifact.
    let dir = TempDir::new();
    let watched = dir.path.join("watched.txt");
    let mut set = WatchSet::new();
    set.watch(&watched).expect("watch starts");

    assert_eq!(set.resolve(&dir.path.join("someone-elses.txt")), None);
}

#[test]
fn a_file_whose_directory_is_gone_is_not_watchable() {
    // Opening a not-yet-created file in a not-yet-created directory: there is
    // nothing to watch, and asking notify to watch it would only error.
    let dir = TempDir::new();
    let mut set = WatchSet::new();
    assert_eq!(set.watch(&dir.path.join("missing").join("file.txt")), None);
    assert!(set.is_empty());
}

#[test]
fn a_path_with_no_file_name_is_not_watchable() {
    let mut set = WatchSet::new();
    assert_eq!(set.watch(Path::new("/")), None);
    assert_eq!(set.resolve(Path::new("/")), None);
}

#[test]
fn a_bare_file_name_resolves_against_the_current_directory() {
    // `vortex notes.md` reaches the core as exactly that, with no parent at all.
    let mut set = WatchSet::new();
    let started = set.watch(Path::new("Cargo.toml"));
    assert_eq!(started, Some(std::env::current_dir().unwrap()));
    assert!(
        set.resolve(&std::env::current_dir().unwrap().join("Cargo.toml"))
            .is_some()
    );
}

#[test]
fn access_and_meta_events_are_filtered_out() {
    use notify::EventKind;
    use notify::event::{AccessKind, CreateKind, ModifyKind, RemoveKind};

    assert!(!is_interesting(EventKind::Access(AccessKind::Open(
        notify::event::AccessMode::Read
    ))));
    assert!(!is_interesting(EventKind::Other));
    assert!(is_interesting(EventKind::Create(CreateKind::File)));
    assert!(is_interesting(EventKind::Modify(ModifyKind::Any)));
    assert!(is_interesting(EventKind::Remove(RemoveKind::File)));
    // A kind notify has not invented yet still counts: missing a real change is
    // worse than forwarding one the core will find nothing to do about.
    assert!(is_interesting(EventKind::Any));
}

/// A stand-in for `notify`'s watcher that records what it was asked to follow.
/// The loop's job is routing, not watching, so this is what the routing tests
/// need - and it makes them deterministic, where a real backend would mean
/// writing files and waiting on the OS to notice.
#[derive(Default)]
struct FakeWatcher {
    watched: std::sync::Arc<std::sync::Mutex<Vec<(String, PathBuf)>>>,
}

impl notify::Watcher for FakeWatcher {
    fn new<F: notify::EventHandler>(_handler: F, _config: notify::Config) -> notify::Result<Self> {
        Ok(Self::default())
    }

    fn watch(&mut self, path: &Path, _mode: RecursiveMode) -> notify::Result<()> {
        self.watched
            .lock()
            .unwrap()
            .push(("watch".into(), path.to_path_buf()));
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.watched
            .lock()
            .unwrap()
            .push(("unwatch".into(), path.to_path_buf()));
        Ok(())
    }

    fn kind() -> notify::WatcherKind {
        notify::WatcherKind::NullWatcher
    }
}

/// Run the loop over a fake watcher until it ends, returning what the watcher was
/// asked to do and what reached the core.
///
/// Both queues are pre-filled, and only the *raw* one is closed: the loop is
/// biased to requests, so a closed request channel would stop it before it ever
/// looked at a path. Keeping that side open and ending on the raw side drains the
/// requests first and then the paths, which is also the order they matter in.
fn run_loop(
    requests: Vec<WatchRequest>,
    raw: Vec<PathBuf>,
) -> (Vec<(String, PathBuf)>, Vec<FileEvent>) {
    let (request_tx, request_rx) = smol::channel::unbounded::<WatchRequest>();
    let (raw_tx, raw_rx) = smol::channel::unbounded::<PathBuf>();
    let (event_tx, event_rx) = smol::channel::unbounded::<FileEvent>();
    for r in requests {
        request_tx.try_send(r).unwrap();
    }
    for p in raw {
        raw_tx.try_send(p).unwrap();
    }
    raw_tx.close(); // a closed channel still yields what is already queued

    let watcher = FakeWatcher::default();
    let calls = std::sync::Arc::clone(&watcher.watched);
    smol::block_on(run(watcher, request_rx, raw_rx, event_tx));
    drop(request_tx);

    let events = std::iter::from_fn(|| event_rx.try_recv().ok()).collect();
    let calls = calls.lock().unwrap().clone();
    (calls, events)
}

#[test]
fn a_watch_request_watches_the_containing_directory() {
    // Not the file: a save that renames a temp over the target replaces the file,
    // and a per-file watch would be left holding the one that was replaced.
    let dir = TempDir::new();
    let file = dir.path.join("notes.txt");
    let (calls, _) = run_loop(vec![WatchRequest::Watch(file)], Vec::new());
    assert_eq!(
        calls,
        vec![("watch".to_string(), dir.path.canonicalize().unwrap())]
    );
}

#[test]
fn the_directory_is_released_only_when_its_last_file_goes() {
    let dir = TempDir::new();
    let a = dir.path.join("a.txt");
    let b = dir.path.join("b.txt");
    let (calls, _) = run_loop(
        vec![
            WatchRequest::Watch(a.clone()),
            WatchRequest::Watch(b.clone()),
            WatchRequest::Unwatch(a),
            WatchRequest::Unwatch(b),
        ],
        Vec::new(),
    );
    let canonical = dir.path.canonicalize().unwrap();
    assert_eq!(
        calls,
        vec![
            ("watch".to_string(), canonical.clone()),
            ("unwatch".to_string(), canonical)
        ],
        "one watch for the pair, and one release when the second closes"
    );
}

#[test]
fn only_events_for_watched_files_reach_the_core() {
    let dir = TempDir::new();
    let watched = dir.path.join("watched.txt");
    let neighbour = dir.path.join("neighbour.txt");
    let (_, events) = run_loop(
        vec![WatchRequest::Watch(watched.clone())],
        vec![neighbour, watched.clone()],
    );
    // Forwarded in resolved form, which is what the core matches its open paths
    // against - the same file reported under a different spelling is the same file.
    let expected = dir.path.canonicalize().unwrap().join("watched.txt");
    assert_eq!(events, vec![FileEvent::Changed(expected)]);
}

#[test]
fn an_unwatch_for_a_file_never_watched_touches_the_watcher() {
    // The core unwatches on close, and a buffer can be closed without its path
    // ever having been watchable - a file in a directory that no longer exists.
    let dir = TempDir::new();
    let (calls, _) = run_loop(
        vec![WatchRequest::Unwatch(dir.path.join("ghost.txt"))],
        Vec::new(),
    );
    assert!(calls.is_empty());
}

#[test]
fn the_loop_ends_when_the_core_stops_listening() {
    // Both ways this thread is asked to finish. First: the core is gone by the time
    // a matching event is ready to forward, so the send fails and the loop stops
    // rather than spinning on a dead channel.
    let dir = TempDir::new();
    let watched = dir.path.join("watched.txt");
    let (request_tx, request_rx) = smol::channel::unbounded::<WatchRequest>();
    let (raw_tx, raw_rx) = smol::channel::unbounded::<PathBuf>();
    let (event_tx, event_rx) = smol::channel::unbounded::<FileEvent>();
    request_tx
        .try_send(WatchRequest::Watch(watched.clone()))
        .unwrap();
    raw_tx.try_send(watched).unwrap();
    drop(event_rx);
    smol::block_on(run(FakeWatcher::default(), request_rx, raw_rx, event_tx));
    drop(request_tx);

    // Second: the core drops the request side, which is what shutdown looks like.
    let (request_tx, request_rx) = smol::channel::unbounded::<WatchRequest>();
    let (_raw_tx, raw_rx) = smol::channel::unbounded::<PathBuf>();
    let (event_tx, _event_rx) = smol::channel::unbounded::<FileEvent>();
    drop(request_tx);
    smol::block_on(run(FakeWatcher::default(), request_rx, raw_rx, event_tx));
}

#[test]
fn notify_events_are_queued_by_path_and_failures_ignored() {
    let (raw_tx, raw_rx) = smol::channel::unbounded::<PathBuf>();
    let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
        .add_path(PathBuf::from("/tmp/one"))
        .add_path(PathBuf::from("/tmp/two"));
    forward(Ok(event), &raw_tx);
    assert_eq!(raw_rx.try_recv(), Ok(PathBuf::from("/tmp/one")));
    assert_eq!(raw_rx.try_recv(), Ok(PathBuf::from("/tmp/two")));

    // A backend error is not something the editor can act on, and must not stop it.
    forward(Err(notify::Error::generic("backend hiccup")), &raw_tx);
    // ...and an access event is filtered before it ever reaches the queue.
    let opened = notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Any))
        .add_path(PathBuf::from("/tmp/three"));
    forward(Ok(opened), &raw_tx);
    assert!(raw_rx.is_empty());
}

#[test]
fn a_real_watcher_starts() {
    // The one thing the fake cannot tell us: that `notify` will actually stand up
    // its backend here. The loop is dropped without running - this is a smoke test
    // of the constructor, not of the OS.
    let (handle, _loop) = watcher().expect("a backend is available");
    drop(handle);
}

#[test]
fn a_symlink_is_watched_where_its_target_lives() {
    // The dotfiles shape: `~/.vimrc -> ~/dotfiles/vimrc`, where the link and the
    // file it points at are in different directories. A save resolves the link
    // before it writes - so does every other editor - which means the write lands
    // in the *target's* directory. Watching the link's own directory therefore
    // watched a place nothing ever writes, and an external change to a symlinked
    // file was silently never reported.
    let dir = TempDir::new();
    let real_dir = dir.path.join("dotfiles");
    std::fs::create_dir(&real_dir).unwrap();
    let target = real_dir.join("vimrc");
    std::fs::write(&target, "set nocompatible").unwrap();
    let link = dir.path.join(".vimrc");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(not(unix))]
    return;

    let mut set = WatchSet::new();
    let watched_dir = set.watch(&link).expect("watch starts");
    assert_eq!(
        watched_dir,
        real_dir.canonicalize().unwrap(),
        "the target's directory is the one writes land in"
    );

    // An event for the real file resolves to the watched file. The core matches it
    // to the buffer by canonicalized identity, so the link it was opened under and
    // the target it resolves to are the same document.
    let canonical = target.canonicalize().unwrap();
    assert_eq!(set.resolve(&canonical), Some(canonical.clone()));
    // And so does one spelled with the link, since both canonicalize alike.
    assert_eq!(set.resolve(&link), Some(canonical));

    // Releasing it releases the target's directory, not the link's.
    assert_eq!(
        set.unwatch(&link),
        Some(real_dir.canonicalize().unwrap()),
        "the last file leaving stops the watch it started"
    );
}
