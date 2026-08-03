//! Microbenchmarks for the picker's per-keystroke filter path (SPEC §10.4, §13).
//!
//! One question, and it is the one a code review raised without a number attached:
//! **`refilter` resolves match marks for every ranked row, when at most sixteen are
//! ever painted.** Ranking is one pass over the corpus and has to be; the marks are a
//! *second* full `nucleo` alignment per surviving row, run so `mark_row` can highlight
//! the characters the query earned (SPEC §7.5, M10). The file picker offers up to
//! `filepicker::MAX_FILES` = 10_000 rows, and a one-character query matches most of
//! them, so the second pass is bounded by the corpus rather than by the viewport.
//!
//! Fixing that properly means resolving marks for the visible window, which is known
//! only inside `render(&self)` - and `Pattern::indices` needs the matcher mutably. So
//! it is a change to the `Layer` seam, not a cleanup, and it should be paid for with a
//! measurement rather than a hunch. That measurement is here.
//!
//! - **`keystroke`** - the shipped path: one character typed into a picker of `n`
//!   rows, resolved through the real keymap in the layer's own context so it is the
//!   shipped dispatch and not a reimplementation of it. This is what a user waits
//!   for. (`compositor::send` says the same two lines, but it is `cfg(test)` and a
//!   bench is not a test.)
//! - **`rank_only`** - the same corpus through `Pattern::match_list` alone, which is
//!   the work the filter *cannot* avoid. The gap between the two series is what
//!   resolving marks eagerly costs.
//!
//! Read them together. If `keystroke` is roughly `rank_only`, the second pass is
//! noise and the seam change is not worth its complexity. If it is a multiple of it,
//! the window fix buys back that multiple on every keystroke into a large picker.
//!
//! **The answer, measured 2026-08-02 (M1 laptop, so read the ratio, not the µs).**
//! `keystroke` is ~2.6x `rank_only` at every size, and both are linear in the
//! corpus: 31 / 155 / 310 µs against 13 / 61 / 118 µs at 1k / 5k / 10k rows. So the
//! marks pass really is what the review said it was - about 190 µs of the 310 at the
//! file picker's ceiling, and bounded by the corpus rather than by the sixteen rows
//! it feeds.
//!
//! **And it stays.** 310 µs is under 2% of a 16.7 ms frame, at a corpus size the
//! picker caps itself at, for work that happens once per keystroke and not once per
//! frame. Resolving marks for the visible window would mean a mutable pre-paint pass
//! on the `Layer` trait - every surface pays that complexity so one of them can save
//! a fifth of a millisecond nobody can perceive. Rejected on the number.
//!
//! What would reopen it: a picker whose corpus is not capped (a project-wide symbol
//! list), a matcher change that makes `indices` superlinear, or this series drifting
//! away from linear. That is what the bench is for now - a tripwire, not a to-do.
//!
//! Running and the loop's `--test` smoke mode: see the header of
//! `vortex-core/benches/hot_paths.rs`. Timings are a human-read A/B tool against a
//! saved baseline, never gated.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatcherConfig, Matcher};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use vortex_tui::command::Command;
use vortex_tui::compositor::Layer;
use vortex_tui::config::Config;
use vortex_tui::keymap::Keymap;
use vortex_tui::picker::{Item, Picker};

/// Corpus sizes: a middling project, and the file picker's own ceiling.
const SIZES: [usize; 3] = [1_000, 5_000, 10_000];

/// Path-shaped labels, which is what the two pickers that carry ten thousand rows
/// hold. Built once and cloned into each fresh picker, so the corpus is the same
/// string set in every series.
fn labels(n: usize) -> Vec<String> {
    (0..n)
        .map(|k| format!("src/module_{}/component_{k}.rs", k % 64))
        .collect()
}

fn items(labels: &[String]) -> Vec<Item> {
    labels
        .iter()
        .map(|label| Item {
            label: label.clone(),
            dim_columns: 0,
            shortcut: None,
            command: Command::OpenPalette,
        })
        .collect()
}

fn keystroke(c: &mut Criterion) {
    let mut group = c.benchmark_group("picker_keystroke");
    let config = Config::default();
    let keymap = Keymap::default();
    // A printable key the `picker` context does not bind, so it falls through to the
    // query - which is the path a user types on.
    let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
    for n in SIZES {
        let corpus = labels(n);
        // A fresh picker per iteration, built in criterion's *untimed* setup: a
        // picker reused across iterations would grow its query a character at a
        // time and measure a different filter every pass.
        group.bench_with_input(BenchmarkId::new("keystroke", n), &n, |b, _| {
            b.iter_batched_ref(
                || Picker::new("Bench", items(&corpus), true, &config),
                |picker| {
                    let bound = keymap.bound(picker.context(), key);
                    black_box(picker.handle_key(key, bound))
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

fn rank_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("picker_keystroke");
    for n in SIZES {
        let corpus = labels(n);
        // The floor: rank the corpus and keep the order, with no second pass for
        // the marks. Mirrors `refilter`'s first half - same pattern parse, same
        // path-tuned matcher config, same haystacks.
        group.bench_with_input(BenchmarkId::new("rank_only", n), &n, |b, _| {
            let mut matcher = Matcher::new(MatcherConfig::DEFAULT.match_paths());
            b.iter(|| {
                let pattern = Pattern::parse("c", CaseMatching::Ignore, Normalization::Smart);
                let ranked = pattern.match_list(corpus.iter(), &mut matcher);
                black_box(ranked.len())
            });
        });
    }
    group.finish();
}

criterion_group!(benches, keystroke, rank_only);
criterion_main!(benches);
