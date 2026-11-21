# Vibe coding warning && AI slop warning

This entire server was written end-to-end by an AI coding agent in one
working session — the architecture, the Rust, the tests and the benchmark
numbers. Treat it the way you should treat any AI-generated code: read it,
run the tests, and stay suspicious.

# dtsls

A DTS language server for the Linux kernel.

## Implemented

- **LSP over stdio** — initialize, incremental `didChange` (no full reparse),
  `didOpen`/`didClose`, UTF-16 positions, `--version` / `--help`
- **Syntax diagnostics** — tree-sitter ERROR / missing tokens, with
  indentation-aware localization of the unclosed `{` (instead of squiggling
  to EOF)
- **Semantic diagnostics** — duplicate node / property / label names under
  one parent (dtc merges them silently), unit-address sharing warnings
- **Binding validation** — every node with `compatible` is checked against
  the kernel bindings: missing required properties (incl. any-of groups and
  child-node schemas), properties not allowed by strict bindings, unknown
  compatibles as warnings; dt-schema `pattern` positions supported; `$ref`s
  resolve against the kernel tree plus the host **dtschema** python package
- **Phandle / interrupt checks** (ported from dtc) — argument-cell counts vs
  the provider's `#*-cells`, `interrupts` sized as a multiple of the
  controller's `#interrupt-cells` (resolved through `interrupt-parent`,
  inherited up the tree), controller/cells pairing, `&{/path}` refs,
  constant-expression cells
- **Cross-file analysis** — a file is analysed in the context of the board
  that includes it: in-process preprocessor (includes, object/function-like
  macros, `#if`/`#elif`, `#pragma once`), open buffers override disk, and the
  diagnostics are mapped back to the file they came from
- **Completion** — `&label` (unit-wide, cross-file items annotated),
  dt-bindings macros, property names from bindings (required first, existing
  dropped, `#`-prefixed properties included), two-stage `compatible`
  (vendor → device, vendor-less compatibles)
- **Navigation** — goto definition / declaration for labels, find references,
  goto binding YAML for a node's `compatible`
- **Inlay hints** — bidirectional `X` / `X-names` pairing with strides from
  the phandle target's `#*-cells`; `reg` strides from the parent bus; a bare
  `interrupts = <...>` shows its controller from `interrupt-parent`
- **documentSymbol, semantic tokens** — directives as decorators, dead
  properties struck through, unit addresses highlighted as numbers
- **`#include` document links**
- **Feature status notices** — a feature that cannot work reports an ERROR
  at the top of every file; degraded features report a WARNING. Nothing
  degrades silently

## Not implemented

- rename
- hover
- code actions / quick fixes
- formatting
- folding ranges
- `semanticTokens/range` (full-document tokens only)
- completion `resolveProvider`
- workspace-wide symbol search
- non-Linux hosts (the server is Linux-only)

## Build

```sh
cargo build --release
```

See [INSTALL](INSTALL) for the full walkthrough: install paths, nvim setup,
cross-compiling and the optional dtschema/kernel-tree pieces.

## Test

```sh
cargo test                          # unit tests (incl. testdata fixtures)
cargo test -- --ignored             # parses a real kernel dtsi (path baked in test)
```

The end-to-end LSP verification harness (smoke / diagnostics / tu / compat
driven over stdio, plus the latency and throughput benchmarks) is kept out
of the repository as a local-only workflow.

## Architecture notes

One shared toolkit (`model.rs`) owns tree walking, canonical node paths
(`&label { }` fragments merge into the nodes they extend), reference
resolution, cell parsing and the raw-text brace scanner — every feature reads
the tree through it. `unit.rs` builds a translation unit (preprocess + parse +
one name-collection walk) and maps unit diagnostics back to their files.
`workspace.rs` keeps the include graph and per-root snapshots. `backend.rs` is
a thin LSP layer over that state.

## License

dtsls is licensed under the GNU Affero General Public License v3.0 only
(`SPDX-License-Identifier: AGPL-3.0-only` in every source file; full text in
[`COPYING`](COPYING)). It builds on the `tree-sitter-devicetree2` grammar,
which is GPL-3.0; per AGPL-3.0 §13 the two may be combined, and the combined
binary carries both licenses' obligations.
