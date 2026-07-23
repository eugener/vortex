//! A dynamically-loadable Rust grammar (M4).
//!
//! Every grammar dylib the editor loads exports one uniform C entry point,
//! `vortex_grammar`, returning the raw `TSLanguage` pointer. The frontend
//! `dlopen`s the file, resolves this one symbol regardless of language, and wraps
//! the pointer back into a `tree_sitter::Language` (see the TUI's grammar loader).
//! A uniform name is what lets the loader be language-agnostic: the config maps a
//! file type to a dylib path, nothing more.
//!
//! Referencing `tree_sitter_rust::LANGUAGE` pulls the grammar's compiled C parser
//! into this dylib; the `#[no_mangle]` wrapper guarantees the symbol is exported
//! (a bare re-export of the C symbol is dropped by the linker as unused).

/// Return the raw tree-sitter language pointer for this grammar.
///
/// # Safety
/// The returned pointer is a `'static` `TSLanguage` owned by this library's
/// image; it stays valid for as long as the library is loaded. The caller must
/// keep the library loaded while any parser using the language is alive.
#[unsafe(no_mangle)]
pub extern "C" fn vortex_grammar() -> *const core::ffi::c_void {
    let raw = tree_sitter_rust::LANGUAGE.into_raw();
    // SAFETY: `into_raw` yields the grammar's own `extern "C"` entry point, whose
    // sole contract is to return its static language pointer; calling it has no
    // preconditions.
    unsafe { raw() as *const core::ffi::c_void }
}
