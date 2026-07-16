//! `vortex-core` - the headless editor core.
//!
//! The core owns buffer state, selections, undo, syntax, and LSP; frontends talk
//! to it only by message (SPEC §1). This crate MUST NOT depend on any terminal
//! crate - that boundary is enforced by its `Cargo.toml` and is what lets other
//! frontends (GUI, web, remote) attach later without touching core logic.

pub mod action;
pub mod buffer;
pub mod editor;
mod history;
pub mod selection;
pub mod view;

pub use action::Action;
pub use buffer::{Buffer, ByteRange, EditError, Position, RopeBuffer, Text};
pub use editor::{Core, CoreHandle, new};
pub use selection::{Motion, Selection, SelectionSet};
pub use view::{BufferId, Delta, Notification, ViewSnapshot};

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
