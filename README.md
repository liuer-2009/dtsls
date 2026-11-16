# dtsls

Fast LSP server for Device Tree Source (DTS), written in Rust. Linux only.

Built on `tower-lsp` + `tree-sitter` with the `tree-sitter-devicetree` grammar
(the dtc-derived one that parses the whole Linux kernel tree).

## Build

```sh
cargo build --release
```

## Capabilities (M0)

- Incremental text sync (`textDocument/didChange`, full reparse avoided)
- Syntax error diagnostics (`textDocument/publishDiagnostics`) — tree-sitter
  ERROR nodes and missing tokens (`missing '>'`, `missing '}'`, ...), republished
  on every change, cleared on close
- `textDocument/documentSymbol` — node tree with full paths, property value kinds
- `textDocument/semanticTokens/full` — labels, node names, properties, numbers,
  strings, comments, preprocessor directives, dts keywords (`/dts-v1/`, `/delete-node/`, ...)

## Test

```sh
cargo test                          # unit tests (incl. testdata fixtures)
cargo test -- --ignored             # parses a real kernel dtsi (path baked in test)
python3 scripts/smoke_test.py       # end-to-end LSP session over stdio
python3 scripts/diagnostics_test.py # broken/valid fixtures -> publishDiagnostics
python3 scripts/bench_real.py /path/to/soc.dtsi   # real-world timing
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
