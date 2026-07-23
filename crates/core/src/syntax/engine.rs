//! Spawning and driving the highlighter (SPEC §3, §5, M4) - the I/O half.
//!
//! The shape mirrors [`crate::lsp::client`]: [`highlighter`] builds the channels
//! and hands back the loop as a `Future` the frontend spawns. Nothing here spawns
//! a task or names an executor. Unlike the LSP client, the engine is a pure
//! in-process parser (no subprocess), so the loop is driven end-to-end in tests
//! with a real grammar - it needs no coverage exemption.

use async_channel::{Receiver, Sender};
use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, Highlighter};

use crate::editor::BoxFuture;
use crate::syntax::highlight::{names, spans_from_events};
use crate::syntax::{SyntaxEvent, SyntaxSync};

/// Why the highlighter could not start. Typed so the frontend can degrade to "no
/// highlights" rather than let the editor die (SPEC §8), exactly like
/// [`crate::lsp::LspError`]: a broken grammar or query must never take the editor
/// with it.
#[derive(Debug, thiserror::Error)]
pub enum SyntaxError {
    /// The highlight/injection/locals query failed to compile against the grammar
    /// (a malformed `.scm`, or one written for a different grammar version).
    #[error("invalid highlight query: {0}")]
    Query(String),
}

/// Channels the editor uses to talk to a running highlighter - the syntax twin of
/// [`crate::lsp::LspHandle`].
pub struct SyntaxHandle {
    /// editor -> highlighter: text to reparse (SPEC §5 full-document sync).
    pub sync: Sender<SyntaxSync>,
    /// highlighter -> editor: highlight batches. Bounded and ordered.
    pub events: Receiver<SyntaxEvent>,
}

/// Sync channel bound: one message per coalesced change; the loop drains to the
/// newest before parsing, so a backlog costs at most one wasted `try_recv` sweep.
const SYNC_CAP: usize = 64;
/// Event channel bound: highlight batches, one per reparse.
const EVENT_CAP: usize = 64;

/// Start a highlighter for one grammar and return its channels plus its loop.
///
/// `language` is the tree-sitter grammar (the frontend loads it, dynamically,
/// from config) and the three query strings are its `.scm` sources; `name` is the
/// grammar's own name, used only in tree-sitter error messages. The returned
/// future runs until the editor drops [`SyntaxHandle::sync`] or a query fails to
/// compile, and resolves to why it stopped. Nothing happens until the frontend
/// polls it.
pub fn highlighter(
    language: Language,
    name: impl Into<String>,
    highlights_query: String,
    injections_query: String,
    locals_query: String,
) -> (SyntaxHandle, BoxFuture<Result<(), SyntaxError>>) {
    let (sync_tx, sync_rx) = async_channel::bounded::<SyntaxSync>(SYNC_CAP);
    let (event_tx, event_rx) = async_channel::bounded::<SyntaxEvent>(EVENT_CAP);
    let name = name.into();
    (
        SyntaxHandle {
            sync: sync_tx,
            events: event_rx,
        },
        Box::pin(run(
            language,
            name,
            highlights_query,
            injections_query,
            locals_query,
            sync_rx,
            event_tx,
        )),
    )
}

#[allow(clippy::too_many_arguments)]
async fn run(
    language: Language,
    name: String,
    highlights_query: String,
    injections_query: String,
    locals_query: String,
    sync: Receiver<SyntaxSync>,
    events: Sender<SyntaxEvent>,
) -> Result<(), SyntaxError> {
    // Build the configuration once: the grammar and queries are fixed for this
    // producer's lifetime, so compilation (the only fallible step) happens up
    // front and a bad query stops the loop before any text is parsed.
    let mut config = HighlightConfiguration::new(
        language,
        name,
        &highlights_query,
        &injections_query,
        &locals_query,
    )
    .map_err(|e| SyntaxError::Query(e.to_string()))?;
    config.configure(&names());

    let mut highlighter = Highlighter::new();
    loop {
        // Park until there is text to parse; a closed sync channel means the
        // editor is gone, which is a clean stop, not an error.
        let Ok(mut msg) = sync.recv().await else {
            return Ok(());
        };
        // Coalesce: if the editor queued newer text while we were parsing, parse
        // only the newest and drop the intermediate versions - each is fully
        // superseded (full-document sync), so parsing them would be wasted work
        // on the exact fast-typing path where it hurts most.
        while let Ok(newer) = sync.try_recv() {
            msg = newer;
        }
        let SyntaxSync { version, text } = msg;

        // A parse or query-execution error is per-batch, not fatal: skip this
        // version and wait for the next text rather than killing the producer
        // (SPEC §8). `injection_callback` returns `None` - language injection
        // (code in doc comments, embedded languages) is deferred (SPEC §14).
        let spans = match highlighter.highlight(&config, text.as_bytes(), None, |_| None) {
            Ok(iter) => match spans_from_events(iter) {
                Ok(spans) => spans,
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        if events
            .send(SyntaxEvent::Highlights { version, spans })
            .await
            .is_err()
        {
            // The editor dropped the event receiver: it has stopped.
            return Ok(());
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
