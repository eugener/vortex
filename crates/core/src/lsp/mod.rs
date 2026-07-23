//! The LSP client (SPEC §3, §4, M2).
//!
//! **The position-space boundary lives here.** LSP speaks line + UTF-16 code
//! unit; the core speaks byte offsets. SPEC §4 requires that conversion happen
//! "once, in one place" because getting it wrong is the notorious "diagnostic
//! underline is one column off" bug - that one place is [`convert`].
//!
//! **The core still never spawns itself.** [`client`] returns the client's loop
//! as a `Future` for the frontend to spawn on whatever executor it owns, exactly
//! like [`crate::editor::new`]. `async-process` and `async-lsp` are used inside
//! this module but appear in no public signature, so `vortex-core` stays
//! executor-agnostic.
//!
//! **Only UTF-16 is negotiated.** The LSP spec lets a server advertise UTF-8
//! positions, and SPEC §3 suggests preferring it - but supporting both means a
//! second conversion path that no test would exercise on a server that picks
//! UTF-16 anyway. So the client advertises UTF-16 only (the protocol default,
//! which every server must support) and there is exactly one path. Preferring
//! UTF-8 is a later optimization, not a correctness gap.

pub(crate) mod client;
pub(crate) mod convert;

use std::path::PathBuf;

use crate::buffer::Utf16Position;
use crate::decoration::Severity;

pub use client::{LspError, LspHandle, client};

/// The language-server loop the frontend spawns, resolving to why it stopped
/// (SPEC §8). A named alias so the frontend does not spell out the boxed-future
/// type - and so the core still names no executor.
pub type BoxLspLoop = crate::editor::BoxFuture<Result<(), LspError>>;

/// One diagnostic, in the core's own vocabulary rather than `lsp_types`'.
///
/// Deliberately not `lsp_types::Diagnostic`: keeping the wire type inside this
/// module means the editor actor - and its tests - never link the LSP types, and
/// a future non-LSP diagnostic producer (a linter, a compiler wrapper) feeds the
/// same struct. Positions stay in [`Utf16Position`] because that is the space the
/// server computed them in; they become byte offsets only against the buffer
/// text, in [`convert::decorations_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The flagged span, `start..end`, in the server's UTF-16 position space.
    pub start: Utf16Position,
    pub end: Utf16Position,
    pub severity: Severity,
    pub message: String,
}

/// What the language server tells the editor. Ordered, bounded channel: a
/// dropped diagnostic batch would leave stale squiggles on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LspEvent {
    /// The server published a fresh, complete diagnostic set for `path`. LSP
    /// defines this as a full replacement, including the empty list meaning
    /// "this file is clean now".
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
    },
}

/// What the editor tells the language server about a document's lifetime.
///
/// Text sync is **full-document**: every change ships the whole buffer. LSP also
/// allows incremental sync, and the core already produces exactly the deltas it
/// would need (SPEC §5) - but incremental sync requires the client and server to
/// agree on a version-by-version edit history, and a desync silently corrupts
/// every position the server returns afterwards. Full sync cannot desync. The
/// cost is re-sending the buffer per coalesced change, which is bounded by file
/// size and off the keystroke path; switching to incremental is an optimization
/// to make against a benchmark, not a default to assume.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DocumentSync {
    Opened {
        path: PathBuf,
        language_id: String,
        text: String,
    },
    Changed {
        path: PathBuf,
        /// The buffer version this text is (SPEC §5). The server echoes it back
        /// on diagnostics so a stale batch is recognizable.
        version: u64,
        text: String,
    },
}
