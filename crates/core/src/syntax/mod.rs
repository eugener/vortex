//! The syntax highlighter (SPEC §3, §5, M4).
//!
//! **A second decoration producer, the shape of the first.** M2 built the LSP
//! client as a producer that takes buffer text in and emits decorations out over
//! channels, spawned by the frontend on its own executor; this is the same shape
//! with a different engine. [`highlighter`] returns a [`SyntaxHandle`] plus a loop
//! `Future` the frontend spawns (mirroring [`crate::editor::new`] and
//! [`crate::lsp::client`]), so `vortex-core` still names no executor and the parse
//! runs off the editor actor - never on the keystroke path.
//!
//! **The grammar is injected, not bundled.** [`highlighter`] takes a
//! `tree_sitter::Language` and the query strings as parameters, exactly as
//! [`crate::lsp::client`] takes a server *command* rather than hard-coding one.
//! The frontend owns the file-type -> grammar mapping and loads grammars at
//! runtime (dynamically, from config), so adding a language never touches the
//! core. tree-sitter's own types appear in no other public signature: the loop
//! emits the core's semantic [`HighlightKind`], which the theme maps to a color
//! (SPEC §5), keeping styling frontend-owned.
//!
//! **Full reparse per snapshot, not incremental.** This is the same call M2 made
//! for LSP `didChange` (see [`crate::lsp::DocumentSync`]): tree-sitter *can* parse
//! incrementally from the previous tree plus `InputEdit`s, but that requires the
//! producer and editor to agree on a version-by-version edit history, and one
//! desync silently mis-colors every span after it. A full reparse cannot desync,
//! and tree-sitter parses a typical file in well under a frame off the keystroke
//! path. Incremental parsing is an optimization to make against a benchmark, not
//! a default to assume - deferred with the interval index the decoration channel
//! also wants (SPEC §14).

pub(crate) mod engine;
pub(crate) mod highlight;

use crate::buffer::ByteRange;
use crate::decoration::HighlightKind;

pub use engine::{SyntaxError, SyntaxHandle, highlighter};

/// The loop the frontend spawns, resolving to why it stopped (SPEC §8). A named
/// alias so the frontend does not spell out the boxed-future type, matching
/// [`crate::lsp::BoxLspLoop`].
pub type BoxSyntaxLoop = crate::editor::BoxFuture<Result<(), SyntaxError>>;

/// editor -> highlighter: the buffer text to reparse and the version it is
/// (SPEC §5). Full-document, for the "cannot desync" reason in the module doc;
/// the version rides along so a highlight batch is recognizable as the version it
/// was computed against, the same role it plays for LSP `didChange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxSync {
    pub version: u64,
    pub text: String,
}

/// One highlighted span, in the core's own vocabulary rather than tree-sitter's:
/// a byte range plus the semantic [`HighlightKind`] the theme colors. Byte
/// offsets, resolved by the producer against the text it parsed - they become a
/// [`Decoration::Highlight`](crate::decoration::Decoration::Highlight) unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub range: ByteRange,
    pub kind: HighlightKind,
}

/// highlighter -> editor: a fresh, complete highlight set for a version. Like LSP
/// `publishDiagnostics`, this is a full replacement of the syntax bucket, not a
/// delta (SPEC §5); an empty list means "nothing highlights" (e.g. an empty
/// buffer), which clears the bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyntaxEvent {
    Highlights {
        version: u64,
        spans: Vec<HighlightSpan>,
    },
}
