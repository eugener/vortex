use std::future::Future;

use async_channel::{Receiver, Sender};
use tree_sitter::Language;

use crate::view::BufferId;

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
    drive_rust_with(String::new(), f)
}

/// [`drive_rust`] with a context query, so the scope half of the producer (M8)
/// runs too. An empty one is the no-sticky-context path every other test takes.
fn drive_rust_with<F, Fut, T>(context: String, f: F) -> T
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
        context,
    );
    ex.spawn(async move {
        let _ = run.await;
    })
    .detach();
    smol::block_on(ex.run(f(handle.sync, handle.events)))
}

/// The version and spans of a highlight batch. A function rather than an
/// irrefutable `let`, because the producer has two event variants now (M8) and a
/// test that asked for highlights must fail loudly if it is handed scopes.
fn highlights(event: SyntaxEvent) -> (u64, Vec<HighlightSpan>) {
    match event {
        SyntaxEvent::Highlights { version, spans, .. } => (version, spans),
        other => panic!("expected a highlight batch, got {other:?}"),
    }
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
            buffer_id: BufferId(0),
            version: 7,
            text: source.into(),
        })
        .await
        .unwrap();
        let (version, spans) = highlights(events.recv().await.unwrap());
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
            buffer_id: BufferId(0),
            version: 1,
            text: "fn old() {}".into(),
        })
        .unwrap();
        sync.try_send(SyntaxSync {
            buffer_id: BufferId(0),
            version: 2,
            text: "fn new() {}".into(),
        })
        .unwrap();
        let (version, _) = highlights(events.recv().await.unwrap());
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
            buffer_id: BufferId(0),
            version: 1,
            text: "".into(),
        })
        .await
        .unwrap();
        let (_, spans) = highlights(events.recv().await.unwrap());
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
                buffer_id: BufferId(0),
                version: v,
                text: text.into(),
            })
            .await
            .unwrap();
            let (version, _) = highlights(events.recv().await.unwrap());
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
        String::new(),
    );
    let task = ex.spawn(run);
    let result = smol::block_on(ex.run(async move {
        let SyntaxHandle { sync, events } = handle;
        sync.send(SyntaxSync {
            buffer_id: BufferId(0),
            version: 1,
            text: "fn a() {}".into(),
        })
        .await
        .unwrap();
        // Confirm the loop is running, then drop the receiver and prod it again: the
        // next batch has nowhere to go, so the loop returns.
        events.recv().await.unwrap();
        drop(events);
        sync.send(SyntaxSync {
            buffer_id: BufferId(0),
            version: 2,
            text: "fn b() {}".into(),
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
        String::new(),
    );
    drop(handle);
    assert!(matches!(smol::block_on(run), Err(SyntaxError::Query(_))));
}

/// A context query for the scope tests below: the two Rust items a header would
/// ever pin in this fixture.
const CONTEXT: &str = "(impl_item) @context (function_item) @context";

#[test]
fn a_context_query_emits_scopes_for_the_version_it_parsed() {
    let source = "impl Foo {\n    fn bar(&self) {\n        let x = 1;\n    }\n}\n";
    let (version, spans) = drive_rust_with(CONTEXT.to_string(), |sync, events| async move {
        sync.send(SyntaxSync {
            buffer_id: BufferId(0),
            version: 4,
            text: source.into(),
        })
        .await
        .unwrap();
        // Highlights come first, scopes second - the order the loop sends them,
        // because the colors are what a first paint waits on (M8).
        highlights(events.recv().await.unwrap());
        match events.recv().await.unwrap() {
            SyntaxEvent::Scopes { version, spans, .. } => (version, spans),
            other => panic!("expected a scope batch, got {other:?}"),
        }
    });
    assert_eq!(version, 4, "a scope batch is tagged like a highlight batch");
    let covered: Vec<&str> = spans.iter().map(|r| &source[r.clone()]).collect();
    assert_eq!(
        covered.len(),
        2,
        "impl and fn, outermost first: {covered:?}"
    );
    assert!(covered[0].starts_with("impl Foo"), "got {covered:?}");
    assert!(covered[1].starts_with("fn bar"), "got {covered:?}");
}

#[test]
fn without_a_context_query_the_producer_sends_only_highlights() {
    // The no-opt-in path: a grammar shipping no `context.scm` must not pay the
    // second parse, and the way that is observable from outside is that nothing
    // but highlights ever arrives.
    let versions = drive_rust(|sync, events| async move {
        let mut seen = Vec::new();
        for v in [1u64, 2] {
            sync.send(SyntaxSync {
                buffer_id: BufferId(0),
                version: v,
                text: "impl Foo {\n    fn bar() {}\n}\n".into(),
            })
            .await
            .unwrap();
            seen.push(highlights(events.recv().await.unwrap()).0);
        }
        seen
    });
    assert_eq!(
        versions,
        vec![1, 2],
        "a Scopes event would have been seen here"
    );
}

#[test]
fn a_malformed_context_query_stops_the_loop_with_an_error() {
    // Same contract as a broken highlight query (SPEC §8): typed, so the frontend
    // degrades to no sticky context rather than losing the editor.
    let (handle, run) = highlighter(
        rust_language(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
        String::new(),
        String::new(),
        "(this is not a valid query".to_string(),
    );
    drop(handle);
    assert!(matches!(smol::block_on(run), Err(SyntaxError::Query(_))));
}
