//! Spawning and driving a language server (SPEC §3, M2).
//!
//! The shape mirrors [`crate::editor::new`]: [`client`] builds the channels and
//! hands back the loop as a `Future` the frontend spawns. Nothing here spawns a
//! task or names an executor, so `vortex-core` remains executor-agnostic even
//! though the implementation sits on the smol stack.

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_channel::{Receiver, Sender};
use async_lsp::lsp_types::notification::{PublishDiagnostics, ShowMessage};
use async_lsp::lsp_types::{
    ClientCapabilities, DidChangeTextDocumentParams, DidOpenTextDocumentParams, InitializeParams,
    InitializedParams, PositionEncodingKind, TextDocumentContentChangeEvent, TextDocumentItem,
    TextDocumentSyncClientCapabilities, Url, VersionedTextDocumentIdentifier, WorkspaceFolder,
};
use async_lsp::router::Router;
use async_lsp::{LanguageServer, MainLoop};
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use tower::ServiceBuilder;

use crate::editor::BoxFuture;
use crate::lsp::{Diagnostic, DocumentSync, LspEvent, convert};

/// Why the LSP client could not start or keep running. Typed so the frontend can
/// surface it as a toast rather than the editor dying (SPEC §8): a missing
/// language server must degrade to "no diagnostics", never to "no editor".
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("could not start language server `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("language server protocol error: {0}")]
    Protocol(String),
    #[error("workspace root {0} is not a valid file URL")]
    BadRoot(PathBuf),
}

/// Channels the editor uses to talk to a running language server.
pub struct LspHandle {
    /// editor -> server: document lifecycle (SPEC §5 full-text sync).
    pub sync: Sender<DocumentSync>,
    /// server -> editor: diagnostics. Bounded and ordered - dropping a batch
    /// would strand stale squiggles on screen.
    pub events: Receiver<LspEvent>,
}

/// Document sync channel bound: one message per coalesced change.
const SYNC_CAP: usize = 64;
/// Event channel bound: diagnostics arrive in bursts while a server indexes.
const EVENT_CAP: usize = 64;

/// Start a language server and return the channels plus its loop.
///
/// `command` is the server executable (e.g. `rust-analyzer`); `root` is the
/// workspace folder it should analyze. The returned future runs until the editor
/// drops [`LspHandle::sync`] or the server exits, and resolves to the reason it
/// stopped. Nothing happens until the frontend polls it.
pub fn client(command: &str, root: &Path) -> (LspHandle, BoxFuture<Result<(), LspError>>) {
    let (sync_tx, sync_rx) = async_channel::bounded::<DocumentSync>(SYNC_CAP);
    let (event_tx, event_rx) = async_channel::bounded::<LspEvent>(EVENT_CAP);

    let command = command.to_string();
    let root = root.to_path_buf();
    (
        LspHandle {
            sync: sync_tx,
            events: event_rx,
        },
        Box::pin(run(command, root, sync_rx, event_tx)),
    )
}

/// Marker event that stops the `async-lsp` main loop from inside its own router.
struct Stop;

/// A diagnostic batch as it leaves the router, before it reaches the editor.
struct Published {
    path: PathBuf,
    diagnostics: Vec<Diagnostic>,
}

async fn run(
    command: String,
    root: PathBuf,
    sync: Receiver<DocumentSync>,
    events: Sender<LspEvent>,
) -> Result<(), LspError> {
    let root_url = Url::from_file_path(&root).map_err(|()| LspError::BadRoot(root.clone()))?;

    // The router runs on the main loop's task and must not block, so it forwards
    // batches over an unbounded channel that this task drains and re-publishes on
    // the editor's bounded one. That keeps back-pressure at the editor boundary
    // (where it belongs) without stalling the protocol reader (which would also
    // stall responses to our own requests).
    let (published_tx, mut published_rx) = mpsc::unbounded::<Published>();

    let (mainloop, mut server) = MainLoop::new_client(|_server| {
        let mut router = Router::new(published_tx);
        router
            .notification::<PublishDiagnostics>(|tx, params| {
                if let Ok(path) = params.uri.to_file_path() {
                    let _ = tx.unbounded_send(Published {
                        path,
                        diagnostics: params
                            .diagnostics
                            .into_iter()
                            .map(convert::diagnostic)
                            .collect(),
                    });
                }
                ControlFlow::Continue(())
            })
            // A server is free to chat; none of it changes editor state, and an
            // unhandled notification would otherwise be a protocol error.
            .notification::<ShowMessage>(|_, _| ControlFlow::Continue(()))
            .event(|_, _: Stop| ControlFlow::Break(Ok(())));
        ServiceBuilder::new()
            .layer(async_lsp::panic::CatchUnwindLayer::default())
            .layer(async_lsp::concurrency::ConcurrencyLayer::default())
            .service(router)
    });

    let mut child = async_process::Command::new(&command)
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The server's stderr is its own log stream; inheriting it would scribble
        // over the alternate screen the TUI is drawing on.
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| LspError::Spawn {
            command: command.clone(),
            source,
        })?;
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        return Err(LspError::Protocol("server pipes unavailable".into()));
    };

    // The protocol reader/writer. It owns the child's pipes and runs until the
    // connection ends; `futures::select` below drives it alongside our own work
    // rather than spawning, so the core never touches an executor.
    let mut protocol = Box::pin(mainloop.run_buffered(stdout, stdin));

    let init = futures::select! {
        init = server.initialize(initialize_params(root_url)).fuse() => init,
        // The server died before answering: report that, not a hang.
        r = protocol.as_mut().fuse() => {
            return Err(LspError::Protocol(match r {
                Err(e) => format!("server exited during initialize: {e}"),
                Ok(()) => "server exited during initialize".into(),
            }));
        }
    }
    .map_err(|e| LspError::Protocol(e.to_string()))?;

    check_encoding(init.capabilities.position_encoding.as_ref())?;
    server
        .initialized(InitializedParams {})
        .map_err(|e| LspError::Protocol(e.to_string()))?;

    // Track the language each opened document was announced with, so a change
    // notification can address a document the server actually knows about.
    let mut opened: Vec<PathBuf> = Vec::new();
    let result = loop {
        futures::select! {
            // The protocol loop ending means the server is gone; stop with it.
            r = protocol.as_mut().fuse() => {
                break r.map_err(|e| LspError::Protocol(e.to_string()));
            }
            batch = published_rx.next() => {
                let Some(batch) = batch else { continue };
                // A full event channel back-pressures the server's diagnostics,
                // which is the correct place to absorb a burst.
                if events.send(LspEvent::Diagnostics {
                    path: batch.path,
                    diagnostics: batch.diagnostics,
                }).await.is_err() {
                    break Ok(()); // editor hung up
                }
            }
            message = sync.recv().fuse() => {
                let Ok(message) = message else { break Ok(()) }; // editor hung up
                if let Err(e) = send_sync(&mut server, &mut opened, message) {
                    break Err(e);
                }
            }
        }
    };

    // Best-effort shutdown: we are stopping either way, and a server that has
    // already died must not turn into an error on the way out.
    server.shutdown(()).await.ok();
    server.exit(()).ok();
    server.emit(Stop).ok();
    result
}

/// Reject a server that ignored the encoding negotiation.
///
/// We advertise UTF-16 only, so anything else means every position it sends
/// would be silently misread - squiggles in the wrong place, quietly. Absent
/// means the protocol default, which *is* UTF-16. `PositionEncodingKind` wraps a
/// `String`, so this is an equality check rather than a match pattern.
fn check_encoding(encoding: Option<&PositionEncodingKind>) -> Result<(), LspError> {
    match encoding {
        Some(e) if *e != PositionEncodingKind::UTF16 => Err(LspError::Protocol(format!(
            "server chose unsupported position encoding {e:?}; only UTF-16 is negotiated"
        ))),
        _ => Ok(()),
    }
}

/// The notification a [`DocumentSync`] becomes, or `None` if it should be
/// dropped.
///
/// Split from the sending so the protocol *decisions* - which notification, what
/// version, and the two drop cases - are testable without a language server; the
/// sender below is then a straight-line dispatch with nothing left to get wrong.
enum Outgoing {
    Open(DidOpenTextDocumentParams),
    Change(DidChangeTextDocumentParams),
}

/// Plan the notification for `message`, recording newly-opened documents in
/// `opened`.
///
/// Returns `None` when there is nothing valid to send: a path that is not a file
/// URL (an unnamed or virtual buffer), or a change to a document the server was
/// never told about - which would be a protocol error on its side.
fn outgoing(opened: &mut Vec<PathBuf>, message: DocumentSync) -> Option<Outgoing> {
    match message {
        DocumentSync::Opened {
            path,
            language_id,
            text,
        } => {
            let uri = Url::from_file_path(&path).ok()?;
            if !opened.contains(&path) {
                opened.push(path);
            }
            Some(Outgoing::Open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri,
                    language_id,
                    version: 0,
                    text,
                },
            }))
        }
        DocumentSync::Changed {
            path,
            version,
            text,
        } => {
            if !opened.contains(&path) {
                return None;
            }
            let uri = Url::from_file_path(&path).ok()?;
            Some(Outgoing::Change(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier {
                    uri,
                    version: version as i32,
                },
                // No `range` means "this is the whole document" - the full sync
                // contract described on `DocumentSync`.
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text,
                }],
            }))
        }
    }
}

/// Announce a document open or change to the server (full-text sync, SPEC §5).
fn send_sync(
    server: &mut async_lsp::ServerSocket,
    opened: &mut Vec<PathBuf>,
    message: DocumentSync,
) -> Result<(), LspError> {
    match outgoing(opened, message) {
        Some(Outgoing::Open(params)) => server.did_open(params),
        Some(Outgoing::Change(params)) => server.did_change(params),
        None => return Ok(()),
    }
    .map_err(|e| LspError::Protocol(e.to_string()))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;

/// The `initialize` payload. Advertises UTF-16 positions only (see the module
/// docs) and full-document sync.
fn initialize_params(root: Url) -> InitializeParams {
    InitializeParams {
        workspace_folders: Some(vec![WorkspaceFolder {
            uri: root,
            name: "root".into(),
        }]),
        capabilities: ClientCapabilities {
            general: Some(async_lsp::lsp_types::GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF16]),
                ..Default::default()
            }),
            text_document: Some(async_lsp::lsp_types::TextDocumentClientCapabilities {
                synchronization: Some(TextDocumentSyncClientCapabilities {
                    dynamic_registration: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}
