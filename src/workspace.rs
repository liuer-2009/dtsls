// SPDX-License-Identifier: AGPL-3.0-only
//! Workspace state built on top of translation units.
//!
//! A translation unit already provides the per-file tables (labels, macros,
//! `#*-cells`), so this module keeps only what a unit cannot answer:
//!
//! * which `.dts` roots exist and which files each of them includes (a cheap
//!   line scan, no parsing), used to map a file to the boards that contain it;
//! * the last checked unit and preprocessed-text snapshot per root, so an
//!   edit reparses incrementally. Snapshots are capped (FIFO) and the contexts
//!   of open documents are never evicted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use walkdir::WalkDir;

use crate::document::Document;
use crate::unit::TuData;

/// How many board contexts keep their unit and preprocessed snapshot.
const MAX_CONTEXTS: usize = 8;

#[derive(Default)]
pub struct Workspace {
    /// every `.dts` found by the scan
    roots: RwLock<Vec<PathBuf>>,
    /// file -> files it includes (forward edges, for closure membership)
    includes: RwLock<HashMap<PathBuf, Vec<PathBuf>>>,
    /// included file -> files that include it
    includers: RwLock<HashMap<PathBuf, HashSet<PathBuf>>>,
    /// last checked unit per root
    units: RwLock<HashMap<PathBuf, Arc<TuData>>>,
    /// preprocessed text per root, kept so edits reparse incrementally
    snapshots: Mutex<HashMap<PathBuf, (Document, Vec<PathBuf>)>>,
    /// insertion order of the context keys, for FIFO eviction
    order: Mutex<Vec<PathBuf>>,
}

impl Workspace {
    /// Drop the least recently created contexts beyond [`MAX_CONTEXTS`],
    /// keeping the ones in `protected` (open documents and the active board)
    /// without counting them against the cap.
    pub fn prune(&self, protected: &HashSet<PathBuf>) {
        let mut order = self.order.lock().unwrap();
        let mut keep: Vec<PathBuf> = Vec::with_capacity(order.len());
        let mut contexts = 0usize;
        for path in order.drain(..).rev() {
            if protected.contains(&path) {
                keep.push(path);
            } else if contexts < MAX_CONTEXTS {
                contexts += 1;
                keep.push(path);
            } else {
                self.units.write().unwrap().remove(&path);
                self.snapshots.lock().unwrap().remove(&path);
                tracing::debug!(root = %path.display(), "evicted translation-unit context");
            }
        }
        keep.reverse();
        *order = keep;
    }

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
        let mut includes: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        let mut includers: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for slice in files.chunks(chunk) {
                handles.push(scope.spawn(move || {
                    let mut local_roots = Vec::new();
                    let mut local_includes: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
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
                            local_includes
                                .entry(path.clone())
                                .or_default()
                                .push(resolved.clone());
                            local_includers
                                .entry(resolved)
                                .or_default()
                                .insert(path.clone());
                        }
                    }
                    (local_roots, local_includes, local_includers)
                }));
            }
            for handle in handles {
                let (local_roots, local_includes, local_includers) =
                    handle.join().expect("scan worker");
                roots.extend(local_roots);
                for (file, children) in local_includes {
                    includes.entry(file).or_default().extend(children);
                }
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
        *self.includes.write().unwrap() = includes;
        *self.includers.write().unwrap() = includers;
    }

    /// Whether `file` is the board itself or reachable from it through
    /// `#include` lines (this is what decides "analyze in board context").
    pub fn closure_contains(&self, board: &Path, file: &Path) -> bool {
        let board = canonical(board);
        let file = canonical(file);
        if board == file {
            return true;
        }
        let includes = self.includes.read().unwrap();
        let mut seen: HashSet<PathBuf> = HashSet::from([board.clone()]);
        let mut queue = vec![board];
        while let Some(current) = queue.pop() {
            let Some(children) = includes.get(&current) else {
                continue;
            };
            for child in children {
                if *child == file {
                    return true;
                }
                if seen.insert(child.clone()) {
                    queue.push(child.clone());
                }
            }
        }
        false
    }

    /// Boards whose unit contains `path`. Falls back to the file itself so a
    /// lone `.dtsi` still gets its own closure checked.
    pub fn roots_for(&self, path: &Path) -> Vec<PathBuf> {
        let path = canonical(path);
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

    /// Last checked unit for a root.
    pub fn unit(&self, root: &Path) -> Option<Arc<TuData>> {
        self.units.read().unwrap().get(&canonical(root)).cloned()
    }

    pub fn store_unit(&self, root: PathBuf, data: Arc<TuData>) {
        let root = canonical(&root);
        self.units.write().unwrap().insert(root.clone(), data);
        let mut order = self.order.lock().unwrap();
        if !order.contains(&root) {
            order.push(root);
        }
    }

    /// Take the previous preprocessed snapshot so the unit run can reparse it
    /// incrementally instead of from scratch.
    pub fn take_snapshot(&self, root: &Path) -> Option<(Document, Vec<PathBuf>)> {
        self.snapshots.lock().unwrap().remove(&canonical(root))
    }

    pub fn put_snapshot(&self, root: PathBuf, doc: Document, files: Vec<PathBuf>) {
        let root = canonical(&root);
        self.snapshots
            .lock()
            .unwrap()
            .insert(root.clone(), (doc, files));
        let mut order = self.order.lock().unwrap();
        if !order.contains(&root) {
            order.push(root);
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    fn closure_membership_follows_includes() {
        let base = dir("closure");
        let board = base.join("board.dts");
        let soc = base.join("soc.dtsi");
        let deep = base.join("deep.dtsi");
        write(&soc, "#include \"deep.dtsi\"\n");
        write(&deep, "/ { };\n");
        write(&board, "#include \"soc.dtsi\"\n");
        let workspace = Workspace::default();
        workspace.scan(&[base.clone()], &[base.clone()]);
        assert!(workspace.closure_contains(&board, &deep));
        assert!(workspace.closure_contains(&board, &board));
        assert!(!workspace.closure_contains(&deep, &board));
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

    #[test]
    fn prune_keeps_protected_contexts_and_the_cap() {
        let workspace = Workspace::default();
        let roots: Vec<PathBuf> = (0..10)
            .map(|i| std::env::temp_dir().join(format!("dtsls-prune-{i}")))
            .collect();
        for root in &roots {
            workspace.store_unit(root.clone(), Arc::new(TuData {
                labels: Default::default(),
                refs: Vec::new(),
                defines: Default::default(),
                cells: Vec::new(),
            }));
        }
        // the oldest context is protected: it survives despite being evictable
        let protected: HashSet<PathBuf> = [roots[0].clone()].into_iter().collect();
        workspace.prune(&protected);
        assert!(workspace.unit(&roots[0]).is_some());
        // 0 is protected, 2..10 (newest 8) survive, 1 was evicted
        assert!(workspace.unit(&roots[1]).is_none());
        assert!(workspace.unit(&roots[2]).is_some());
        assert!(workspace.unit(&roots[9]).is_some());
    }
}
