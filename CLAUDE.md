# Vortex

Terminal-based text editor: headless Rust core + thin frontend, so the terminal is one
of several possible frontends. See `docs/SPEC.md` for the full architecture and the
decision record.

**This is a Rust Cargo workspace.** The Bun/Node tooling in the parent
`rust/CLAUDE.md` does not apply here - ignore it unless/until a web frontend crate is
added, at which point Bun applies to *that crate only*.

---

## Verification loop (never claim done without this)

Run the full loop before calling any task complete. Order is cheapest-to-most-expensive
so failures surface fast. All steps must pass with zero warnings.

```sh
cargo fmt --all -- --check     # 1. formatting (use `cargo fmt --all` to fix)
cargo clippy --all-targets --all-features -- -D warnings   # 2. lint (warnings are errors)
cargo build --workspace        # 3. compile
cargo test --workspace         # 4. tests
# 5. coverage gates - EVERY file must stay above its crate's floor (SPEC §13).
#    Ratchet: no regress. Current: core 99.4% lines, tui 92.4%.
cargo llvm-cov --package vortex-core --fail-under-file-lines 90 \
  --ignore-filename-regex 'lsp/client\.rs' --summary-only
cargo llvm-cov --package vortex-tui  --fail-under-file-lines 60 --summary-only
# 6. benches still build and run (bit-rot check, NOT a timing gate - see below).
cargo bench --workspace -- --test
```

`lsp/client.rs` is exempted from the core gate (M2): it is the LSP subprocess +
protocol shell - the first genuinely I/O-bound file in the core, the same shape as
`vortex-tui`'s `main.rs`. Its *decisions* are extracted into pure functions
(`check_encoding`, `outgoing`, `initialize_params`) and covered 100%; the `run` loop
itself needs a live language server, which `tests/lsp_rust_analyzer.rs` exercises
(`--ignored`, requires `rust-analyzer` on PATH). With the exemption the rest of the core
holds 99.4%.

The gate uses `--fail-under-file-lines` (per-file), not the package aggregate: a per-file
floor means no single file can slip below its floor while a 100% neighbor masks it in the
total. The floors are asymmetric because the architecture is (SPEC §13): the core is
headless and should stay near-100% (M0 baseline: **100%**), while `vortex-tui` carries a
genuinely untestable I/O shell in `main.rs` alongside logic that *is* extractable and
tested (keymap, viewport/display-column math, the picker, theme loading). The tui floor
activated at M1+ as predicted; every file now clears it with room to spare, so the ratchet
is the binding constraint there, not the floor. M4 added a little untestable I/O to that
shell - the grammar `dlopen`/attach glue (`load_grammar`, `GrammarManager::ensure`), the
frontend twin of `lsp/client.rs`'s exemption: the grammar cdylib is not built under the
coverage harness, so the successful-load path cannot run there. The *resolution* logic
(which library, which queries) is extracted into `grammar.rs` and tested, which is why the
tui line total eased from 89.9% to 88.9% while every file still clears its floor. M5's
watcher took the same shape - `watcher.rs`'s loop is generic over `notify::Watcher` so the
routing is tested against a stand-in, leaving only the backend constructor untested - and
brought the tui total back to 89.9%. M7's in-buffer search lifted it to 91.9%: the find
prompt and the query-replace walk are pure logic with no I/O at all, so `buffersearch.rs`
lands at 100% and pulls the total up rather than diluting it. M8's chrome holds it at
92.4%, and for the reason the milestone is shaped that way: each piece is display-column
math in `layout.rs` (99.5%) plus a config key, so the only untestable part is the
scrollbar's drag state, which lives in the event loop with the rest of the I/O shell.
Sticky context is the exception that proves it - the header's *height* is settled inside
`paint`, so it is asserted through a `TestBackend` render rather than a pure function,
which is why `paint` now hands back the view state it settled on.
Requires `cargo-llvm-cov` >=0.8.6 (the release that added `--fail-under-file-lines`) +
`rustup component add llvm-tools-preview`. Install/upgrade with `cargo install cargo-llvm-cov`.

Step 6 runs the benchmark harness in criterion's `--test` mode: each benchmark executes
**once**, unmeasured, so the loop catches a bench that stopped compiling or started
panicking without paying for a timed run. **Wall-clock timings are deliberately not
gated** - they are machine-dependent and noisy, so a green loop must never depend on them;
a threshold there would fail on a busy laptop and pass on a fast one. The benches are a
*tool*, not a gate: when you touch a hot path, run
`cargo bench --workspace -- --save-baseline before` on the old code and `--baseline before`
on the new, and read criterion's per-benchmark delta yourself.

`vortex-core/benches/hot_paths.rs` covers the per-keystroke motion paths (the line-copy
§10.4 flags); `vortex-tui/benches/layout.rs` covers the display-column paint paths
(`render_line`/`style_at`, and the sorted-span column walk). The tui benches are why the
frontend's logic lives in a **library target** (`crates/tui/src/lib.rs`): a `[[bench]]` can
only link a lib, so the pure paint math had to move out of the binary. `main.rs` is now the
thin I/O shell it always claimed to be and depends on the lib; `testutil.rs` backs both a
`pub mod` in the lib and a `#[cfg(test)] mod` in the binary, since a dependency's
`cfg(test)` items are invisible to the crate depending on it. Still not benched: the edit
path's O(N²) selection remap, which needs an actor-driven bench (noted in the core bench's
header).

Then, for any change with a runtime surface, **actually exercise it** - do not infer
success from a green test suite:
- Core changes: drive the headless core with a script of `Action`s and assert on emitted
  `ViewUpdate`s (the core is designed to be testable without a terminal - see SPEC §1).
- TUI changes: run the editor and confirm the behavior in a real terminal.

"Done" = the loop above passed *and* the change was observed working. Report failures
with their actual output; if a step was skipped, say so.

### After each significant code change
Run `/code-review` on the diff, then `/security-review`, before considering the change
complete. Address (or explicitly triage) findings from both. "Significant" = any change to
`vortex-core`/`vortex-tui` logic, the seam types, or file/LSP handling - not doc-only or
config-only edits.

### Rules
- **Every change gets test coverage, every turn.** New functions get tests in the same
  change; bug fixes get a regression test that fails before the fix. Coverage is ratcheted
  (must not decrease) with floors of ≥90% for `vortex-core` and ≥60% for `vortex-tui`
  (SPEC §13). Untestable glue is marked coverage-ignore *with a reason*, never gamed.
- **`unsafe` requires justification.** A `// SAFETY:` comment stating the invariant, or
  don't write it. Prefer a safe alternative.
- **No `unwrap()` / `expect()` / `panic!` on paths that handle user input, file I/O, or
  LSP responses.** Return `Result` and propagate. `unwrap` is acceptable only where the
  invariant is locally proven (and then note why).
- When fixing a bug, reproduce it first in an e2e setting as close as possible to how the
  end user hits it, then fix.

---

## Architecture invariants (enforced, not just preferred)

These come from `docs/SPEC.md`. Violating one is a bug even if it compiles:

- **`core/` has zero terminal dependencies.** No `crossterm`, no `ratatui` in `core/`'s
  `Cargo.toml`. This is the compile-time guarantee that view logic cannot leak into the
  core. If you need a terminal type in core, the design is wrong.
- **The core/frontend seam is message-passing** (`Action` in, `ViewUpdate` out over a
  channel), never direct method calls across the boundary. This is the future RPC seam.
- **`Action` models intent** (`MoveCursorWordRight`), never keystrokes (`Ctrl+Right`).
  Key→intent translation is frontend-owned.
- **Cursor state is a `SelectionSet`**, never a single cursor. Motions/edits map over the
  set.
- **Positions that outlive a single edit use anchors**, never raw byte offsets - anything
  async (LSP, file watch) must survive concurrent edits.
- **The buffer lives behind a `Buffer` trait; `crop::Rope` never appears in `vortex-core`'s
  public API.** Keeps the CRDT and Tier-3 paged-buffer backends swap-ready (SPEC §2.1,
  §10.4). No eager full-file scans on load (lazy line index, sampled encoding detection).
- **Core state has a single owner** (the editor actor task). No `Arc<RwLock<Editor>>`
  shared across threads; other subsystems send messages in.

---

## Code quality

- Match surrounding style. Prefer simple/readable over clever. Minimize abstraction until
  duplication is proven (three similar lines beats a premature helper).
- Reuse existing functions/patterns before adding new ones.
- **Do not add dependencies without asking** - the stack in SPEC §3 is deliberate. New
  crates need a reason that section doesn't already cover.
- No dead code, commented-out blocks, or stray TODOs unless asked.
- No temporal naming (`new_`, `improved_`, `old_`).
- Verify library APIs against current docs (context7, else web search) before using them -
  don't code from memory. Several stack choices (crossterm sync-output, Kitty flags,
  async-lsp on smol) have version-specific surfaces flagged in the SPEC.

## Git
- Never commit, push, or open a PR unless asked.
- Conventional commits (`feat:`, `fix:`, `refactor:`, `test:`, `chore:`). Explain *why*,
  not *what*. No AI/Claude mentions, no Co-Authored-By trailers.
- Feature branches over committing to main. Atomic commits.
