//! Microbenchmarks for the core's per-keystroke hot paths (SPEC §13, §10.4).
//!
//! These exist so perf decisions are *measured*, not guessed - the harness the
//! SPEC's deferred "cursor motion allocates the caret's line on every keystroke"
//! item names as its trigger (SPEC §10.4). What they cover today:
//!
//! - **`cursor_motion`** - one cursor on a single very long line. `Motion::Right`
//!   and `Motion::Down` each copy the caret's line (`Text::line` returns an owned
//!   `String`), so this is the direct cost of holding an arrow key on minified
//!   text - the line-length bound §10.4 accepts, quantified.
//! - **`cursor_motion_scaling`** - the same motion over a growing cursor set, so
//!   the per-cursor cost (and any super-linear surprise) is visible as multi-cursor
//!   grows the count.
//!
//! ## Running
//!
//! Full timed run, saving a baseline to compare a change against:
//! ```sh
//! git stash && cargo bench -p vortex-core -- --save-baseline before
//! git stash pop && cargo bench -p vortex-core -- --baseline before
//! ```
//! Criterion prints the delta per benchmark with a significance verdict.
//!
//! The verification loop runs `cargo bench -p vortex-core -- --test`, which
//! executes each benchmark **once** without measuring - a bit-rot check that the
//! harness still compiles and runs. Wall-clock timings are machine-dependent and
//! noisy, so they are never gated in the loop; they are a tool for A/B comparison
//! when a hot path is touched, read by a human against a baseline.
//!
//! ## Not yet covered (need a lib/bin split or actor drive - deliberately deferred)
//!
//! - The edit path's O(N²) selection remap (`selections_after_edits`) and the
//!   decoration `transform` are `pub(crate)`, reachable only by driving the actor
//!   with an `Action` script - an async, executor-bound bench worth adding when the
//!   remap is the question.
//! - `vortex-tui`'s `render_line`/`style_at`/`span_columns` live in a bin-only
//!   crate, so a bench cannot link them until the frontend grows a lib target.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use vortex_core::{Buffer, Motion, RopeBuffer, Selection, SelectionSet, Text};

/// A buffer of a single line of `n` ASCII bytes - the minified-file shape whose
/// per-keystroke line copy §10.4 flags.
fn long_line(n: usize) -> Text {
    RopeBuffer::from("a".repeat(n).as_str()).text()
}

/// A buffer of `lines` rows, each `width` ASCII bytes wide.
fn grid(lines: usize, width: usize) -> Text {
    let row = "a".repeat(width);
    let joined = vec![row; lines].join("\n");
    RopeBuffer::from(joined.as_str()).text()
}

fn cursor_motion(c: &mut Criterion) {
    let mut group = c.benchmark_group("cursor_motion");

    // Right copies the current line to find the next grapheme (grapheme_after).
    let one_line = long_line(100_000);
    let start = SelectionSet::single(Selection::cursor(0));
    group.bench_function("right_100k_line", |b| {
        b.iter(|| {
            let mut sel = start.clone();
            sel.move_all(black_box(&one_line), Motion::Right, false);
            black_box(sel.primary().head)
        })
    });

    // Down copies the target line to resolve the goal column (vstep).
    let two_lines = grid(2, 100_000);
    group.bench_function("down_100k_line", |b| {
        b.iter(|| {
            let mut sel = start.clone();
            sel.move_all(black_box(&two_lines), Motion::Down, false);
            black_box(sel.primary().head)
        })
    });

    group.finish();
}

fn cursor_motion_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("cursor_motion_scaling");

    for &n in &[1usize, 16, 256] {
        // One cursor per line so every caret has its own line to copy on a motion.
        let text = grid(n, 256);
        let mut base = SelectionSet::single(Selection::cursor(0));
        for _ in 1..n {
            base.add_cursor_below(&text);
        }
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let mut sel = base.clone();
                sel.move_all(black_box(&text), Motion::Right, false);
                black_box(sel.len())
            })
        });
    }

    group.finish();
}

criterion_group!(benches, cursor_motion, cursor_motion_scaling);
criterion_main!(benches);
