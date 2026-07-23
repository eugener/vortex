//! `vortex-core` - the headless editor core.
//!
//! The core owns buffer state, selections, undo, syntax, and LSP; frontends talk
//! to it only by message (SPEC §1). This crate MUST NOT depend on any terminal
//! crate - that boundary is enforced by its `Cargo.toml` and is what lets other
//! frontends (GUI, web, remote) attach later without touching core logic.

pub mod action;
mod anchor;
pub mod buffer;
pub mod decoration;
pub mod editor;
mod history;
pub mod lsp;
pub mod selection;
pub mod view;

pub use action::Action;
pub use buffer::{Buffer, ByteRange, EditError, Position, RopeBuffer, Text, Utf16Position};
pub use decoration::{Decoration, DecorationSet, DecorationSource, GutterKind, Severity};
pub use editor::{Core, CoreHandle, new, with_lsp};
pub use lsp::{Diagnostic, DocumentSync, LspEvent, LspHandle};
pub use selection::{Motion, Selection, SelectionSet};
pub use view::{BufferId, Delta, Notification, ViewSnapshot};

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
