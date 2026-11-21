# Changelog

## 0.2.0 - 2026-11-21

A ground-up consolidation of the 0.1.0 codebase: same feature set, one
third of the duplication gone, and every module now reads from a single
shared core.

### Rewrite

- **`model.rs`** — one shared toolkit for tree walking, canonical node paths
  (`&label { }` fragments merge into the nodes they extend), reference
  resolution, cell parsing (with C constant-expression evaluation) and the
  raw-text brace scanner. Absorbs the old `util.rs` + `analysis.rs` and the
  per-module copies: four "find node by path" walkers, three label
  collectors, four property lookups and two half-written-node text scanners
  collapsed into one implementation each.
- **`unit.rs`** — translation units in one place: preprocess + parse + a
  single name-collection walk (labels, `&label` refs, macros, `#*-cells`)
  producing `TuData`, plus the per-file diagnostic mapping. Absorbs `tu.rs`
  and the unit-output plumbing that lived in `backend.rs`.
- **Diagnostics split into two disjoint tiers** — buffer tier (syntax,
  includes, bindings, feature notices; instant) and unit tier (duplicate
  labels/properties/nodes on the merged tree, phandle checks, preprocessor
  errors; coalesced ~200 ms, mapped back per file). The old design ran
  overlapping checks in both tiers and merged them by message text.
- **`DiagCache` simplified** — nested subtree memoization kept, label tables
  removed from cache entries (duplicate labels use a dedicated leaf walk),
  `dashmap` → `Mutex<HashMap>`, no `DTSLS_VERIFY_CACHE` debug switch.
- **Workspace** — FIFO context cap replaces the LRU clock, `*_uri` wrapper
  pairs removed, dead `provider_cells` table removed.
- **`backend.rs`** — `Arc<Inner>` state with thin LSP delegates; the
  initialize scan orchestration extracted into `spawn_scan`.
- **Dependencies 13 → 9**: dashmap, memchr, rustc-hash and ropey dropped
  (documents are `String + line_starts` now, parsed without the chunk
  callback indirection).
- **C preprocessor** — the 11-level hand-written expression parser replaced
  by a precedence table.
- **Bindings** — `pattern` positions in compound compatibles are supported
  (Adreno's `^qcom,adreno-[0-9a-f]{8}$` + `const: qcom,adreno` forms no
  longer report "no binding found"); the pointer-keyed `allowed_cache`
  replaced by compiled `RegexSet`s; `regex::RegexSet` per binding for
  `patternProperties`.

### Performance

Benchmarked against 0.1.0 on the same machine (kernel tree, 200 KB dtsi):

- edit + full diagnostics: 2.02 → **1.23 ms** (sm8650.dtsi), 3.44 → **1.84 ms**
  (hamoa.dtsi, 223 KB)
- cross-file jump: 0.72 → **0.23 ms**; references: 1,295 → **3,872 ops/s**
- property completion: 1,893 → **4,386 ops/s**; binding-yaml jump: 1,934 →
  **2,875 ops/s**
- scan→refresh gap after the binding scan: 0.7–1.0 → **0.5–0.8 ms**

Interactive paths stay sub-millisecond; unit runs are coalesced off the
keystroke path.

### Fixed

- Phandle/interrupt diagnostics on included files squiggled from the cell to
  the end of the file (the end position was left in unit coordinates).
- Duplicate/interrupt diagnostics on a board-closure file appeared twice with
  different severities (buffer tier ran the same checks as the unit tier and
  results were merged by message text).
- `interrupts` inlay hint: a trailing partition phandle (GICv3 4-cell form,
  e.g. the `pmu-a520` PPI entries) suppressed the controller hint; it now
  shows the inherited interrupt-parent, which is the actual disambiguation.
- `interrupts` stride lookup never found the controller reference (direct
  child lookup on `prop_data` instead of a descendant walk).
- `interrupt-parent` inherited from the root devicetree was invisible to the
  inlay-hint walk (ancestor filter excluded the root).
- Binding-scan refresh ran with empty include dirs (captured before the
  workspace roots were populated), flagging every `<dt-bindings/...>`
  include as unresolved.
- `#`-prefixed property names (`#clock-cells`) never triggered completion.
- Unchanged files are no longer republished on every unit run.
- The server never swallowed a parse/analysis failure: internal errors are
  published as diagnostics, never silently dropped.

### Added

- `dtsls --version` / `--help`.
- Licensing: AGPL-3.0-only — `COPYING` with the full license text and an
  `SPDX-License-Identifier` header in every source file. Built on the
  GPL-3.0 `tree-sitter-devicetree2` grammar (combined per AGPL-3.0 §13).
