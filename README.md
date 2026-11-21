# dtsls

Fast LSP server for Device Tree Source (DTS), written in Rust. Linux only.

Built on `tower-lsp` + `tree-sitter` with the `tree-sitter-devicetree2` grammar
(whame's dtc-derived grammar, built to parse every DTS file in the Linux
kernel tree — GPL-3.0). dtsls itself is AGPL-3.0-only; per AGPL §13 the
combined binary carries both obligations (see License).

## Build

```sh
cargo build --release
```

## Capabilities (M0)

- Incremental text sync (`textDocument/didChange`, full reparse avoided)
- Syntax error diagnostics (`textDocument/publishDiagnostics`) — tree-sitter
  ERROR nodes and missing tokens (`missing '>'`, `missing '}'`, ...), republished
  on every change, cleared on close
- Semantic diagnostics — duplicate node names under one parent (error; dtc
  silently merges them), same unit address shared by different nodes (warning;
  legitimate on e.g. Qualcomm GENI serial engines)
- `textDocument/documentSymbol` — node tree with full paths, property value kinds
- `textDocument/semanticTokens/full`:
  - labels (`declaration`), node names, properties, numbers, strings, comments,
    `#include`/`#define` macros
  - slash directives (`/dts-v1/`, `/memreserve/`, `/include/`, `/bits/`, ...)
    as `decorator` for a distinct color
  - `/delete-node/` + `/delete-property/` directives and their targets rendered
    `deprecated` (dim/strikethrough)
  - overridden properties rendered `deprecated`: later same-name definitions,
    properties removed by `/delete-property/`, and — for same-file overlays —
    base properties overridden by `&label { }` / `&{/path} { }` extensions
    (last definition wins, matching dtc merge order)
- **Analysis context** — an active board claims the files in its include
  closure: a `.dtsi` opened from the board (goto definition, or a `#include`
  link) is analysed inside the board's unit, while a `.dtsi` opened on its own
  is treated as a standalone dts with its own closure. Opening a board later
  adopts standalone documents that it includes. Diagnostics are only
  published into documents analysed in that context. At most 8 board contexts
  keep their unit and preprocessed snapshot (FIFO eviction; open documents and
  the active board are never evicted) and are re-analysed when reused.
- **`#include` links** — include paths are document links, so "go to file" on
  an include lands in the included source with the board still active
- **M1: `&label` completion** — trigger on `&`; the active translation unit
  provides the labels (own labels first-free, cross-file items show
  `path — file`), and the live buffer is merged so a label just typed appears
  before the next unit run. Before the first unit run only the buffer's own
  labels are offered
- **M1: workspace scan** — on initialize only `.dts`/`.dtsi` files are visited
  and just their `#include` lines are read to build the root list and the
  include graph (no parsing). That is what maps a file to the boards whose
  unit contains it; all name tables come from the units themselves
- **M1: goto definition / find references** — resolved from the unit's labels
  and `&label` references (exact, post-preprocessor); without a unit the
  current buffer is searched
- **M1: macro completion** — object-like `#define`s (no function-like macros)
  from the file, its `#include`s and the workspace; `<dt-bindings/...>` angle
  includes resolve against auto-detected kernel include dirs (walks up for
  `include/dt-bindings`) or `initializationOptions.includePaths`. Triggers:
  `&` for labels, ALL-CAPS words / `<` for macros; lowercase never pops
- **M3: binding validation** — every node with `compatible` is checked against
  the kernel bindings (`Documentation/devicetree/bindings`, auto-detected or
  `initializationOptions.bindingsPath`). Binding violations (missing required
  property, property not allowed by a strict binding) are ERRORS — including
  in `.dtsi` fragments; a `compatible` with no binding is a WARNING. Compound
  compatibles (`items:`) match position by position; child-node schemas in
  `patternProperties` validate as their own bindings; meta-schema properties
  (`pinctrl-*`, `*-supply`, `assigned-clocks`, ...) are always allowed.
  `$ref`s and dtschema-hosted compatibles (`cache`, `operating-points-v2`)
  resolve against the host's **dtschema python package** — install it with
  `pip install dtschema` (or point `DTSLS_SCHEMAS_DIR` at a package dir).
  Nothing is downloaded; without the package the feature is degraded and
  unresolved `$ref`s only disable the unknown-property check for that binding.
- **Feature status notices** — a feature that cannot work at all reports an
  ERROR at the top of every file; a feature that works with reduced fidelity
  reports a WARNING. Message shape is always `<feature>: unavailable|degraded
  — <detail>`. Current notices: `devicetree bindings` (no bindings dir found →
  error), `dtschema` (pip package missing → warning), `dt-bindings headers`
  (kernel tree without include/dt-bindings → warning), `workspace` (no
  workspace root → warning). Nothing degrades silently.
- **Cross-file checks as translation units** — like clangd, a file is checked
  in the context of the root board that includes it: the board `.dts` plus its
  include closure is preprocessed **in-process** (object/function-like macros,
  `#if`/`#elif`, includes, `#pragma once`, comments), open buffers override the
  on-disk files, and a source map ties every expanded line back to its file and
  line. No `cpp` process is spawned per edit; the preprocessed text is reparsed
  incrementally (common prefix/suffix), so a localized edit only reshapes the
  changed region. Checks are split so they never run twice: the **buffer tier**
  (syntax errors, unresolved includes, binding violations, feature notices)
  answers on the live buffer within a keystroke, and the **unit tier**
  (duplicate labels/properties/nodes on the merged tree, phandle-argument
  checks, preprocessor errors) republishes ~200 ms after typing settles,
  mapped back to the file each diagnostic came from — exactly like dtc judges
  the merged tree, with `#ifdef` branches resolved by the preprocessor instead
  of guessed. A file not covered by a unit yet also gets duplicate checks from
  the buffer tier. Labels, `&label` references, macro definitions and
  `#*-cells` come from the unit, so completion, go-to-definition, references,
  macro completion and `*-names` hints all see macro-expanded names and the
  branches the preprocessor actually kept; the live buffer is merged on top for
  labels so a name just typed appears before the next run. Without a unit the
  current buffer is the only source, and `&` completion falls back to scanning
  its `#define`s / labels directly.
- **Fast state updates** — diagnostics only touch structural nodes (property
  values are never entered), syntax recovery is skipped unless the tree has
  errors, and per-subtree results are memoized by subtree text: a keystroke
  rechecks just the node it touched. Unit re-runs are coalesced and start
  ~200 ms after typing pauses; the preprocessed text is reparsed incrementally. On a 200 KB dtsi an edit plus its full
  diagnostics takes ~2 ms server-side (~440 edits/s end to end, 1 s window).
- **M3: goto binding** — with the cursor on a node (not on a label), `gd`
  (definition) and `gD` (declaration) jump to the YAML binding of the node's
  `compatible`, landing on its `compatible:` property. Half-written nodes work
  too (the text reader supplies the compatible list when the tree is broken)
- **Inlay hints** — bidirectional `X` / `X-names` pairing with strides from
  the phandle target's `#*-cells` (`reg` uses the parent bus's
  `#address-cells + #size-cells`; `interrupts` uses the interrupt-parent's
  `#interrupt-cells`, walking up the tree). A bare `interrupts = <...>` (no
  inline `&ref`) additionally shows its controller inline — `&intc` from
  `interrupt-parent`, tooltip with the controller path and `#interrupt-cells`
- **M3: property completion** — typing a property name inside a node whose
  `compatible` has a binding suggests that binding's properties (required
  ones first, already-present ones dropped); value positions never trigger.
  Works while the node is still half-written (missing `;` and closing braces):
  tree-sitter's recovery collapses such regions into one ERROR node, so the
  enclosing node is re-read from the raw text (indentation-aware brace
  matching, `{` on the next line and header-without-braces included), which
  makes the property list available right after `compatible` is accepted
- **M3: `compatible` completion** — typing a property name also offers
  `compatible = ""` (snippet, cursor between the quotes) on any node that does
  not have one; inside the string the server offers vendors first (`qcom`,
  `ti`, … plus vendor-less compatibles such as `operating-points-v2`), and
  after `vendor,` that vendor's devices. Endpoint/port nodes are skipped.
  Lists come straight from the binding scan (in-memory, no I/O on the path)

## Test

```sh
cargo test                          # unit tests (incl. testdata fixtures)
cargo test -- --ignored             # parses a real kernel dtsi (path baked in test)
```

The end-to-end LSP verification harness (smoke / diagnostics / tu / compat
driven over stdio, plus the latency and throughput benchmarks) is kept out
of the repository as a local-only workflow.

## Neovim

```lua
vim.lsp.config('dtsls', {
  cmd = { '/dtsls/target/release/dtsls' },
  filetypes = { 'dts', 'dtsi', 'dtso', 'overlay' },
})
vim.lsp.enable('dtsls')
```

Logs go to `$DTSLS_LOG` (default `/tmp/dtsls.log`), verbosity via `RUST_LOG`.

## Roadmap

All milestones are implemented: M1 (workspace scan, `&label` completion, goto
definition/references), M2 (`*-names` inlay hints), M3 (YAML binding
integration, dtc-style phandle checks), M4 (packaging is the Neovim snippet
above).

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
