//! M2's verification: a real language server, a real diagnostic, the right span
//! (SPEC §14).
//!
//! This is the test that validates the SPEC §3 stack assumption end to end -
//! `smol` + `async-lsp` + a genuine `rust-analyzer` subprocess - rather than
//! against a fake on the same channels (which `lib_tests` already covers).
//!
//! **Ignored by default.** It spawns `rust-analyzer`, which is not present on
//! every machine and takes tens of seconds to index even a two-line crate. Run
//! it explicitly:
//!
//! ```sh
//! cargo test --package vortex-core --test lsp_rust_analyzer -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use vortex_core::{Action, Core, CoreHandle, Severity, ViewSnapshot};

/// A fixture whose byte, char and UTF-16 columns all disagree, so only a correct
/// UTF-16 reading lands on the flagged token: the trailing `msg` sits at byte 32,
/// char 23 and UTF-16 unit 24 of line 1. Returning a `&str` where `i32` is
/// declared is the error rust-analyzer reports.
const FIXTURE: &str = "pub fn bad() -> i32 {\n    let msg = \"日本語 😀\"; msg\n}\n";

/// A throwaway cargo project for rust-analyzer to index, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> std::io::Result<Self> {
        let mut root = std::env::temp_dir();
        root.push(format!("vortex-ra-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )?;
        std::fs::write(root.join("src/lib.rs"), FIXTURE)?;
        Ok(Self { root })
    }

    fn file(&self) -> PathBuf {
        self.root.join("src/lib.rs")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Await the first snapshot carrying a decoration, or give up after `timeout`.
/// rust-analyzer publishes an empty batch immediately and the real diagnostics
/// only once it has indexed, so this waits for content rather than the first
/// message.
async fn first_decorated(h: &CoreHandle, timeout: Duration) -> Option<ViewSnapshot> {
    let decorated = async {
        loop {
            let snap = h.snapshots.recv().await.ok()?;
            if !snap.decorations.is_empty() {
                return Some(snap);
            }
        }
    };
    smol::future::or(decorated, async {
        smol::Timer::after(timeout).await;
        None
    })
    .await
}

fn rust_analyzer_available() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
#[ignore = "spawns rust-analyzer; slow and requires it on PATH"]
fn a_real_diagnostic_underlines_the_right_span() {
    assert!(
        rust_analyzer_available(),
        "rust-analyzer must be on PATH: `rustup component add rust-analyzer`"
    );
    let fixture = Fixture::new().expect("create fixture crate");
    let file = fixture.file();

    let ex = smol::Executor::new();
    let (lsp, lsp_loop) = vortex_core::lsp::client("rust-analyzer", &fixture.root);
    let Core { handle, run } = vortex_core::with_lsp(64, lsp);

    ex.spawn(run).detach();
    // The LSP loop's result is the client's failure channel; surface it rather
    // than letting the test time out with no explanation.
    let lsp_task = ex.spawn(lsp_loop);

    let snapshot = smol::block_on(ex.run(async {
        handle
            .actions
            .send(Action::Open(file.clone()))
            .await
            .expect("core alive");
        // Indexing a fresh crate is slow on a cold cargo cache.
        first_decorated(&handle, Duration::from_secs(120)).await
    }));

    let snapshot = snapshot.unwrap_or_else(|| {
        panic!("no diagnostics within the timeout; lsp task finished: {lsp_task:?}")
    });

    let underlines: Vec<_> = snapshot
        .decorations
        .underlines_in(0..snapshot.text.byte_len())
        .collect();
    assert!(
        !underlines.is_empty(),
        "the error should underline something"
    );

    // The milestone's criterion: the squiggle covers exactly the flagged
    // identifier. Reading rust-analyzer's UTF-16 column 24 as a byte or char
    // column would land on "; " or inside the emoji instead.
    let covered: Vec<String> = underlines
        .iter()
        .map(|(range, _)| snapshot.text.slice(range.clone()))
        .collect();
    assert!(
        covered.iter().any(|s| s == "msg"),
        "expected an underline over `msg`, got {covered:?}"
    );
    assert!(
        underlines.iter().any(|(_, sev)| *sev == Severity::Error),
        "the type mismatch is an error"
    );

    // And the gutter is marked on the line the error is on.
    let error_line = snapshot.text.line_of_byte(
        underlines
            .iter()
            .find(|(_, sev)| *sev == Severity::Error)
            .map(|(range, _)| range.start)
            .expect("an error underline"),
    );
    assert!(
        snapshot
            .decorations
            .gutter_mark(&snapshot.text, error_line)
            .is_some(),
        "the error's line should carry a gutter mark"
    );
}

/// A missing language server must degrade to "no diagnostics", never take the
/// editor down (SPEC §8). Cheap and hermetic, so it is not `#[ignore]`d.
#[test]
fn a_missing_language_server_is_an_error_not_a_panic() {
    let ex = smol::Executor::new();
    let (lsp, lsp_loop) = vortex_core::lsp::client(
        "vortex-no-such-language-server",
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );
    let Core { handle, run } = vortex_core::with_lsp(16, lsp);
    ex.spawn(run).detach();

    let (result, text) = smol::block_on(ex.run(async {
        let result = lsp_loop.await;
        // The editor keeps working with the server's channels closed.
        handle
            .actions
            .send(Action::Insert("still editing".into()))
            .await
            .expect("core alive");
        let snap = handle.snapshots.recv().await.expect("a snapshot");
        (result, snap.text.to_string())
    }));

    assert!(
        matches!(result, Err(vortex_core::lsp::LspError::Spawn { .. })),
        "expected a spawn error, got {result:?}"
    );
    assert_eq!(text, "still editing");
}
