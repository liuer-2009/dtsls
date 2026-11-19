//! Workspace state built on top of translation units.
//!
//! The old workspace index parsed every file into per-file tables. That is
//! exactly what a translation unit already provides (after preprocessing), so
//! this module keeps only what a unit cannot answer:
//!
//! * which `.dts` roots exist and which files each of them includes (a cheap
//!   line scan, no parsing), used to map a file to the boards that contain it;
//! * the last checked unit per root (labels, references, macros, `#*-cells`);
//! * the preprocessed text snapshot per root for incremental reparsing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use tower_lsp::lsp_types::{Range, Url};
use walkdir::WalkDir;

use crate::document::Document;

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

#[derive(Default)]
pub struct Workspace {
    /// every `.dts` found by the scan
    roots: RwLock<Vec<PathBuf>>,
    /// included file -> files that include it
    includers: RwLock<HashMap<PathBuf, HashSet<PathBuf>>>,
    /// last checked unit per root
    units: RwLock<HashMap<PathBuf, Arc<TuData>>>,
    /// preprocessed text per root, kept so edits reparse incrementally
    snapshots: Mutex<HashMap<PathBuf, (Document, Vec<PathBuf>)>>,
}

impl Workspace {
    /// Discover `.dts` roots and the include graph. Only `#include` lines are
    /// read: no parsing, no macro expansion, so this stays a cheap scan.
    pub fn scan(&self, search_roots: &[PathBuf], include_dirs: &[PathBuf]) {
        let started = std::time::Instant::now();
        let mut files: Vec<PathBuf> = Vec::new();
        for root in search_roots {
            for entry in WalkDir::new(root)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || !PRUNED_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
                })
                .filter_map(|entry| entry.ok())
            {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                match path.extension().and_then(|ext| ext.to_str()) {
                    Some("dts") | Some("dtsi") => files.push(path.to_path_buf()),
                    _ => {}
                }
            }
        }

        let workers = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(4)
            .min(files.len().max(1));
        let chunk = files.len().div_ceil(workers).max(1);
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut includers: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for slice in files.chunks(chunk) {
                handles.push(scope.spawn(move || {
                    let mut local_roots = Vec::new();
                    let mut local_includers: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
                    for path in slice {
                        let Ok(text) = std::fs::read_to_string(path) else {
                            continue;
                        };
                        if path.extension().and_then(|ext| ext.to_str()) == Some("dts") {
                            local_roots.push(path.clone());
                        }
                        let directory = path.parent();
                        for (target, quoted) in include_lines(&text) {
                            let Some(resolved) =
                                resolve_include(directory, &target, quoted, include_dirs)
                            else {
                                continue;
                            };
                            let resolved =
                                std::fs::canonicalize(&resolved).unwrap_or(resolved);
                            local_includers
                                .entry(resolved)
                                .or_default()
                                .insert(path.clone());
                        }
                    }
                    (local_roots, local_includers)
                }));
            }
            for handle in handles {
                let (local_roots, local_includers) = handle.join().expect("scan worker");
                roots.extend(local_roots);
                for (file, set) in local_includers {
                    includers.entry(file).or_default().extend(set);
                }
            }
        });
        roots.sort();
        roots.dedup();
        tracing::info!(
            files = files.len(),
            roots = roots.len(),
            edges = includers.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "workspace scan"
        );
        *self.roots.write().unwrap() = roots;
        *self.includers.write().unwrap() = includers;
    }

    /// Boards whose unit contains `path`, preferring nothing here (the caller
    /// may sort). Falls back to the file itself so a lone `.dtsi` still gets
    /// its own closure checked.
    pub fn roots_for(&self, path: &Path) -> Vec<PathBuf> {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let includers = self.includers.read().unwrap();
        let is_root = |file: &Path| file.extension().and_then(|ext| ext.to_str()) == Some("dts");
        let mut roots = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::from([path.clone()]);
        let mut queue = vec![path.clone()];
        while let Some(current) = queue.pop() {
            let Some(parents) = includers.get(&current) else {
                continue;
            };
            for parent in parents {
                if !seen.insert(parent.clone()) {
                    continue;
                }
                if is_root(parent) {
                    roots.push(parent.clone());
                } else {
                    queue.push(parent.clone());
                }
            }
        }
        if roots.is_empty() {
            roots.push(path);
        }
        roots.sort();
        roots.dedup();
        roots.truncate(4);
        roots
    }

    pub fn unit_for_uri(&self, uri: &Url) -> Option<Arc<TuData>> {
        let path = uri.to_file_path().ok()?;
        self.unit(&path)
    }

    pub fn store_unit_uri(&self, uri: Url, data: Arc<TuData>) {
        if let Ok(path) = uri.to_file_path() {
            self.store_unit(path, data);
        }
    }

    pub fn take_snapshot_uri(&self, uri: &Url) -> Option<(Document, Vec<PathBuf>)> {
        let path = uri.to_file_path().ok()?;
        self.take_snapshot(&path)
    }

    pub fn put_snapshot_uri(&self, uri: Url, doc: Document, files: Vec<PathBuf>) {
        if let Ok(path) = uri.to_file_path() {
            self.put_snapshot(path, doc, files);
        }
    }

    pub fn unit(&self, root: &Path) -> Option<Arc<TuData>> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        self.units.read().unwrap().get(&root).cloned()
    }

    pub fn store_unit(&self, root: PathBuf, data: Arc<TuData>) {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        self.units.write().unwrap().insert(root, data);
    }

    /// Take the previous preprocessed snapshot so the unit run can reparse it
    /// incrementally instead of from scratch.
    pub fn take_snapshot(&self, root: &Path) -> Option<(Document, Vec<PathBuf>)> {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        self.snapshots.lock().unwrap().remove(&root)
    }

    pub fn put_snapshot(&self, root: PathBuf, doc: Document, files: Vec<PathBuf>) {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        self.snapshots
            .lock()
            .unwrap()
            .insert(root, (doc, files));
    }
}

/// `#include "x"` / `#include <x>` lines of a source file.
fn include_lines(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("#include") else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix('"')
            && let Some(end) = rest.find('"')
        {
            out.push((rest[..end].to_string(), true));
        } else if let Some(rest) = rest.strip_prefix('<')
            && let Some(end) = rest.find('>')
        {
            out.push((rest[..end].to_string(), false));
        }
    }
    out
}

/// Resolve an include target to an existing file. Quoted includes try the
/// including file's directory first, then the include dirs; angle-bracket
/// includes only use the include dirs.
pub fn resolve_include(
    file_dir: Option<&Path>,
    target: &str,
    quoted: bool,
    include_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if quoted
        && let Some(dir) = file_dir
    {
        let candidate = dir.join(target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for dir in include_dirs {
        let candidate = dir.join(target);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Directories never worth walking for dts sources.
const PRUNED_DIRS: &[&str] = &[
    ".git",
    "Documentation",
    "tools",
    "samples",
    "include",
    "scripts",
    "rust",
    "certs",
    "virt",
];

/// Auto-detect the kernel tree root by walking up from each root.
pub fn detect_kernel_root(roots: &[PathBuf]) -> Option<PathBuf> {
    for root in roots {
        let mut dir = Some(root.as_path());
        for _ in 0..8 {
            let Some(current) = dir else { break };
            let has_headers = current.join("include/dt-bindings").is_dir();
            let has_bindings = current.join("Documentation/devicetree/bindings").is_dir()
                && current.join("scripts/dtc").is_dir();
            if has_headers || has_bindings {
                return Some(current.to_path_buf());
            }
            dir = current.parent();
        }
    }
    None
}

/// Include dirs for `<dt-bindings/...>` resolution.
pub fn detect_include_dirs(roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(kernel_root) = detect_kernel_root(roots) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let include = kernel_root.join("include");
    if include.is_dir() {
        out.push(include);
    }
    let prefixes = kernel_root.join("scripts/dtc/include-prefixes");
    if prefixes.is_dir() {
        out.push(prefixes);
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

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dtsls-ws-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_finds_roots_and_include_edges() {
        let base = dir("scan");
        let board = base.join("board.dts");
        let soc = base.join("soc.dtsi");
        let sub = base.join("sub.dtsi");
        write(&soc, "#include \"sub.dtsi\"\n/ { };\n");
        write(&sub, "/ { };\n");
        write(&board, "#include \"soc.dtsi\"\n/ { };\n");

        let workspace = Workspace::default();
        workspace.scan(&[base.clone()], &[base.clone()]);
        assert!(!workspace.roots_for(&base.join("missing.dtsi")).is_empty());

        let roots = workspace.roots_for(&sub);
        assert_eq!(roots, vec![std::fs::canonicalize(&board).unwrap()]);
        assert_eq!(
            workspace.roots_for(&std::fs::canonicalize(&board).unwrap()),
            vec![std::fs::canonicalize(&board).unwrap()]
        );
    }

    #[test]
    fn standalone_file_is_its_own_root() {
        let base = dir("standalone");
        let header = base.join("h.dtsi");
        write(&header, "/ { };\n");
        let workspace = Workspace::default();
        workspace.scan(&[base.clone()], &[base]);
        assert_eq!(
            workspace.roots_for(&header),
            vec![std::fs::canonicalize(&header).unwrap()]
        );
    }

    #[test]
    fn include_lines_are_parsed_conservatively() {
        let lines = include_lines(
            "#include \"a.dtsi\"\n  #include <dt-bindings/x.h>\n// #include \"b.dtsi\"\n#define X\n",
        );
        assert_eq!(lines, vec![("a.dtsi".into(), true), ("dt-bindings/x.h".into(), false)]);
    }
}
