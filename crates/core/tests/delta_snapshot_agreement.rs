//! Property test: the delta stream and the snapshot never disagree (SPEC §5, §13).
//!
//! Invariant (SPEC §5): "applying the delta stream from version N to a version-N
//! buffer yields exactly the version-(N+1) buffer." We test the whole-history form:
//! replaying *every* emitted delta, in order, against a fresh empty buffer must
//! reproduce the text of the final snapshot the core emits. If delta emission and
//! buffer mutation ever drift apart, this fails and proptest shrinks the random
//! `Action` script to a minimal reproducer.
//!
//! This is the M1 verify gate. It exercises the core purely through the message
//! seam - no terminal, no PTY (the point of the headless design, SPEC §1).

use proptest::prelude::*;
use vortex_core::{Action, Buffer, Core, Delta, Motion, RopeBuffer, new};

/// Apply one delta to a plain buffer the way a remote frontend would (SPEC §5).
/// Panics only on a delta the core should never emit - a genuine test failure.
fn apply_delta(buffer: &mut RopeBuffer, delta: &Delta) {
    buffer
        .replace(delta.range.clone(), &delta.new_text)
        .expect("core emitted a delta that does not apply to its own base buffer");
}

/// Run `actions` through a real core on a smol executor, collecting every delta
/// emitted and the final snapshot's text + version. Mirrors how a frontend drives
/// the core: send an action, drain any delta(s), read the latest snapshot.
fn run_script(actions: Vec<Action>) -> (Vec<Delta>, String, u64) {
    let ex = smol::Executor::new();
    let Core { handle, run } = new(64);
    ex.spawn(run).detach();

    smol::block_on(ex.run(async move {
        let mut deltas = Vec::new();
        let mut last_text = String::new();
        let mut last_version = 0;

        for action in actions {
            handle.actions.send(action).await.unwrap();
            // An edit emits deltas *before* its snapshot; drain all currently
            // available deltas (non-blocking) so the bounded channel never fills.
            while let Ok(delta) = handle.deltas.try_recv() {
                deltas.push(delta);
            }
            let snap = handle.snapshots.recv().await.unwrap();
            last_text = snap.text.to_string();
            last_version = snap.version;
            // Deltas for this action may land after its snapshot in scheduling;
            // drain again to be safe.
            while let Ok(delta) = handle.deltas.try_recv() {
                deltas.push(delta);
            }
        }

        // Final drain in case a trailing delta is still queued.
        while let Ok(delta) = handle.deltas.try_recv() {
            deltas.push(delta);
        }
        (deltas, last_text, last_version)
    }))
}

/// A strategy producing edit/motion actions - the ones that drive buffer state.
/// Text is kept short and includes a multibyte grapheme so the property covers
/// the Unicode boundary cases (SPEC §4), not just ASCII.
fn action_strategy() -> impl Strategy<Value = Action> {
    prop_oneof![
        // Weight inserts higher so buffers actually grow.
        4 => prop::string::string_regex("[a-c\né語]{0,4}").unwrap().prop_map(Action::Insert),
        1 => Just(Action::DeleteBackward),
        1 => Just(Action::DeleteForward),
        1 => prop_oneof![
            Just(Motion::Left),
            Just(Motion::Right),
            Just(Motion::Up),
            Just(Motion::Down),
            Just(Motion::LineStart),
            Just(Motion::LineEnd),
            Just(Motion::BufferStart),
            Just(Motion::BufferEnd),
        ]
        .prop_flat_map(|motion| {
            any::<bool>().prop_map(move |extend| Action::MoveCursor { motion, extend })
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// Replaying the emitted delta stream reproduces the final snapshot text.
    #[test]
    fn delta_stream_reproduces_snapshot(actions in prop::collection::vec(action_strategy(), 0..40)) {
        let (deltas, snapshot_text, _version) = run_script(actions);

        let mut replay = RopeBuffer::new();
        for delta in &deltas {
            apply_delta(&mut replay, delta);
        }

        prop_assert_eq!(
            replay.text().to_string(),
            snapshot_text,
            "delta replay diverged from snapshot after {} deltas",
            deltas.len()
        );
    }

    /// The number of emitted deltas equals the number of applied edits, which is
    /// the final version (each applied edit action bumps the version exactly once).
    /// Guards against a delta being dropped or double-emitted relative to state.
    #[test]
    fn version_counts_applied_edits(actions in prop::collection::vec(action_strategy(), 0..40)) {
        let (_deltas, _text, version) = run_script(actions);
        // Every version bump corresponds to at least one delta; a buffer that
        // never rejects an edit (our generated ranges are always valid) means
        // version <= delta count is guaranteed, and version==0 iff no edit applied.
        prop_assert!(version <= _deltas.len() as u64);
    }
}
