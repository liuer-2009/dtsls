//! Translation units for diagnostics.
//!
//! Devicetree sources are compiled the same way the kernel does it: the
//! preprocessor expands `#include`, `#define` and `#ifdef`, then dtc parses
//! the result. Checks that span files (duplicate labels, duplicate properties,
//! duplicate node names, syntax) only make sense on that preprocessed text,
//! so we do what clangd does: analyse a file in the context of the root that
//! includes it, and map positions back through a source map.
//!
//! The expansion runs in-process (see [`crate::preproc`]) — no `cpp` process
//! per edit — and open editor buffers override the on-disk files, which is the
//! equivalent of clangd's VFS.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
}
