# Editor runtime files

Data the editor loads at runtime rather than compiling in - the same split Helix
and Neovim use, and what makes grammars and their queries swappable without
rebuilding the editor (SPEC §3, §14, M4).

## Layout

- `grammars/` - loadable grammar libraries (`lib<name>_grammar.dylib` / `.so` /
  `.dll`), each exporting the uniform `vortex_grammar` entry point. Built from the
  `grammar-*` workspace crates; **not committed** (see `.gitignore`). For an
  install, copy each built `grammar-*` cdylib here; for development this directory
  can stay empty (see Resolution below).
- `queries/<language>/` - the tree-sitter highlight queries for a language:
  `highlights.scm` (required) and `injections.scm` (optional). Committed, since
  they are source, and version-paired with the grammar they target.

## Resolution

The editor looks for this directory, in order:

1. `$VORTEX_RUNTIME`, if set.
2. A `runtime/` dir beside the executable or any ancestor of it (an install).
3. A `runtime/` dir in the current directory or any ancestor (running from a repo
   checkout - the dev path).

The grammar library itself is resolved from `$VORTEX_RUNTIME/grammars/` if present,
otherwise from the directory of the running executable (where `cargo build` places
the `grammar-*` cdylibs beside `vortex`), so a plain `cargo build --workspace`
followed by `cargo run` highlights out of the box.

## Provenance

`queries/rust/*.scm` are vendored verbatim from `tree-sitter-rust` 0.24.2, the
version the `grammar-rust` crate compiles, so captures and grammar node names match.
