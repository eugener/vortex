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
//! - **`indent_guides`** - substituting the guide glyphs into a row's painted text
//!   ([`layout::with_indent_guides`]), measured as a line's *indentation* grows - the
//!   one input here that **file content** sizes rather than the viewport. Two series,
//!   and the gap between them is the point. `raw` passes every guide the line offers,
//!   so it is the function's own curve: it should be **linear** in the indent, and
//!   anything steeper means the membership cursor has regressed to a per-cell scan.
//!   `clipped` runs [`layout::guides_in_window`] first, which is the shipped path, and
//!   should stay **flat** as the indent grows however long the line's whitespace is. If
//!   `clipped` ever starts tracking `raw`, the viewport bound (SPEC §10.4) is gone and a
//!   file can size the frame again.
//!
//! - **`bufferline`** - fitting the head bar's tab strip, which repaints every
//!   frame and allocates per open buffer. Measured across buffer counts and at
//!   both widths that matter: `fits` (every tab shown, the common case) and
//!   `windowed` (a narrow bar, which runs the window-growing loop as well). Use it
//!   before adding anything per-tab to this path, and to decide whether the strip
//!   ever needs caching between frames.
//!
//! Running and the loop's `--test` smoke mode: see the header of
//! `vortex-core/benches/hot_paths.rs`. Timings are a human-read A/B tool against a
//! saved baseline, never gated.

use std::hint::black_box;
use std::ops::Range;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use ratatui::style::{Color, Style};
use vortex_core::{BufferId, BufferInfo};
use vortex_tui::config::Glyphs;
use vortex_tui::layout::{self, ColumnWalker};

/// The marks the chrome paints by default; the bench measures the paint, not the
/// profile, and both profiles are one cell per mark.
const GLYPHS: Glyphs = Glyphs::UNICODE;

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

/// Indent guides run per visible row per frame, and their cost is driven by *file
/// content* (a line's indentation) rather than by the viewport - so a line with a
/// pathological amount of leading whitespace is the case to watch, not a deeply nested
/// one. Depths here run from realistic (16 columns, four levels of Rust) to absurd
/// (16 384) precisely to show which term dominates as the indent grows: linear in the
/// indent is the cost of walking cells that exist, anything steeper is the membership
/// test being re-scanned per cell.
fn indent_guides(c: &mut Criterion) {
    let mut group = c.benchmark_group("indent_guides");
    let tab_width = 4;
    // An 80-column window, which is what the painter actually hands this.
    let window = 80usize;
    for &indent in &[16usize, 256, 4096, 16_384] {
        let line = format!("{}code();", " ".repeat(indent));
        let columns: Vec<usize> = (0..indent).step_by(tab_width).collect();

        // `raw` is the function's own curve: every guide the line offers, which is what
        // it costs before the painter clips. Read it to check the curve is linear -
        // anything steeper means the membership cursor has regressed to a scan.
        group.bench_with_input(BenchmarkId::new("raw", indent), &indent, |b, _| {
            b.iter(|| {
                let out = layout::with_indent_guides(black_box(&line), black_box(&columns), GLYPHS);
                black_box(out.len())
            })
        });

        // `clipped` is the shipped path: `guides_in_window` first, so the work is the
        // window's and not the file's. This is the line that should stay flat as the
        // indent grows - if it tracks `raw`, the clip is gone.
        group.bench_with_input(BenchmarkId::new("clipped", indent), &indent, |b, _| {
            b.iter(|| {
                let visible = layout::guides_in_window(black_box(&columns), 0, window);
                let out = layout::with_indent_guides(black_box(&line), visible, GLYPHS);
                black_box(out.len())
            })
        });
    }
    group.finish();
}

/// The bufferline is refitted on every frame, so its cost scales with how many
/// buffers are open, not with how often they change. Both widths are measured: one
/// where every tab fits (the fast path) and one narrow enough to force the window to
/// be grown around the active tab.
fn bufferline(c: &mut Criterion) {
    let mut group = c.benchmark_group("bufferline");
    for &n in &[2usize, 8, 32] {
        let buffers: Vec<BufferInfo> = (0..n)
            .map(|i| BufferInfo {
                id: BufferId(i as u64),
                path: Some(format!("/project/src/module_{i}.rs").into()),
                modified: i % 3 == 0,
            })
            .collect();
        // Active in the middle, so the windowed case grows in both directions.
        let active = BufferId((n / 2) as u64);

        group.bench_with_input(BenchmarkId::new("fits", n), &n, |b, _| {
            b.iter(|| black_box(layout::bufferline(black_box(&buffers), active, 400, GLYPHS)))
        });
        group.bench_with_input(BenchmarkId::new("windowed", n), &n, |b, _| {
            b.iter(|| black_box(layout::bufferline(black_box(&buffers), active, 60, GLYPHS)))
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    render_line_overlays,
    span_columns,
    indent_guides,
    bufferline
);
criterion_main!(benches);
