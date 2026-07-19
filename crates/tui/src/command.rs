//! A frontend command: what a key binding or a picked palette entry *does* (SPEC
//! §7.5 UI-commit vocabulary).
//!
//! Either a core editor intent forwarded to the actor, or a frontend-local effect
//! (open an overlay) that never crosses the seam. This is the single type the event
//! loop dispatches, whether the command came from the keymap ([`crate::keymap`]) or
//! from a compositor layer committing a choice ([`crate::compositor::Layer`]) - so a
//! bound key and a palette selection run through the exact same path.

use vortex_core::Action;

/// A dispatchable frontend command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Forward a core intent to the editor actor.
    Editor(Action),
    /// Open the file-open prompt overlay (frontend-local).
    OpenFilePrompt,
}
