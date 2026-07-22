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
    /// Open the command palette overlay (frontend-local).
    OpenPalette,
    /// Open the file picker overlay (frontend-local).
    OpenFilePicker,
    /// Open the theme picker overlay (frontend-local).
    OpenThemePicker,
    /// Switch to the named theme (frontend-local: chrome never crosses the seam).
    ///
    /// Carries data, so unlike the openers above it is not a bindable
    /// [`crate::keymap::Command`] - it is only ever emitted by the theme picker,
    /// which is where the names come from.
    SetTheme(String),
}
