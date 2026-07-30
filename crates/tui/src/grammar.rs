//! Grammar resolution for syntax highlighting (M4) - the pure, filesystem half.
//!
//! Where a file's language, its grammar library, and its highlight queries live -
//! all decided here as plain functions of the environment and filesystem, so the
//! whole resolution is testable without loading a library or spawning a thread.
//! The `dlopen`-and-attach glue that consumes a [`Resolved`] lives in `main.rs`
//! next to the LSP attach path, the editor's other producer of untestable I/O.
//!
//! **Grammars are runtime data, not a build dependency.** Each is a `cdylib` (the
//! `grammar-*` crates) loaded at runtime from the runtime directory, exporting the
//! uniform `vortex_grammar` entry point. Adding a language is a new grammar crate
//! plus a row in [`grammar_target`] - never a change to the core. This is the
//! "genuinely dynamic" contract (SPEC §3, §14).

use std::path::{Path, PathBuf};

/// The language name for a file, or `None` if its type has no grammar. The one
/// place a file extension maps to a grammar, mirroring `main.rs`'s `lsp_target`.
pub fn grammar_target(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        _ => None,
    }
}

/// A grammar resolved for a language: the library to `dlopen` and the query
/// sources to compile. Everything `main.rs` needs to attach a highlighter, with no
/// I/O left to do beyond loading the library itself.
pub struct Resolved {
    pub lib_path: PathBuf,
    pub queries: Queries,
}

/// A language's `.scm` sources. A struct rather than the tuple of same-typed
/// `String`s this was, for the reason `vortex_core::syntax`'s own `Queries` is one:
/// three positional strings are three things a caller can silently swap.
#[derive(Debug, PartialEq, Eq)]
pub struct Queries {
    pub highlights: String,
    pub injections: String,
    /// The structural scopes the sticky context header pins from (SPEC §7.5, M8),
    /// empty for a grammar shipping no `context.scm`. Empty is what tells the core
    /// to skip the scope parse entirely, so a language that has not opted in pays
    /// nothing for the feature.
    pub context: String,
}

/// Resolve `lang`'s grammar library and queries from the environment, or `None` if
/// either is missing. The single entry point `main.rs` calls; it composes the
/// directory discovery, library lookup, and query reading below.
pub fn resolve(lang: &str) -> Option<Resolved> {
    let lib_path = find_grammar_lib(lang, &grammar_dirs())?;
    let queries = read_queries(&runtime_dir()?, lang)?;
    Some(Resolved { lib_path, queries })
}

/// The platform library file name for a grammar. A grammar crate named
/// `grammar-<lang>` builds to `lib grammar_<lang>` with the platform's dylib
/// prefix/extension, so `rust` resolves to `libgrammar_rust.dylib` on macOS.
fn grammar_lib_name(lang: &str) -> String {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    format!("{prefix}grammar_{lang}.{ext}")
}

/// The grammar library for `lang` among `dirs`, first match wins.
fn find_grammar_lib(lang: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let name = grammar_lib_name(lang);
    dirs.iter()
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

/// The queries for `lang` under a runtime directory: `highlights.scm` (required -
/// `None` if absent) plus `injections.scm` and `context.scm` (optional - empty if
/// absent). A missing optional query is a language that does not have that
/// feature, never an error: the core reads an empty one as "nothing to do".
fn read_queries(runtime: &Path, lang: &str) -> Option<Queries> {
    let dir = runtime.join("queries").join(lang);
    Some(Queries {
        highlights: std::fs::read_to_string(dir.join("highlights.scm")).ok()?,
        injections: std::fs::read_to_string(dir.join("injections.scm")).unwrap_or_default(),
        context: std::fs::read_to_string(dir.join("context.scm")).unwrap_or_default(),
    })
}

/// Directories to search for a grammar library, best first: an explicit
/// `$VORTEX_RUNTIME/grammars`, then beside the running executable - where
/// `cargo build` places the `grammar-*` cdylibs next to `vortex`, so a plain build
/// then run highlights with no install step.
fn grammar_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(rt) = std::env::var("VORTEX_RUNTIME") {
        dirs.push(PathBuf::from(rt).join("grammars"));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.to_path_buf());
    }
    dirs
}

/// The runtime directory holding `queries/`. An explicit `$VORTEX_RUNTIME` wins;
/// otherwise a `runtime/` directory is discovered by walking up from the current
/// directory and the executable's directory (finding a repo checkout in dev, or an
/// install layout in production). `None` if none is found.
fn runtime_dir() -> Option<PathBuf> {
    if let Ok(rt) = std::env::var("VORTEX_RUNTIME") {
        return Some(PathBuf::from(rt));
    }
    let mut starts = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        starts.push(parent.to_path_buf());
    }
    starts
        .iter()
        .flat_map(|start| start.ancestors())
        .map(|ancestor| ancestor.join("runtime"))
        .find(|candidate| candidate.join("queries").is_dir())
}

#[cfg(test)]
#[path = "grammar_tests.rs"]
mod tests;
