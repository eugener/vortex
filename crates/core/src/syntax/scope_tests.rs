use tree_sitter::{Language, Parser, Query};

use super::scopes_from_tree;

fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

/// Parse `source` as Rust, run `query` over it, and return each scope as the text
/// it covers - offset-independent assertions, as `engine_tests::covered` does for
/// highlights.
fn scopes(source: &str, query: &str) -> Vec<String> {
    let language = rust_language();
    let query = Query::new(&language, query).expect("test query compiles");
    let mut parser = Parser::new();
    parser.set_language(&language).expect("grammar loads");
    let tree = parser.parse(source, None).expect("source parses");
    scopes_from_tree(&tree, &query, source.as_bytes())
        .into_iter()
        .map(|r| source[r].to_string())
        .collect()
}

const ITEMS: &str = "(impl_item) @context (function_item) @context";

#[test]
fn captures_the_whole_node_outermost_first() {
    let source = "impl Foo {\n    fn bar(&self) {\n        let x = 1;\n    }\n}\n";
    let found = scopes(source, ITEMS);
    assert_eq!(found.len(), 2, "{found:?}");
    // The whole node, not its first line: the range is what the frontend tests
    // the viewport's top row against, and the first line is derived from it.
    assert!(found[0].starts_with("impl Foo {\n"), "{found:?}");
    assert!(found[0].ends_with("}"), "{found:?}");
    // Sorted by start, so the enclosing `impl` precedes the `fn` it contains -
    // which is the order the header rows are painted in.
    assert!(found[1].starts_with("fn bar"), "{found:?}");
}

#[test]
fn a_single_line_scope_is_dropped() {
    // It can never produce a header: once its row is above the viewport, so is
    // all of it, and nothing about it encloses what is on screen.
    let found = scopes("fn one_liner() {}\n", ITEMS);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn a_single_line_scope_inside_a_multi_line_one_is_dropped_alone() {
    // The enclosing `impl` still spans rows, so it stays; only the one-liner goes.
    let source = "impl Foo {\n    fn bar() {}\n}\n";
    let found = scopes(source, ITEMS);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("impl Foo"), "{found:?}");
}

#[test]
fn a_query_without_a_context_capture_finds_nothing() {
    // A `.scm` that captures under other names describes no scopes - it must not
    // silently promote every capture it happens to make.
    let source = "fn bar() {\n    let x = 1;\n}\n";
    let found = scopes(source, "(function_item) @other");
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn helper_captures_alongside_context_are_ignored() {
    // The convention the capture name exists for: a pattern may capture parts to
    // test them, and only `@context` names a pinnable node.
    let source = "fn bar() {\n    let x = 1;\n}\n";
    let found = scopes(source, "(function_item name: (identifier) @_name) @context");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].starts_with("fn bar"), "{found:?}");
}

#[test]
fn duplicate_matches_on_one_node_collapse() {
    // Two patterns capturing the same node must not put two identical rows in the
    // header - the query author should not have to keep the patterns disjoint.
    let source = "fn bar() {\n    let x = 1;\n}\n";
    let found = scopes(
        source,
        "(function_item) @context (function_item body: (block)) @context",
    );
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn siblings_come_out_in_document_order() {
    let source = "fn a() {\n    1;\n}\nfn b() {\n    2;\n}\n";
    let found = scopes(source, ITEMS);
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].starts_with("fn a"), "{found:?}");
    assert!(found[1].starts_with("fn b"), "{found:?}");
}

#[test]
fn nested_scopes_sharing_a_start_put_the_wider_one_first() {
    // An outer node whose first token opens the inner one too (here a `mod` whose
    // body starts at the same byte as nothing else would, so this is built from
    // two captures of ranges that genuinely share a start). Outermost first is
    // what makes the header read from module down to function.
    let source = "mod m {\n    fn a() {\n        1;\n    }\n}\n";
    let found = scopes(source, "(mod_item) @context (declaration_list) @context");
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found[0].starts_with("mod m"), "{found:?}");
    assert!(found[1].starts_with("{"), "{found:?}");
}

#[test]
fn an_empty_source_has_no_scopes() {
    assert!(scopes("", ITEMS).is_empty());
}
