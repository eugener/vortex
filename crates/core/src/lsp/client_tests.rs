use super::*;

fn tmp_file(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("vortex-client-test-{}-{name}", std::process::id()));
    p
}

#[test]
fn an_absent_encoding_is_the_protocol_default_and_is_accepted() {
    // The LSP spec's default is UTF-16, which is exactly what we advertise.
    assert!(check_encoding(None).is_ok());
}

#[test]
fn utf16_is_accepted() {
    assert!(check_encoding(Some(&PositionEncodingKind::UTF16)).is_ok());
}

#[test]
fn a_server_ignoring_the_negotiation_is_refused() {
    // Accepting UTF-8 positions while converting as UTF-16 would put every
    // squiggle in the wrong place *quietly*, which is worse than no diagnostics.
    let err = check_encoding(Some(&PositionEncodingKind::UTF8)).unwrap_err();
    assert!(
        matches!(&err, LspError::Protocol(m) if m.contains("position encoding")),
        "unexpected error: {err}"
    );
}

#[test]
fn opening_a_document_records_it_and_carries_its_text() {
    let mut opened = Vec::new();
    let path = tmp_file("a.rs");
    let out = outgoing(
        &mut opened,
        DocumentSync::Opened {
            path: path.clone(),
            language_id: "rust".into(),
            text: "fn main() {}".into(),
        },
    );
    let Some(Outgoing::Open(params)) = out else {
        panic!("expected a didOpen");
    };
    assert_eq!(params.text_document.language_id, "rust");
    assert_eq!(params.text_document.text, "fn main() {}");
    assert_eq!(opened, vec![path]);
}

#[test]
fn a_change_before_the_open_is_dropped() {
    // Sending didChange for a document the server has never seen is a protocol
    // error on its side, so it must not leave the client at all.
    let mut opened = Vec::new();
    let out = outgoing(
        &mut opened,
        DocumentSync::Changed {
            path: tmp_file("a.rs"),
            version: 1,
            text: "x".into(),
        },
    );
    assert!(out.is_none());
}

#[test]
fn a_change_after_the_open_carries_the_whole_document_and_its_version() {
    let mut opened = Vec::new();
    let path = tmp_file("a.rs");
    outgoing(
        &mut opened,
        DocumentSync::Opened {
            path: path.clone(),
            language_id: "rust".into(),
            text: "one".into(),
        },
    );
    let out = outgoing(
        &mut opened,
        DocumentSync::Changed {
            path,
            version: 7,
            text: "two".into(),
        },
    );
    let Some(Outgoing::Change(params)) = out else {
        panic!("expected a didChange");
    };
    assert_eq!(params.text_document.version, 7);
    assert_eq!(params.content_changes.len(), 1);
    // Full-document sync: no range means "replace everything" (SPEC §5).
    assert!(params.content_changes[0].range.is_none());
    assert_eq!(params.content_changes[0].text, "two");
}

#[test]
fn reopening_the_same_document_does_not_duplicate_the_record() {
    let mut opened = Vec::new();
    let path = tmp_file("a.rs");
    for _ in 0..3 {
        outgoing(
            &mut opened,
            DocumentSync::Opened {
                path: path.clone(),
                language_id: "rust".into(),
                text: String::new(),
            },
        );
    }
    assert_eq!(opened.len(), 1);
}

#[test]
fn a_path_that_is_not_a_file_url_is_dropped() {
    // A relative path has no file URL; an unnamed buffer must not panic here.
    let mut opened = Vec::new();
    assert!(
        outgoing(
            &mut opened,
            DocumentSync::Opened {
                path: PathBuf::from("relative/not/absolute.rs"),
                language_id: "rust".into(),
                text: String::new(),
            },
        )
        .is_none()
    );
    assert!(opened.is_empty());
}

#[test]
fn initialize_advertises_utf16_only() {
    // The single-conversion-path decision (module docs) depends on never telling
    // a server we can read UTF-8 positions.
    let root = Url::from_file_path(std::env::temp_dir()).unwrap();
    let params = initialize_params(root.clone());
    let encodings = params
        .capabilities
        .general
        .and_then(|g| g.position_encodings)
        .expect("position encodings are advertised");
    assert_eq!(encodings, vec![PositionEncodingKind::UTF16]);
    assert_eq!(params.workspace_folders.unwrap()[0].uri, root);
}

#[test]
fn a_bad_workspace_root_is_reported_rather_than_panicking() {
    // `client` accepts any path; a relative one cannot become a file URL and must
    // surface as an error from the loop (SPEC §8).
    let (_handle, loop_) = client("true", Path::new("relative/root"));
    let result = smol::block_on(loop_);
    assert!(
        matches!(result, Err(LspError::BadRoot(_))),
        "expected BadRoot, got {result:?}"
    );
}
