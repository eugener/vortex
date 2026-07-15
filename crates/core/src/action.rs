//! `Action` - intent sent from a frontend into the core (SPEC §1, §12.2).
//!
//! Actions model *intent* (`Quit`), never keystrokes (`CtrlC`). Key->intent
//! translation is the frontend's job, so a future GUI with different keys emits
//! the same actions. M0 defines only the minimum needed to prove the seam; the
//! full vocabulary (motion/edit/selection/history/file) lands from M1 on.

/// A single intent from a frontend to the core.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Request an immediate `ViewSnapshot` without changing state.
    /// Used by M0 to prove the round-trip; harmless to keep afterwards.
    RequestSnapshot,
    /// Shut the editor down cleanly. The core drains and stops its loop.
    Quit,
}
