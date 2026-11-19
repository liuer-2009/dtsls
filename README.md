# dtsls

Fast LSP server for Device Tree Source (DTS), written in Rust. Linux only.

Built on `tower-lsp` + `tree-sitter` with the `tree-sitter-devicetree2` grammar
(whame's dtc-derived grammar, built to parse every DTS file in the Linux
kernel tree — GPL-3.0, which makes this binary GPL-3.0 too).

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
- **Analysis context (dts-lsp's `activeContext`)** — an active board claims
  the files in its include closure: a `.dtsi` opened from the board (goto
  definition, or a `#include` link) is analysed inside the board's unit, while
  a `.dtsi` opened on its own is treated as a standalone dts with its own
  closure. Opening a board later adopts standalone documents that it includes.
  Diagnostics are only published into documents analysed in that context.
  Binding checks wait for the background binding scan, and every open document
  is re-diagnosed and republished when the scan finishes (the active document
  first, the rest computed in parallel); the workspace scan runs after that so
  the refresh is not competing for CPU. At most 8 contexts keep their unit and
  preprocessed snapshot; the least recently used one is evicted (open documents and the active board are never
  evicted) and re-analysed when it is next used.
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
  changed region. Duplicate labels, duplicate properties,
  duplicate node names and syntax errors are therefore judged on the merged
  tree, exactly like dtc; `#ifdef` branches are resolved by the preprocessor
  instead of being guessed. Re-runs are coalesced (200 ms) and only happen for
  open or affected boards. Labels, `&label` references, macro definitions and
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
python3 scripts/smoke_test.py       # end-to-end LSP session over stdio
python3 scripts/compat_test.py      # two-stage compatible completion
python3 scripts/tu_test.py          # cross-file checks via cpp + line markers
python3 scripts/diagnostics_test.py # broken/valid fixtures -> publishDiagnostics
python3 scripts/bench.py            # state update / jump / completion latencies
python3 scripts/bench_rate.py       # throughput: ops per fixed >=100 ms window
python3 scripts/bench_startup.py    # binding scan -> refreshed diagnostics gap
python3 scripts/bench_real.py /path/to/soc.dtsi   # real-world parse timing
```

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

- M1: workspace index (labels / nodes / phandles), `&label` completion, goto definition/references
- M2: rename, `*-names` inlay hints (stride from `#*-cells` of the phandle target)
- M3: YAML binding integration (`Documentation/devicetree/bindings`)
- M4: dtc diagnostics bridge, editor packaging
