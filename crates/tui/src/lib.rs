//! `vortex-tui` library - the frontend's testable logic, split out from the
//! binary so it can be unit-tested, benchmarked, and (later) reused by another
//! frontend crate.
//!
//! The architecture (SPEC §1, §7) is a thin terminal frontend over a headless
//! core. "Thin" is a claim about `main.rs`, not about the frontend as a whole:
//! key→intent translation ([`keymap`]), viewport and display-column math
//! ([`layout`]), the overlay compositor ([`compositor`], [`picker`]), theme
//! loading ([`config`], [`theme`]), and the file/theme pickers are all pure logic
//! with no terminal I/O, so they live here and are covered by tests. `main.rs`
//! keeps only the genuinely untestable I/O shell - raw-mode setup, the
//! `event::read` loop, and the ratatui draw call - and depends on this crate.
//!
//! Keeping these modules in a library (rather than `mod` declarations inside the
//! binary) is also what lets `benches/` link them: a `[[bench]]` can only depend
//! on a library target, so the display-column hot paths in [`layout`] are
//! benchmarkable precisely because they live here.

pub mod bufferpicker;
pub mod command;
pub mod compositor;
pub mod config;
pub mod filepicker;
pub mod grammar;
pub mod keymap;
pub mod layout;
pub mod osc52;
pub mod palette;
pub mod picker;
pub mod prompt;
pub mod theme;
pub mod themepicker;
pub mod toast;
pub mod watcher;

// Shared unit-test helpers. Compiled only under `cfg(test)`, so nothing test-only
// ships in a release build; the binary keeps its own `#[cfg(test)] mod testutil`
// over the same file for its tests, since a dependency's `cfg(test)` items are
// invisible to the crates that depend on it.
#[cfg(test)]
pub mod testutil;
