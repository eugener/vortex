//! Microbenchmarks for the frontend's per-line paint hot paths (SPEC §5, §13).
//!
//! These are the tui half of the harness in `vortex-core/benches/hot_paths.rs`,
//! and exist for the same reason: to decide the remaining perf items by
//! measurement, not guesswork. They are reachable only because the layout logic
//! lives in the library target - a `[[bench]]` cannot link a bin.
//!
//! - **`render_line_overlays`** - the O(width × overlays) `style_at` scan inside
//!   [`layout::render_line`], measured as the overlay count on a line grows. This
//!   is the "resolve `render_line`'s `style_at` into an interval sweep" item: run
//!   it before deciding whether that rewrite is worth its complexity.
//! - **`span_columns`** - resolving a line's sorted syntax spans to display
//!   columns, `walker` (one shared [`ColumnWalker`] pass, the shipped path) versus
//!   `fresh_scan` (a fresh scan from byte 0 per span, the pre-optimization path).
//!   The gap between the two is the win from resolving highlights in one pass,
//!   quantified rather than asserted.
//!
//! Running and the loop's `--test` smoke mode: see the header of
//! `vortex-core/benches/hot_paths.rs`. Timings are a human-read A/B tool against a
//! saved baseline, never gated.

use std::hint::black_box;
use std::ops::Range;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ratatui::style::{Color, Style};
use vortex_tui::layout::{self, ColumnWalker};

fn render_line_overlays(c: &mut Criterion) {
    let mut group = c.benchmark_group("render_line_overlays");
    let width = 200usize;
    let line = "a".repeat(width);
    let base = Style::new().bg(Color::Indexed(236));

    // Non-overlapping overlays tiling the window, as a syntax-highlighted line
    // carries: each painted cell scans every overlay (style_at), so cost grows
    // with the product of width and overlay count.
    for &n in &[4usize, 32, 200] {
        let step = (width / n).max(1);
        let overlays: Vec<(Range<usize>, Style)> = (0..n)
            .map(|i| {
                let start = i * step;
                let style = Style::new().fg(Color::Indexed((i % 255) as u8));
                (start..(start + step).min(width), style)
            })
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                let spans =
                    layout::render_line(black_box(&line), 0, width, base, black_box(&overlays));
                black_box(spans.len())
            })
        });
    }
    group.finish();
}

fn span_columns(c: &mut Criterion) {
    let mut group = c.benchmark_group("span_columns");
    // A long (minified) line where per-span rescanning is quadratic.
    let line = "a".repeat(10_000);
    let line_len = line.len();
    let line_end_excl = line_len + 1;

    for &k in &[16usize, 128, 1024] {
        let step = line_len / k;
        // k sorted, non-overlapping spans - the highlights_in invariant.
        let spans: Vec<(usize, usize)> = (0..k).map(|i| (i * step, i * step + step / 2)).collect();

        // The shipped path: one walker resolves all spans in a single pass.
        group.bench_with_input(BenchmarkId::new("walker", k), &k, |b, _| {
            b.iter(|| {
                let mut walker = ColumnWalker::new(black_box(&line), 4);
                let mut acc = 0usize;
                for &(s, e) in &spans {
                    if let Some(r) =
                        layout::span_columns(&mut walker, line_len, 0, line_end_excl, s, e)
                    {
                        acc += r.end - r.start;
                    }
                }
                black_box(acc)
            })
        });

        // The pre-optimization path: selection_columns rescans from byte 0 per span.
        group.bench_with_input(BenchmarkId::new("fresh_scan", k), &k, |b, _| {
            b.iter(|| {
                let mut acc = 0usize;
                for &(s, e) in &spans {
                    if let Some(r) =
                        layout::selection_columns(black_box(&line), 0, line_end_excl, 4, s, e)
                    {
                        acc += r.end - r.start;
                    }
                }
                black_box(acc)
            })
        });
    }
    group.finish();
}

criterion_group!(benches, render_line_overlays, span_columns);
criterion_main!(benches);
