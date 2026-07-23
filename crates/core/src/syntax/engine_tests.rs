use std::future::Future;

use async_channel::{Receiver, Sender};
use tree_sitter::Language;

use super::{SyntaxError, SyntaxHandle, highlighter};
use crate::decoration::HighlightKind;
use crate::syntax::{HighlightSpan, SyntaxEvent, SyntaxSync};

fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

/// Spawn a Rust highlighter loop on an executor and run `f` against its channels,
/// exactly as the editor actor would (attach the handle, feed it text, drain
/// events). The loop is a pure in-process parser, so nothing here needs a
/// subprocess - which is why the engine needs no coverage exemption.
fn drive_rust<F, Fut, T>(f: F) -> T
where
    F: FnOnce(Sender<SyntaxSync>, Receiver<SyntaxEvent>) -> Fut,
    Fut: Future<Output = T>,
{
    let ex = smol::Executor::new();
    let (handle, run) = highlighter(
        rust_language(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
        tree_sitter_rust::INJECTIONS_QUERY.to_string(),
        String::new(),
    );
    ex.spawn(async move {
        let _ = run.await;
    })
    .detach();
    smol::block_on(ex.run(f(handle.sync, handle.events)))
}

/// Each span paired with the source text it covers, for offset-independent
/// assertions.
fn covered(source: &str, spans: &[HighlightSpan]) -> Vec<(String, HighlightKind)> {
    spans
        .iter()
        .map(|s| (source[s.range.clone()].to_string(), s.kind))
        .collect()
}

#[test]
fn parses_text_and_emits_highlights_for_its_version() {
    let source = "fn main() {}";
    let (spans, version) = drive_rust(|sync, events| async move {
        sync.send(SyntaxSync {
            version: 7,
            text: source.to_string(),
        })
        .await
        .unwrap();
        let SyntaxEvent::Highlights { version, spans } = events.recv().await.unwrap();
        (spans, version)
    });
    // The batch is tagged with the version it parsed, so the editor can reason
    // about staleness (SPEC §5).
    assert_eq!(version, 7);
    let painted = covered(source, &spans);
    assert!(
        painted.contains(&("fn".to_string(), HighlightKind::Keyword)),
        "expected `fn` keyword, got {painted:?}"
    );
    assert!(
        painted.contains(&("main".to_string(), HighlightKind::Function)),
        "expected `main` function, got {painted:?}"
    );
}

#[test]
fn coalesces_to_the_newest_queued_text() {
    // Two syncs land before the parked loop wakes (both `try_send`s run before the
    // closure first awaits, so the single-threaded executor has not polled the
    // producer yet). The loop must parse only the newest and skip the stale one -
    // so the first event we see is the *second* version, never the first.
    let version = drive_rust(|sync, events| async move {
        sync.try_send(SyntaxSync {
            version: 1,
            text: "fn old() {}".to_string(),
        })
        .unwrap();
        sync.try_send(SyntaxSync {
            version: 2,
            text: "fn new() {}".to_string(),
        })
        .unwrap();
        let SyntaxEvent::Highlights { version, .. } = events.recv().await.unwrap();
        version
    });
    assert_eq!(
        version, 2,
        "the stale v1 parse should have been coalesced away"
    );
}

#[test]
fn an_empty_buffer_highlights_nothing() {
    let spans = drive_rust(|sync, events| async move {
        sync.send(SyntaxSync {
            version: 1,
            text: String::new(),
        })
        .await
        .unwrap();
        let SyntaxEvent::Highlights { spans, .. } = events.recv().await.unwrap();
        spans
    });
    assert!(spans.is_empty());
}

#[test]
fn successive_edits_each_produce_a_fresh_batch() {
    // Draining events one at a time (as the editor loop does) keeps the producer
    // uncoalesced, so each distinct version is parsed and reported in turn.
    let versions = drive_rust(|sync, events| async move {
        let mut seen = Vec::new();
        for (v, text) in [(1u64, "fn a() {}"), (2, "fn ab() {}"), (3, "fn abc() {}")] {
            sync.send(SyntaxSync {
                version: v,
                text: text.to_string(),
            })
            .await
            .unwrap();
            let SyntaxEvent::Highlights { version, .. } = events.recv().await.unwrap();
            seen.push(version);
        }
        seen
    });
    assert_eq!(versions, vec![1, 2, 3]);
}

#[test]
fn dropping_the_editor_stops_the_loop_cleanly() {
    // The editor gone (its sync sender + event receiver dropped) is a clean stop,
    // not an error - the highlighter must never outlive or panic past the editor.
    let (handle, run) = highlighter(
        rust_language(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
        tree_sitter_rust::INJECTIONS_QUERY.to_string(),
        String::new(),
    );
    drop(handle);
    assert!(smol::block_on(run).is_ok());
}

#[test]
fn the_editor_dropping_the_event_channel_stops_the_loop() {
    // The editor gone mid-session (its event receiver dropped) surfaces as a failed
    // send inside the loop, which stops cleanly rather than erroring or spinning.
    let ex = smol::Executor::new();
    let (handle, run) = highlighter(
        rust_language(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
        tree_sitter_rust::INJECTIONS_QUERY.to_string(),
        String::new(),
    );
    let task = ex.spawn(run);
    let result = smol::block_on(ex.run(async move {
        let SyntaxHandle { sync, events } = handle;
        sync.send(SyntaxSync {
            version: 1,
            text: "fn a() {}".to_string(),
        })
        .await
        .unwrap();
        // Confirm the loop is running, then drop the receiver and prod it again: the
        // next batch has nowhere to go, so the loop returns.
        events.recv().await.unwrap();
        drop(events);
        sync.send(SyntaxSync {
            version: 2,
            text: "fn b() {}".to_string(),
        })
        .await
        .unwrap();
        task.await
    }));
    assert!(result.is_ok());
}

#[test]
fn a_malformed_query_stops_the_loop_with_an_error() {
    // A broken `.scm` must surface as a typed error the frontend can swallow
    // (degrade to no highlights), never as a panic or a hang (SPEC §8).
    let (handle, run) = highlighter(
        rust_language(),
        "rust",
        "(this is not a valid query".to_string(),
        String::new(),
        String::new(),
    );
    drop(handle);
    assert!(matches!(smol::block_on(run), Err(SyntaxError::Query(_))));
}
