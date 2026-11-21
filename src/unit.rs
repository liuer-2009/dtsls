// SPDX-License-Identifier: AGPL-3.0-only
//! Translation units for diagnostics.
//!
//! Devicetree sources are compiled the way the kernel does it: the
//! preprocessor expands `#include`, `#define` and `#ifdef`, then dtc parses
//! the result. Checks that span files (duplicate labels, duplicate properties,
//! duplicate node names, phandle arguments) only make sense on that
//! preprocessed text, so we do what clangd does: analyse a file in the context
//! of the root that includes it, and map positions back through a source map.
//!
//! The expansion runs in-process (see [`crate::preproc`]) — no `cpp` process
//! per edit — and open editor buffers override the on-disk files, which is the
//! equivalent of clangd's VFS.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
use tree_sitter::Node;

use crate::document::Document;
use crate::model;

/// Where one line of the preprocessed text came from.
#[derive(Clone, Copy, Debug)]
pub struct SourceRef {
    /// index into [`TuResult::files`]
    pub file: u32,
    /// 0-based line in that file
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct TuError {
    /// index into [`TuResult::files`] when the path is known
    pub file: Option<u32>,
    /// 0-based line
    pub line: u32,
    /// 0-based column
    pub column: u32,
    pub message: String,
}

/// A macro definition seen while preprocessing.
#[derive(Clone, Debug)]
pub struct TuDefine {
    pub name: String,
    pub value: String,
    pub file: Option<u32>,
}

pub struct TuResult {
    pub files: Vec<PathBuf>,
    /// preprocessed source with directives, comments and inactive branches
    /// removed
    pub text: String,
    /// one entry per line of `text`
    pub lines: Vec<SourceRef>,
    /// preprocessor diagnostics (missing includes, bad directives, ...)
    pub errors: Vec<TuError>,
    /// effective macro set after preprocessing
    pub defines: Vec<TuDefine>,
}

impl TuResult {
    pub fn locate(&self, line: u32) -> Option<(SourceRef, &Path)> {
        let source = *self.lines.get(line as usize)?;
        Some((source, &self.files[source.file as usize]))
    }
}

pub struct TuInput<'a> {
    pub root: &'a Path,
    pub include_dirs: &'a [PathBuf],
    /// unsaved editor buffers, keyed by canonical absolute path
    pub overlays: &'a BTreeMap<PathBuf, String>,
}

pub fn preprocess(input: &TuInput<'_>) -> TuResult {
    let expanded = crate::preproc::expand(&crate::preproc::Input {
        root: input.root,
        include_dirs: input.include_dirs,
        overlays: input.overlays,
    });
    TuResult {
        files: expanded.files,
        text: expanded.text,
        lines: expanded
            .lines
            .into_iter()
            .map(|(file, line)| SourceRef { file, line })
            .collect(),
        errors: expanded
            .errors
            .into_iter()
            .map(|error| TuError {
                file: Some(error.file),
                line: error.line,
                column: 0,
                message: error.message,
            })
            .collect(),
        defines: expanded
            .defines
            .into_iter()
            .map(|define| TuDefine {
                name: define.name,
                value: define.value,
                file: Some(define.file),
            })
            .collect(),
    }
}

// ------------------------------------------------------------ unit name tables

/// macro name -> (value snippet, defining file name)
pub type DefineMap = HashMap<String, (String, String)>;

/// `#address-cells`-style metadata used to derive `X-names` strides.
#[derive(Clone, Debug)]
pub struct CellsProp {
    pub path: String,
    pub name: String,
    pub value: u32,
}

/// A label defined in a preprocessed unit.
pub struct TuLabel {
    /// node path, for the completion detail
    pub path: String,
    /// file the label is defined in
    pub file: PathBuf,
    /// range of the label in that file
    pub range: Range,
}

/// An `&label` use in a preprocessed unit.
pub struct TuRef {
    pub label: String,
    pub file: PathBuf,
    pub range: Range,
}

/// Everything a unit tells us besides diagnostics.
pub struct TuData {
    pub labels: HashMap<String, TuLabel>,
    pub refs: Vec<TuRef>,
    pub defines: DefineMap,
    pub cells: Vec<CellsProp>,
}

/// Names defined anywhere in the unit: labels, `&label` references, effective
/// macros and `#*-cells` properties, with paths canonicalized across
/// `&label { }` fragments.
pub fn tu_data(unit: &TuResult, doc: &Document) -> TuData {
    let mut data = TuData {
        labels: HashMap::new(),
        refs: Vec::new(),
        defines: HashMap::new(),
        cells: Vec::new(),
    };
    for define in &unit.defines {
        let file = define
            .file
            .and_then(|index| unit.files.get(index as usize))
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        data.defines
            .entry(define.name.clone())
            .or_insert((define.value.clone(), file));
    }
    let mut tree = model::Tree::new(doc);
    let mut cursor = doc.tree.root_node().walk();
    collect_unit_names(unit, doc, &mut tree, &mut cursor, &mut data);
    data
}

fn collect_unit_names<'a>(
    unit: &TuResult,
    doc: &'a Document,
    tree: &mut model::Tree<'a>,
    cursor: &mut tree_sitter::TreeCursor<'a>,
    data: &mut TuData,
) {
    loop {
        let node = cursor.node();
        match node.kind() {
            "label" => {
                let text = model::node_text(doc, node);
                let name = text.trim_end_matches(':');
                if !name.is_empty()
                    && let Some((file, range)) = unit_range(unit, doc, node, name.len())
                {
                    let path = tree.display_path(node);
                    data.labels
                        .entry(name.to_string())
                        .or_insert(TuLabel { path, file, range });
                }
            }
            "label_ref" => {
                let text = model::node_text(doc, node);
                let name = text.trim_start_matches('&');
                if !name.is_empty()
                    && let Some((file, range)) = unit_range(unit, doc, node, text.len())
                {
                    data.refs.push(TuRef {
                        label: name.to_string(),
                        file,
                        range,
                    });
                }
            }
            "property" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = model::node_text(doc, name_node);
                    if name.starts_with('#') && name.ends_with("-cells") {
                        let owner = model::owning_node(node).unwrap_or(node);
                        let path = tree.display_path(owner);
                        if let Some(value) = model::property_u32(doc, node) {
                            data.cells.push(CellsProp {
                                path,
                                name: name.to_string(),
                                value,
                            });
                        }
                    }
                }
                // `&label` references live inside property values, so keep
                // descending (a property cannot contain another property)
            }
            _ => {}
        }
        if cursor.goto_first_child() {
            collect_unit_names(unit, doc, tree, cursor, data);
            cursor.goto_parent();
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn unit_range(
    unit: &TuResult,
    doc: &Document,
    node: Node,
    length: usize,
) -> Option<(PathBuf, Range)> {
    let position = model::byte_to_position(doc, node.start_byte());
    let (source, path) = unit.locate(position.line)?;
    Some((
        path.to_path_buf(),
        Range {
            start: Position {
                line: source.line,
                character: position.character,
            },
            end: Position {
                line: source.line,
                character: position.character + length as u32,
            },
        },
    ))
}

// ------------------------------------------------------- unit diagnostics

/// The per-file diagnostics of one unit run, plus its name tables.
pub struct TuOutput {
    pub per_file: HashMap<PathBuf, Vec<Diagnostic>>,
    pub data: TuData,
}

/// Structural diagnostics + phandle checks on a preprocessed unit, mapped back
/// to the files they came from.
pub fn tu_output(unit: &TuResult, doc: &Document) -> TuOutput {
    let data = tu_data(unit, doc);
    let mut per_file = group_diagnostics(unit, doc);
    let report = crate::phandle_cells::check_document(doc);
    // phandle and interrupt checks resolve labels from the unit's own text
    for problem in report.diagnostics {
        push_problem(unit, doc, &mut per_file, problem);
    }

    // `&label { }` blocks that extend nodes outside this unit: one diagnostic
    // per file, listing what could not be resolved
    let mut unresolved: HashMap<PathBuf, (Vec<String>, u32, u32)> = HashMap::new();
    for (start, end, label) in report.unresolved_fragments {
        let line = model::byte_to_position(doc, start as usize).line;
        let Some((_, path)) = unit.locate(line) else {
            continue;
        };
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let entry = unresolved.entry(path).or_insert_with(|| (Vec::new(), start, end));
        entry.0.push(label);
    }
    for (_path, (labels, start, end)) in unresolved {
        let mut names = labels;
        names.sort();
        names.dedup();
        let shown = names.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        let more = names.len().saturating_sub(5);
        let problem = crate::phandle_cells::Problem {
            start,
            end,
            error: true,
            message: format!(
                "{} override block(s) extend nodes that are not defined in this file's \
                 translation unit ({shown}{}); the board file that includes this one must \
                 provide them, so their contents are not checked",
                names.len(),
                if more > 0 { format!(", and {more} more") } else { String::new() }
            ),
        };
        push_problem(unit, doc, &mut per_file, problem);
    }

    TuOutput { per_file, data }
}

/// Map a byte-span problem back to the file it came from and publish it there.
fn push_problem(
    unit: &TuResult,
    doc: &Document,
    per_file: &mut HashMap<PathBuf, Vec<Diagnostic>>,
    problem: crate::phandle_cells::Problem,
) {
    let (start, end) = (problem.start as usize, problem.end as usize);
    let line = model::byte_to_position(doc, start).line;
    let Some((source, path)) = unit.locate(line) else {
        // the source map is missing this line: a preprocessor bug, never
        // silently drop the diagnostic
        let fallback = unit
            .files
            .first()
            .map(|file| std::fs::canonicalize(file).unwrap_or_else(|_| file.clone()))
            .unwrap_or_default();
        per_file.entry(fallback).or_default().push(Diagnostic {
            range: Range::default(),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("dtsls".to_string()),
            message: format!(
                "internal: cannot map a diagnostic at line {} back to a source file: {}",
                line + 1,
                problem.message
            ),
            ..Default::default()
        });
        return;
    };
    let start_position = model::byte_to_position(doc, start);
    let end_position = model::byte_to_position(doc, end);
    // the end position's line must be mapped through the source map too:
    // unit line numbers and source line numbers only coincide for the first
    // file in the unit, and an unmapped end would draw the squiggle to EOF
    let end_position = match unit.locate(end_position.line) {
        Some((end_source, _))
            if end_source.file == source.file && end_source.line >= source.line =>
        {
            Position {
                line: end_source.line,
                character: end_position.character,
            }
        }
        _ => Position {
            line: source.line,
            character: start_position.character + 1,
        },
    };
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    per_file.entry(path).or_default().push(Diagnostic {
        range: Range {
            start: Position {
                line: source.line,
                character: start_position.character,
            },
            end: end_position,
        },
        severity: Some(if problem.error {
            DiagnosticSeverity::ERROR
        } else {
            DiagnosticSeverity::WARNING
        }),
        source: Some("dtsls".to_string()),
        message: problem.message,
        ..Default::default()
    });
}

/// Diagnostics produced for a preprocessed unit, grouped by the file they
/// belong to (positions mapped back through the preprocessor line markers).
fn group_diagnostics(
    unit: &TuResult,
    doc: &Document,
) -> HashMap<PathBuf, Vec<Diagnostic>> {
    let mut out: HashMap<PathBuf, Vec<Diagnostic>> = HashMap::new();
    for diag in crate::diagnostics::structural(doc) {
        let Some((source, path)) = unit.locate(diag.range.start.line) else {
            continue;
        };
        let end = unit
            .locate(diag.range.end.line)
            .map(|(source, _)| source)
            .unwrap_or(source);
        let start = Position {
            line: source.line,
            character: diag.range.start.character,
        };
        let end = if end.file == source.file && end.line >= source.line {
            Position {
                line: end.line,
                character: diag.range.end.character,
            }
        } else {
            Position {
                line: start.line,
                character: start.character + 1,
            }
        };
        out.entry(path.to_path_buf())
            .or_default()
            .push(Diagnostic {
                range: Range { start, end },
                severity: diag.severity,
                source: diag.source,
                message: diag.message,
                ..Default::default()
            });
    }
    for error in &unit.errors {
        // an error without a file must still be shown, not dropped
        let path = error
            .file
            .and_then(|file| unit.files.get(file as usize).cloned())
            .or_else(|| unit.files.first().cloned());
        let Some(path) = path else {
            continue;
        };
        out.entry(path).or_default().push(Diagnostic {
            range: Range {
                start: Position {
                    line: error.line,
                    character: error.column,
                },
                end: Position {
                    line: error.line,
                    character: error.column + 1,
                },
            },
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("dtsls".to_string()),
            message: format!("preprocessor: {}", error.message),
            ..Default::default()
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn work_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dtsls-tu-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn preprocesses_includes_and_maps_lines() {
        let dir = work_dir("includes");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&root, "/dts-v1/;\n#include \"sub.dtsi\"\n/ {\n};\n");
        write(&sub, "/ { l: node {}; };\n");

        let overlays = BTreeMap::new();
        let result = preprocess(&TuInput {
            root: &root,
            include_dirs: &[dir.clone()],
            overlays: &overlays,
        });

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.text.contains("l: node"));
        let label_line = result
            .text
            .lines()
            .position(|line| line.contains("l: node"))
            .unwrap() as u32;
        let (source, path) = result.locate(label_line).unwrap();
        assert_eq!(path, std::fs::canonicalize(&sub).unwrap());
        assert_eq!(source.line, 0);
    }

    #[test]
    fn unsaved_buffers_override_disk() {
        let dir = work_dir("overlay");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&root, "/dts-v1/;\n#include \"sub.dtsi\"\n");
        write(&sub, "/ { from_disk {}; };\n");

        let mut overlays = BTreeMap::new();
        overlays.insert(
            std::fs::canonicalize(&sub).unwrap(),
            "/ { from_editor {}; };\n".to_string(),
        );
        let result = preprocess(&TuInput {
            root: &root,
            include_dirs: &[dir.clone()],
            overlays: &overlays,
        });
        assert!(result.text.contains("from_editor"), "{}", result.text);
        assert!(!result.text.contains("from_disk"));
    }

    #[test]
    fn preprocessor_decides_conditional_branches() {
        let dir = work_dir("ifdef");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&root, "/dts-v1/;\n#define KEEP_A\n#include \"sub.dtsi\"\n");
        write(&sub, "#ifdef KEEP_A\n/ { a {}; };\n#else\n/ { b {}; };\n#endif\n");

        let overlays = BTreeMap::new();
        let result = preprocess(&TuInput {
            root: &root,
            include_dirs: &[dir.clone()],
            overlays: &overlays,
        });
        assert!(result.text.contains(" a {"), "{}", result.text);
        assert!(!result.text.contains(" b {"), "{}", result.text);
    }

    #[test]
    fn missing_include_is_reported() {
        let dir = work_dir("missing");
        let root = dir.join("board.dts");
        write(&root, "/dts-v1/;\n#include \"nope.dtsi\"\n");
        let overlays = BTreeMap::new();
        let result = preprocess(&TuInput {
            root: &root,
            include_dirs: &[dir.clone()],
            overlays: &overlays,
        });
        assert!(!result.errors.is_empty(), "missing include not reported");
    }

    #[test]
    fn names_are_collected_with_fragment_merged_paths() {
        let dir = work_dir("names");
        let root = dir.join("board.dts");
        write(&root, "/dts-v1/;\n/ {\n\tintc: intc@0 { #clock-cells = <1>; };\n};\n&intc { dev { }; };\n");
        let overlays = BTreeMap::new();
        let unit = preprocess(&TuInput {
            root: &root,
            include_dirs: &[dir.clone()],
            overlays: &overlays,
        });
        let doc = Document::open(&unit.text, 0);
        let data = tu_data(&unit, &doc);
        assert_eq!(data.labels["intc"].path, "/intc@0");
        assert_eq!(data.cells.len(), 1);
        assert_eq!(data.cells[0].path, "/intc@0");
        assert_eq!(data.cells[0].value, 1);
    }

    #[test]
    fn phandle_ranges_map_back_to_the_source_file() {
        // the offending property lives in an included file, so unit line
        // numbers differ from source line numbers; an unmapped end position
        // would draw the squiggle from the cell to the end of the file
        let dir = work_dir("phandle-range");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&root, "#include \"sub.dtsi\"\n/ { };\n");
        write(
            &sub,
            "/dts-v1/;\n/ {\n\tclk: clk@1 { #clock-cells = <1>; };\n\tdev { clocks = <&clk 5 6>; };\n};\n",
        );
        let overlays = BTreeMap::new();
        let unit = preprocess(&TuInput {
            root: &root,
            include_dirs: &[dir.clone()],
            overlays: &overlays,
        });
        let doc = Document::open(&unit.text, 0);
        let output = tu_output(&unit, &doc);
        let diags = output
            .per_file
            .get(&std::fs::canonicalize(&sub).unwrap())
            .expect("the problem must be attributed to sub.dtsi");
        let problem = diags
            .iter()
            .find(|d| d.message.contains("should be a phandle"))
            .expect("surplus cell not reported");
        assert_eq!(
            problem.range.start.line, problem.range.end.line,
            "a single-line problem must not span to EOF: {:?}",
            problem.range
        );
    }
}
