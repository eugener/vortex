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
pub mod file;
mod history;
pub mod lsp;
pub mod selection;
pub mod syntax;
pub mod view;
pub mod watch;

pub use action::{Action, CoreOptions, Granularity};
pub use buffer::{Buffer, ByteRange, EditError, Position, RopeBuffer, Text, Utf16Position};
pub use decoration::{
    Decoration, DecorationSet, DecorationSource, GutterKind, HighlightKind, Severity,
};
pub use editor::{Core, CoreHandle, new, with_lsp};
pub use file::{FileFormat, LineEnding};
pub use lsp::{Diagnostic, DocumentSync, LspEvent, LspHandle};
pub use selection::{Motion, Selection, SelectionSet};
pub use syntax::{HighlightSpan, SyntaxEvent, SyntaxHandle, SyntaxSync, highlighter};
pub use view::{BufferId, BufferInfo, Delta, Notification, ViewSnapshot};
pub use watch::{FileEvent, WatchHandle, WatchRequest};

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
