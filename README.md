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
- **M1: `&label` completion** — trigger on `&`, items from the
  current file plus a background-built index of quoted `#include`s (own labels
  preferred, cross-file items show `path — file`). Steady state reads a
  prebuilt index: sub-millisecond, no tree walk on the request path
- **M1: workspace index** — on initialize, all `.dts/.dtsi/.dtso` under the
  workspace roots are parsed in parallel into per-file contribution tables
  (label defs with spans, `&label` usages, include edges). Open buffers are
  re-indexed on every change with version-guarded insertion
- **M1: goto definition / find references** — resolves `&label` through the
  translation unit (own file → transitive includes) with a workspace-wide
  fallback; references search every indexed file. Both are pure index lookups
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
  Nothing is downloaded; without the package every file gets one ERROR at the
  top telling you to install it, and unresolved `$ref`s only disable the
  unknown-property check for that binding. If no kernel bindings directory is
  found at all, every file also gets a top-of-file ERROR — the feature is
  never silently dead.
- **M3: goto binding** — with the cursor on a node (not on a label), `gd`
  jumps to the YAML binding of the node's `compatible`
- **M3: property completion** — typing a property name inside a node whose
  `compatible` has a binding suggests that binding's properties (required
  ones first, already-present ones dropped); value positions never trigger

## Test

```sh
cargo test                          # unit tests (incl. testdata fixtures)
cargo test -- --ignored             # parses a real kernel dtsi (path baked in test)
python3 scripts/smoke_test.py       # end-to-end LSP session over stdio
python3 scripts/diagnostics_test.py # broken/valid fixtures -> publishDiagnostics
python3 scripts/bench.py            # state update / jump / completion latencies
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
