use super::*;

/// Serializes tests that set `$VORTEX_RUNTIME`: the environment is process-global,
/// so two env-mutating tests running at once would see each other's value.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A temp dir removed on drop, for filesystem-resolution tests.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("vortex-grammar-{tag}-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `f` with `$VORTEX_RUNTIME` set to `runtime`, restoring the prior value
/// (usually unset) afterward. Holds [`ENV_LOCK`] for the duration.
fn with_runtime<T>(runtime: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("VORTEX_RUNTIME");
    // SAFETY: single-threaded within the lock; no other thread reads the env here.
    unsafe { std::env::set_var("VORTEX_RUNTIME", runtime) };
    let out = f();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("VORTEX_RUNTIME", v),
            None => std::env::remove_var("VORTEX_RUNTIME"),
        }
    }
    out
}

/// Lay out a runtime dir with a (fake) grammar library and the given query files.
fn make_runtime(dir: &Path, lang: &str, highlights: Option<&str>, injections: Option<&str>) {
    let grammars = dir.join("grammars");
    std::fs::create_dir_all(&grammars).unwrap();
    std::fs::write(grammars.join(grammar_lib_name(lang)), b"not a real dylib").unwrap();
    if let Some(h) = highlights {
        let qdir = dir.join("queries").join(lang);
        std::fs::create_dir_all(&qdir).unwrap();
        std::fs::write(qdir.join("highlights.scm"), h).unwrap();
        if let Some(i) = injections {
            std::fs::write(qdir.join("injections.scm"), i).unwrap();
        }
    }
}

#[test]
fn grammar_target_maps_rust_and_declines_others() {
    assert_eq!(grammar_target(Path::new("main.rs")), Some("rust"));
    assert_eq!(grammar_target(Path::new("src/lib.rs")), Some("rust"));
    // A file type with no grammar highlights nothing, rather than guessing.
    assert_eq!(grammar_target(Path::new("notes.txt")), None);
    assert_eq!(grammar_target(Path::new("Makefile")), None);
    assert_eq!(grammar_target(Path::new("data.json")), None);
}

#[test]
fn grammar_lib_name_uses_the_platform_convention() {
    // The crate `grammar-rust` builds to this artifact next to the binary.
    let name = grammar_lib_name("rust");
    if cfg!(target_os = "macos") {
        assert_eq!(name, "libgrammar_rust.dylib");
    } else if cfg!(target_os = "windows") {
        assert_eq!(name, "grammar_rust.dll");
    } else {
        assert_eq!(name, "libgrammar_rust.so");
    }
}

#[test]
fn find_grammar_lib_returns_the_first_directory_that_has_it() {
    let empty = TempDir::new("empty");
    let has = TempDir::new("has");
    let name = grammar_lib_name("rust");
    std::fs::write(has.0.join(&name), b"not a real dylib").unwrap();

    let dirs = vec![empty.0.clone(), has.0.clone()];
    assert_eq!(find_grammar_lib("rust", &dirs), Some(has.0.join(&name)));

    // No directory has it -> None (the caller degrades to no highlighting).
    assert_eq!(
        find_grammar_lib("rust", std::slice::from_ref(&empty.0)),
        None
    );
    // An empty search list is not a match.
    assert_eq!(find_grammar_lib("rust", &[]), None);
}

#[test]
fn read_queries_requires_highlights_but_not_injections() {
    let runtime = TempDir::new("rt");
    let qdir = runtime.0.join("queries").join("rust");
    std::fs::create_dir_all(&qdir).unwrap();

    // Missing highlights.scm: no queries at all.
    assert_eq!(read_queries(&runtime.0, "rust"), None);

    // highlights.scm alone: injections default to empty.
    std::fs::write(qdir.join("highlights.scm"), "(identifier) @variable").unwrap();
    let (highlights, injections) = read_queries(&runtime.0, "rust").unwrap();
    assert_eq!(highlights, "(identifier) @variable");
    assert_eq!(injections, "");

    // Both present: both are read.
    std::fs::write(qdir.join("injections.scm"), "; injections").unwrap();
    let (_, injections) = read_queries(&runtime.0, "rust").unwrap();
    assert_eq!(injections, "; injections");
}

#[test]
fn read_queries_declines_a_language_with_no_query_dir() {
    let runtime = TempDir::new("noqueries");
    std::fs::create_dir_all(runtime.0.join("queries")).unwrap();
    assert_eq!(read_queries(&runtime.0, "python"), None);
}

#[test]
fn resolve_finds_the_library_and_queries_under_the_runtime_env() {
    let rt = TempDir::new("resolve-ok");
    make_runtime(&rt.0, "rust", Some("(identifier) @variable"), Some("; inj"));
    let resolved = with_runtime(&rt.0, || resolve("rust")).expect("resolves");
    assert_eq!(
        resolved.lib_path,
        rt.0.join("grammars").join(grammar_lib_name("rust"))
    );
    assert_eq!(resolved.highlights, "(identifier) @variable");
    assert_eq!(resolved.injections, "; inj");
}

#[test]
fn resolve_is_none_when_queries_are_missing() {
    // Library present but no queries: nothing to compile.
    let rt = TempDir::new("resolve-noq");
    make_runtime(&rt.0, "rust", None, None);
    assert!(with_runtime(&rt.0, || resolve("rust")).is_none());
}
