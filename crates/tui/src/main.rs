//! `vortex-tui` - the terminal frontend (binary `vortex`).
//!
//! M0 proves the core/frontend seam end to end: it owns the executor, spawns the
//! core actor, sends an `Action`, and prints the `ViewSnapshot`/`Notification` it
//! gets back. No raw mode or ratatui yet - terminal rendering and the frame loop
//! (SPEC §5, §7) arrive in M1. The point of M0 is that this file spawns the core
//! and drives it purely through messages (SPEC §1).

use vortex_core::{Action, Core};

fn main() {
    let ex = smol::Executor::new();

    // The frontend owns the runtime and spawns the core's actor loop on it. The
    // core exposed no executor type - it just handed us a future to run.
    let Core { handle, run } = vortex_core::new(1024);
    ex.spawn(run).detach();

    smol::block_on(ex.run(async move {
        // Prove the round-trip: request a snapshot and render it (as text, for
        // now). The core can shut down and drop its senders at any point, so a
        // recv/send error just means "core is gone" - exit cleanly, never panic.
        if handle.actions.send(Action::RequestSnapshot).await.is_err() {
            println!("vortex M0: core stopped before first request");
            return;
        }
        match handle.snapshots.recv().await {
            Ok(snapshot) => println!(
                "vortex M0: buffer {:?} version {} ({} bytes of text)",
                snapshot.buffer_id,
                snapshot.version,
                snapshot.text.len()
            ),
            Err(_) => {
                println!("vortex M0: core closed before sending a snapshot");
                return;
            }
        }

        // Ask the core to shut down and confirm it acknowledges.
        let _ = handle.actions.send(Action::Quit).await;
        match handle.notifications.recv().await {
            Ok(note) => println!("vortex M0: core said {note:?}"),
            Err(_) => println!("vortex M0: core channel closed"),
        }
    }));
}
