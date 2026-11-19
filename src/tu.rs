//! Translation units for diagnostics.
//!
//! Devicetree sources are compiled the same way the kernel does it: the C
//! preprocessor expands `#include`, `#define` and `#ifdef`, then dtc parses
//! the result. Checks that span files (duplicate labels, duplicate properties,
//! duplicate node names, syntax) only make sense on that preprocessed text,
//! so we do what clangd does: analyse a file in the context of the root that
//! includes it, and map positions back through the preprocessor's line markers.
//!
//! Unsaved editor buffers participate through a symlink mirror of the include
//! closure (clangd's VFS equivalent): open files are written as regular files,
//! everything else is linked to its real path.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

pub struct TuResult {
    pub files: Vec<PathBuf>,
    /// preprocessed source with the line markers removed
    pub text: String,
    /// one entry per line of `text`
    pub lines: Vec<SourceRef>,
    /// preprocessor diagnostics (missing includes, bad directives, ...)
    pub errors: Vec<TuError>,
}

impl TuResult {
    pub fn locate(&self, line: u32) -> Option<(SourceRef, &Path)> {
        let source = *self.lines.get(line as usize)?;
        Some((source, &self.files[source.file as usize]))
    }
}

/// Stable id for a root's mirror/work directory.
pub fn hash_path(path: &Path) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

pub struct TuInput<'a> {
    pub root: &'a Path,
    pub include_dirs: &'a [PathBuf],
    /// unsaved editor buffers, keyed by absolute path
    pub overlays: &'a BTreeMap<PathBuf, String>,
}

/// Preprocess `root` with its include closure. `None` means the preprocessor
/// could not be started at all; errors inside the sources come back in
/// [`TuResult::errors`].
pub fn preprocess(input: &TuInput, work_dir: &Path) -> Option<TuResult> {
    let deps = cpp_dependencies(input.root, input.include_dirs)?;
    let mirror = work_dir.join("mirror");
    let _ = std::fs::remove_dir_all(&mirror);
    std::fs::create_dir_all(&mirror).ok()?;
    for file in &deps {
        if !mirror_file(&mirror, file, input.overlays) {
            return None;
        }
    }
    if !mirror_file(&mirror, input.root, input.overlays) {
        return None;
    }

    let mirror_root = mirror_path(&mirror, input.root);
    let mut command = cpp_command(&mirror_root, input.include_dirs);
    // mirrored include dirs first so overlays win, then the real tree
    for dir in input.include_dirs {
        let mirrored = mirror_path(&mirror, dir);
        if mirrored.is_dir() {
            command.arg("-I").arg(mirrored);
        }
        command.arg("-I").arg(dir);
    }
    let output = command.output().ok()?;
    let mut result = parse_output(&output.stdout, &mirror, input.root);
    result.errors = parse_stderr(&output.stderr, &mirror, &result.files);
    Some(result)
}

/// Include closure of `root`, as absolute paths (the root itself included).
fn cpp_dependencies(root: &Path, include_dirs: &[PathBuf]) -> Option<Vec<PathBuf>> {
    let mut command = cpp_command(root, include_dirs);
    command.arg("-M");
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut deps = Vec::new();
    // `-M` prints one dependency rule, possibly with `\` continuations
    let joined = text.replace("\\\n", " ");
    if let Some((_, rest)) = joined.split_once(':') {
        for token in rest.split_whitespace() {
            let path = PathBuf::from(token.replace("\\ ", " "));
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            if path.is_file() && !deps.contains(&path) {
                deps.push(path);
            }
        }
    }
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if !deps.contains(&root) {
        deps.insert(0, root);
    }
    Some(deps)
}

fn cpp_command(root: &Path, _include_dirs: &[PathBuf]) -> Command {
    let mut command = Command::new("cpp");
    command
        .args(["-nostdinc", "-undef", "-D__DTS__", "-x", "assembler-with-cpp"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Mirror one real file into the shadow tree: overlays become regular files,
/// everything else a symlink to the real path.
fn mirror_file(mirror: &Path, file: &Path, overlays: &BTreeMap<PathBuf, String>) -> bool {
    let target = mirror_path(mirror, file);
    if let Some(parent) = target.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    let _ = std::fs::remove_file(&target);
    if let Some(text) = overlays.get(file) {
        let Ok(mut out) = std::fs::File::create(&target) else {
            return false;
        };
        if out.write_all(text.as_bytes()).is_err() {
            return false;
        }
        return true;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(file, &target).is_ok()
    }
    #[cfg(not(unix))]
    {
        std::fs::copy(file, &target).is_ok()
    }
}

fn mirror_path(mirror: &Path, path: &Path) -> PathBuf {
    let relative = path.strip_prefix("/").unwrap_or(path);
    mirror.join(relative)
}

/// Strip `# N "file"` markers out of the preprocessed output while recording,
/// for every remaining line, where it came from.
fn parse_output(stdout: &[u8], mirror: &Path, root: &Path) -> TuResult {
    let text = String::from_utf8_lossy(stdout);
    let mut files: Vec<PathBuf> = Vec::new();
    let mut lines: Vec<SourceRef> = Vec::new();
    let mut body = String::with_capacity(text.len());
    let mut current = SourceRef { file: 0, line: 0 };
    files.push(root.to_path_buf());

    for raw in text.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\n', '\r']);
        if let Some(marker) = parse_marker(line) {
            current.file = intern_file(&mut files, &marker.0, mirror);
            current.line = marker.1;
            continue;
        }
        if line.is_empty() && raw == "\n" {
            // blank lines keep the mapping but add nothing meaningful
        }
        body.push_str(raw);
        lines.push(current);
        current.line += 1;
    }
    TuResult {
        files,
        text: body,
        lines,
        errors: Vec::new(),
    }
}

/// `# 42 "path"` -> (path, 41)
fn parse_marker(line: &str) -> Option<(String, u32)> {
    let rest = line.strip_prefix("# ")?;
    let (number, rest) = rest.split_once(' ')?;
    let number: u32 = number.trim().parse().ok()?;
    let rest = rest.trim();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let path = rest[..end].replace("\\\"", "\"");
    Some((path, number.saturating_sub(1)))
}

fn intern_file(files: &mut Vec<PathBuf>, path: &str, mirror: &Path) -> u32 {
    let real = if let Some(rest) = path.strip_prefix(&mirror.to_string_lossy().into_owned()) {
        PathBuf::from(rest)
    } else {
        PathBuf::from(path)
    };
    // `<built-in>` / `<command-line>` have no source we can point at
    if real.to_string_lossy().starts_with('<') {
        return 0;
    }
    if let Some(index) = files.iter().position(|file| file == &real) {
        return index as u32;
    }
    files.push(real);
    (files.len() - 1) as u32
}

/// gcc-style diagnostics: `path:line:col: error: message`.
fn parse_stderr(stderr: &[u8], mirror: &Path, files: &[PathBuf]) -> Vec<TuError> {
    let text = String::from_utf8_lossy(stderr);
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((location, message)) = line.split_once(": ") else {
            continue;
        };
        let mut parts = location.rsplitn(3, ':');
        let Some(column) = parts.next().and_then(|part| part.trim().parse::<u32>().ok()) else {
            continue;
        };
        let Some(line_number) = parts.next().and_then(|part| part.trim().parse::<u32>().ok()) else {
            continue;
        };
        let Some(path) = parts.next() else { continue };
        let real = if let Some(rest) = path.strip_prefix(&mirror.to_string_lossy().into_owned()) {
            PathBuf::from(rest)
        } else {
            PathBuf::from(path)
        };
        let file = files.iter().position(|candidate| candidate == &real);
        out.push(TuError {
            file: file.map(|index| index as u32),
            line: line_number.saturating_sub(1),
            column: column.saturating_sub(1),
            message: message.to_string(),
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
        if Command::new("cpp").arg("--version").output().is_err() {
            return; // no preprocessor on this host
        }
        let dir = work_dir("includes");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&root, "/dts-v1/;\n#include \"sub.dtsi\"\n/ {\n};\n");
        write(&sub, "/ { l: node {}; };\n");

        let overlays = BTreeMap::new();
        let result = preprocess(
            &TuInput {
                root: &root,
                include_dirs: &[dir.clone()],
                overlays: &overlays,
            },
            &dir.join("work"),
        )
        .expect("preprocessor failed to start");

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.text.contains("l: node"));
        // the label line must be attributed to sub.dtsi
        let label_line = result
            .text
            .lines()
            .position(|line| line.contains("l: node"))
            .unwrap() as u32;
        let (source, path) = result.locate(label_line).unwrap();
        assert_eq!(path, sub);
        assert_eq!(source.line, 0);
        // the root's own lines map back to the root
        assert!(result.files.contains(&root));
    }

    #[test]
    fn unsaved_buffers_override_disk() {
        if Command::new("cpp").arg("--version").output().is_err() {
            return;
        }
        let dir = work_dir("overlay");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&root, "/dts-v1/;\n#include \"sub.dtsi\"\n");
        write(&sub, "/ { from_disk {}; };\n");

        let mut overlays = BTreeMap::new();
        overlays.insert(sub.clone(), "/ { from_editor {}; };\n".to_string());
        let result = preprocess(
            &TuInput {
                root: &root,
                include_dirs: &[dir.clone()],
                overlays: &overlays,
            },
            &dir.join("work"),
        )
        .expect("preprocessor failed to start");
        assert!(result.text.contains("from_editor"), "{}", result.text);
        assert!(!result.text.contains("from_disk"));
    }

    #[test]
    fn preprocessor_decides_conditional_branches() {
        if Command::new("cpp").arg("--version").output().is_err() {
            return;
        }
        let dir = work_dir("ifdef");
        let root = dir.join("board.dts");
        let sub = dir.join("sub.dtsi");
        write(&root, "/dts-v1/;\n#define KEEP_A\n#include \"sub.dtsi\"\n");
        write(&sub, "#ifdef KEEP_A\n/ { a {}; };\n#else\n/ { b {}; };\n#endif\n");

        let overlays = BTreeMap::new();
        let result = preprocess(
            &TuInput {
                root: &root,
                include_dirs: &[dir.clone()],
                overlays: &overlays,
            },
            &dir.join("work"),
        )
        .expect("preprocessor failed to start");
        assert!(result.text.contains(" a {"), "{}", result.text);
        assert!(!result.text.contains(" b {"), "{}", result.text);
    }

    #[test]
    fn missing_include_is_reported() {
        if Command::new("cpp").arg("--version").output().is_err() {
            return;
        }
        let dir = work_dir("missing");
        let root = dir.join("board.dts");
        write(&root, "/dts-v1/;\n#include \"nope.dtsi\"\n");
        let overlays = BTreeMap::new();
        let result = preprocess(
            &TuInput {
                root: &root,
                include_dirs: &[dir.clone()],
                overlays: &overlays,
            },
            &dir.join("work"),
        )
        .expect("preprocessor failed to start");
        assert!(!result.errors.is_empty(), "missing include not reported");
    }
}
