use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::{Arc, RwLock};

use tower_lsp::lsp_types::Url;
use yaml_rust2::{Yaml, YamlLoader};

/// Compound compatible pattern: per position, the allowed values.
type CompoundPattern = Vec<Vec<String>>;

/// Compound patterns bucketed by their first compatible value.
type CompoundBucket = HashMap<String, Vec<(CompoundPattern, Arc<Binding>)>>;

/// The subset of a devicetree binding this server validates against.
pub struct Binding {
    pub uri: Url,
    /// compatible strings this binding matches anywhere in the node's list
    pub singles: Vec<String>,
    /// compound compatibles (`items:`): per position, the allowed values
    pub compounds: Vec<CompoundPattern>,
    /// property names allowed on matching nodes (own + `$ref`-resolved)
    pub properties: HashSet<String>,
    /// required property names
    pub required: Vec<String>,
    /// `additionalProperties: false` / `unevaluatedProperties: false`
    pub strict: bool,
    /// every `$ref` resolved locally; unknown-property checks are only safe
    /// when the allowed set is complete
    pub refs_complete: bool,
    /// compiled `patternProperties` matchers (e.g. `.*-supply$`)
    pub patterns: Vec<regex::Regex>,
}

/// compatible string -> binding. First file wins (sorted scan order), so
/// lookups are deterministic.
#[derive(Default)]
pub struct BindingIndex {
    by_single: RwLock<HashMap<String, Arc<Binding>>>,
    /// first compatible string -> compound patterns
    by_compound: RwLock<CompoundBucket>,
    count: RwLock<usize>,
    /// meta-schema properties every node may carry (`pinctrl-*`, ...)
    implicit_properties: RwLock<HashSet<String>>,
    /// meta-schema property patterns (`-supply$`, `.*-names$`, ...)
    implicit_patterns: RwLock<Vec<regex::Regex>>,
}

impl BindingIndex {
    /// May the node carry this property according to the binding and the
    /// meta-schemas?
    pub fn is_property_allowed(&self, name: &str, binding: &Binding) -> bool {
        if is_common_property(name) || binding.properties.contains(name) {
            return true;
        }
        if self
            .implicit_properties
            .read()
            .unwrap()
            .contains(name)
        {
            return true;
        }
        if binding.patterns.iter().any(|re| re.is_match(name)) {
            return true;
        }
        self.implicit_patterns
            .read()
            .unwrap()
            .iter()
            .any(|re| re.is_match(name))
    }

    fn set_implicit(&self, properties: HashSet<String>, patterns: Vec<regex::Regex>) {
        *self.implicit_properties.write().unwrap() = properties;
        *self.implicit_patterns.write().unwrap() = patterns;
    }

    /// Match a node's full compatible list: singles match if the list
    /// contains them; compound patterns match position by position.
    pub fn lookup(&self, compatible_list: &[String]) -> Option<Arc<Binding>> {
        {
            let singles = self.by_single.read().unwrap();
            for compatible in compatible_list {
                if let Some(binding) = singles.get(compatible) {
                    return Some(binding.clone());
                }
            }
        }
        let first = compatible_list.first()?;
        let compounds = self.by_compound.read().unwrap();
        for (pattern, binding) in compounds.get(first)? {
            if pattern.len() != compatible_list.len() {
                continue;
            }
            let matches = pattern
                .iter()
                .zip(compatible_list)
                .all(|(allowed, actual)| allowed.contains(actual));
            if matches {
                return Some(binding.clone());
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        *self.count.read().unwrap() == 0
    }

    pub fn len(&self) -> usize {
        *self.count.read().unwrap()
    }

    /// Test-only: insert without the scan.
    #[cfg(test)]
    pub fn insert_for_test(&self, binding: Binding) {
        self.insert(binding);
    }

    fn insert(&self, binding: Binding) {
        let shared = Arc::new(binding.clone_shallow());
        {
            let mut singles = self.by_single.write().unwrap();
            for compatible in &binding.singles {
                singles
                    .entry(compatible.clone())
                    .or_insert_with(|| shared.clone());
            }
        }
        {
            let mut compounds = self.by_compound.write().unwrap();
            for pattern in &binding.compounds {
                // bucket under EVERY allowed first-position value, otherwise
                // a node whose first string is not the first enum entry misses
                if let Some(allowed) = pattern.first() {
                    for first in allowed {
                        compounds
                            .entry(first.clone())
                            .or_default()
                            .push((pattern.clone(), shared.clone()));
                    }
                }
            }
        }
        *self.count.write().unwrap() += 1;
    }
}

impl Binding {
    fn clone_shallow(&self) -> Binding {
        Binding {
            uri: self.uri.clone(),
            singles: self.singles.clone(),
            compounds: self.compounds.clone(),
            properties: self.properties.clone(),
            required: self.required.clone(),
            strict: self.strict,
            refs_complete: self.refs_complete,
            patterns: self.patterns.clone(),
        }
    }
}

/// Properties that any binding implicitly allows.
const COMMON_PROPERTIES: &[&str] = &[
    "compatible",
    "status",
    "label",
    "phandle",
    "linux,phandle",
    "device_type",
    "bootargs",
    "#address-cells",
    "#size-cells",
];

pub fn is_common_property(name: &str) -> bool {
    COMMON_PROPERTIES.contains(&name)
}

/// Scan the kernel bindings tree plus the dtschema package (downloaded on
/// first run). Runs on a worker thread at startup.
pub fn scan(index: &BindingIndex, dir: &Path) {
    let started = std::time::Instant::now();
    let repo = ensure_dtschema_repo();
    if let Some(repo) = &repo {
        let mut properties = HashSet::new();
        let mut patterns = Vec::new();
        collect_meta_allowlist(&repo.join("meta-schemas"), &mut properties, &mut patterns);
        tracing::info!(
            properties = properties.len(),
            patterns = patterns.len(),
            "meta-schema allowlist"
        );
        index.set_implicit(properties, patterns);
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let mut scan_dirs = vec![dir.to_path_buf()];
    if let Some(repo) = &repo {
        scan_dirs.push(repo.join("schemas"));
    }
    for scan_dir in &scan_dirs {
        for entry in walkdir::WalkDir::new(scan_dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            match path.extension().and_then(|e| e.to_str()) {
                Some("yaml") | Some("yml") => files.push(path.to_path_buf()),
                _ => {}
            }
        }
    }
    files.sort();

    if files.is_empty() {
        return;
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len());
    let chunk_size = files.len().div_ceil(workers).max(1);
    let failed: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for chunk in files.chunks(chunk_size) {
            let index = &index;
            let failed = &failed;
            scope.spawn(move || {
                for path in chunk {
                    let bindings = parse_bindings(path);
                    if bindings.is_empty() {
                        let mut guard = failed.lock().unwrap();
                        if guard.len() < 20 {
                            guard.push(path.display().to_string());
                        }
                    } else {
                        for binding in bindings {
                            index.insert(binding);
                        }
                    }
                }
            });
        }
    });
    let failed = failed.into_inner().unwrap();
    tracing::info!(
        files = files.len(),
        parsed = files.len().saturating_sub(failed.len()),
        compatibles = index.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        sample_failures = ?failed,
        "binding index scan"
    );
}

/// Parse one binding file into one or more bindings: the file's own node
/// type plus every child-node schema declared under `patternProperties`
/// (each child schema has its own properties/required — merging them, as an
/// earlier version did, makes e.g. fastrpc's top-level requirements apply to
/// its compute-cb children).
pub fn parse_bindings(path: &Path) -> Vec<Binding> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(docs) = YamlLoader::load_from_str(&strip_yaml_directives(&text)) else {
        return Vec::new();
    };
    let Some(doc) = docs.first() else {
        return Vec::new();
    };
    let Ok(uri) = Url::from_file_path(path) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    // The file's own node type: compatible from top-level `properties.
    // compatible` (or allOf/oneOf branches holding one).
    let mut singles = Vec::new();
    let mut compounds = Vec::new();
    if let Some(compatible) =
        yaml_get(doc, "properties").and_then(|props| yaml_get(props, "compatible"))
    {
        parse_compatible(compatible, &mut singles, &mut compounds);
    }
    collect_branch_compatibles(doc, &mut singles, &mut compounds, 0);
    singles.sort();
    singles.dedup();
    if !(singles.is_empty() && compounds.is_empty()) {
        out.push(build_binding(&uri, path, doc, singles, compounds));
    }

    // Child-node schemas: patternProperties values with their own compatible.
    collect_child_schemas(doc, 0, &mut |schema| {
        let mut child_singles = Vec::new();
        let mut child_compounds = Vec::new();
        if let Some(compatible) =
            yaml_get(schema, "properties").and_then(|props| yaml_get(props, "compatible"))
        {
            parse_compatible(compatible, &mut child_singles, &mut child_compounds);
            child_singles.sort();
            child_singles.dedup();
            out.push(build_binding(
                &uri,
                path,
                schema,
                child_singles,
                child_compounds,
            ));
        }
    });
    out
}

fn build_binding(
    uri: &Url,
    path: &Path,
    schema: &Yaml,
    singles: Vec<String>,
    compounds: Vec<CompoundPattern>,
) -> Binding {
    let mut properties: HashSet<String> = HashSet::new();
    if let Some(props) = yaml_get(schema, "properties").and_then(Yaml::as_hash) {
        for key in props.keys() {
            if let Some(name) = key.as_str() {
                properties.insert(name.to_string());
            }
        }
    }
    let mut patterns = Vec::new();
    if let Some(pattern_props) = yaml_get(schema, "patternProperties").and_then(Yaml::as_hash) {
        for (key, child) in pattern_props {
            if let Some(pattern) = key.as_str()
                && let Some(re) = compile_pattern(pattern)
            {
                patterns.push(re);
            }
            // properties declared inside a pattern are allowed for matching
            // nodes (permissive: avoids false positives from incomplete regex)
            if let Some(props) = yaml_get(child, "properties").and_then(Yaml::as_hash) {
                for prop_key in props.keys() {
                    if let Some(name) = prop_key.as_str() {
                        properties.insert(name.to_string());
                    }
                }
            }
        }
    }

    // pull in properties from `$ref`'d schemas (base.yaml, types.yaml, ...)
    let refs_complete = resolve_refs(
        schema,
        path,
        path.parent(),
        &mut properties,
        &mut HashSet::new(),
        0,
    );

    let mut required = Vec::new();
    if let Some(list) = yaml_get(schema, "required").and_then(Yaml::as_vec) {
        for item in list {
            if let Some(name) = item.as_str() {
                required.push(name.to_string());
            }
        }
    }

    let strict = matches!(
        yaml_get(schema, "additionalProperties"),
        Some(Yaml::Boolean(false))
    ) || matches!(
        yaml_get(schema, "unevaluatedProperties"),
        Some(Yaml::Boolean(false))
    );

    Binding {
        uri: uri.clone(),
        singles,
        compounds,
        properties,
        required,
        strict,
        refs_complete,
        patterns,
    }
}

/// Compatibles declared in top-level allOf/oneOf/anyOf branches (some
/// bindings put their compatible there). `if:`/`definitions`/`examples` are
/// deliberately skipped: their compatibles are conditional or illustrative.
fn collect_branch_compatibles(
    node: &Yaml,
    singles: &mut Vec<String>,
    compounds: &mut Vec<CompoundPattern>,
    depth: usize,
) {
    if depth > 3 {
        return;
    }
    let Some(hash) = node.as_hash() else { return };
    for branch_key in ["allOf", "oneOf", "anyOf"] {
        if let Some(branches) = hash
            .get(&Yaml::String(branch_key.to_string()))
            .and_then(Yaml::as_vec)
        {
            for branch in branches {
                if let Some(compatible) =
                    yaml_get(branch, "properties").and_then(|props| yaml_get(props, "compatible"))
                {
                    parse_compatible(compatible, singles, compounds);
                }
                collect_branch_compatibles(branch, singles, compounds, depth + 1);
            }
        }
    }
}

/// Visit every child-node schema: patternProperties values reachable from
/// the top level (directly or through allOf/oneOf/anyOf branches).
fn collect_child_schemas(node: &Yaml, depth: usize, visit: &mut impl FnMut(&Yaml)) {
    if depth > 3 {
        return;
    }
    let Some(hash) = node.as_hash() else { return };
    if let Some(pattern_props) = hash
        .get(&Yaml::String("patternProperties".to_string()))
        .and_then(Yaml::as_hash)
    {
        for child in pattern_props.values() {
            visit(child);
        }
    }
    for branch_key in ["allOf", "oneOf", "anyOf"] {
        if let Some(branches) = hash
            .get(&Yaml::String(branch_key.to_string()))
            .and_then(Yaml::as_vec)
        {
            for branch in branches {
                collect_child_schemas(branch, depth + 1, visit);
            }
        }
    }
}

fn yaml_get<'a>(doc: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    doc.as_hash()?.get(&Yaml::String(key.to_string()))
}

/// yaml-rust2 chokes on `%YAML 1.2` directives; they carry no information for
/// us, so drop them before parsing.
fn strip_yaml_directives(text: &str) -> String {
    text.lines()
        .filter(|line| !line.starts_with('%'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a `properties.compatible` schema into single-string matches and
/// compound (`items:`) position patterns. Compound parts are *not* registered
/// as standalone compatibles — a node's `"simple-bus"` must not match
/// `fsl,imx28-pinctrl` + `simple-bus` binding just because the second item
/// happens to be `simple-bus`.
fn parse_compatible(
    node: &Yaml,
    singles: &mut Vec<String>,
    compounds: &mut Vec<CompoundPattern>,
) {
    match node {
        Yaml::String(s) => singles.push(s.clone()),
        Yaml::Hash(hash) => {
            for (key, value) in hash {
                match key.as_str() {
                    Some("const") => {
                        if let Some(s) = value.as_str() {
                            singles.push(s.to_string());
                        }
                    }
                    Some("enum") => {
                        if let Some(items) = value.as_vec() {
                            for item in items {
                                if let Some(s) = item.as_str() {
                                    singles.push(s.to_string());
                                }
                            }
                        }
                    }
                    Some("items") => {
                        if let Some(items) = value.as_vec() {
                            let mut pattern = Vec::new();
                            for item in items {
                                let mut allowed = Vec::new();
                                collect_value_strings(item, &mut allowed);
                                if allowed.is_empty() {
                                    pattern.clear();
                                    break;
                                }
                                pattern.push(allowed);
                            }
                            if !pattern.is_empty() {
                                compounds.push(pattern);
                            }
                        }
                    }
                    Some("oneOf") | Some("anyOf") | Some("allOf") => {
                        if let Some(branches) = value.as_vec() {
                            for branch in branches {
                                parse_compatible(branch, singles, compounds);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Yaml::Array(items) => {
            for item in items {
                parse_compatible(item, singles, compounds);
            }
        }
        _ => {}
    }
}

fn collect_value_strings(node: &Yaml, out: &mut Vec<String>) {
    match node {
        Yaml::String(s) => out.push(s.clone()),
        Yaml::Hash(hash) => {
            for (key, value) in hash {
                match key.as_str() {
                    Some("const") | Some("enum") => collect_value_strings(value, out),
                    Some("oneOf") | Some("anyOf") | Some("allOf") => {
                        if let Some(branches) = value.as_vec() {
                            for branch in branches {
                                collect_value_strings(branch, out);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Yaml::Array(items) => {
            for item in items {
                collect_value_strings(item, out);
            }
        }
        _ => {}
    }
}

fn resolve_refs(
    doc: &Yaml,
    binding_path: &Path,
    binding_dir: Option<&Path>,
    properties: &mut HashSet<String>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> bool {
    if depth > 6 {
        return true;
    }
    let mut refs = Vec::new();
    collect_refs(doc, &mut refs);
    let mut complete = true;
    for reference in refs {
        let Some(target) = resolve_ref_path(&reference, binding_path, binding_dir) else {
            complete = false;
            continue;
        };
        if !visited.insert(target.clone()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&target) else {
            complete = false;
            continue;
        };
        let Ok(docs) = YamlLoader::load_from_str(&strip_yaml_directives(&text)) else {
            complete = false;
            continue;
        };
        let Some(ref_doc) = docs.first() else {
            complete = false;
            continue;
        };
        if let Some(props) = yaml_get(ref_doc, "properties").and_then(Yaml::as_hash) {
            for key in props.keys() {
                if let Some(name) = key.as_str() {
                    properties.insert(name.to_string());
                }
            }
        }
        if !resolve_refs(
            ref_doc,
            &target,
            target.parent(),
            properties,
            visited,
            depth + 1,
        ) {
            complete = false;
        }
    }
    complete
}

fn collect_refs(node: &Yaml, out: &mut Vec<String>) {
    match node {
        Yaml::Hash(hash) => {
            for (key, value) in hash {
                if key.as_str() == Some("$ref") {
                    if let Some(s) = value.as_str() {
                        out.push(s.to_string());
                    }
                } else {
                    collect_refs(value, out);
                }
            }
        }
        Yaml::Array(items) => {
            for item in items {
                collect_refs(item, out);
            }
        }
        _ => {}
    }
}

/// Resolve a `$ref` target to a readable file.
///
/// - `#/definitions/x` — same file, already parsed: treat as resolved.
/// - `base.yaml#`      — relative to the referencing binding.
/// - `/schemas/x.yaml` — the dtschema package: in-tree copy if present, then a
///   local `dtschema` python install, then downloaded into the user cache so
///   the allowed-property set is complete even without a dtschema checkout.
fn resolve_ref_path(
    reference: &str,
    binding_path: &Path,
    binding_dir: Option<&Path>,
) -> Option<PathBuf> {
    let clean = reference.split('#').next().unwrap_or(reference);
    if clean.is_empty() {
        return Some(binding_path.to_path_buf());
    }
    if clean.starts_with('/') {
        let rel = clean.trim_start_matches('/');
        if let Some(root) = in_tree_root(binding_dir) {
            let candidate = root.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if let Some(rest) = rel.strip_prefix("schemas/") {
            for root in local_schema_roots() {
                let candidate = root.join(rest);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            // No per-ref network fetches: with an unreliable network each
            // missing ref would stall a scan worker. The dtschema repository
            // is downloaded once up front instead; a ref that is not in it
            // simply marks the binding's property set as incomplete.
            return None;
        }
        return None;
    }
    let base = binding_path.parent().or(binding_dir)?;
    let candidate = base.join(clean);
    if candidate.is_file() {
        return Some(candidate);
    }
    if let Some(root) = in_tree_root(binding_dir) {
        let candidate = root.join(clean);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Rust's regex has no look-around; drop look-behind groups (making the
/// matcher slightly more permissive) before compiling.
fn compile_pattern(pattern: &str) -> Option<regex::Regex> {
    if let Ok(re) = regex::Regex::new(pattern) {
        return Some(re);
    }
    let mut sanitized = String::with_capacity(pattern.len());
    let mut chars = pattern.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == '(' && chars.peek().is_some_and(|(_, n)| *n == '?') {
            // consume "(?<" ... ")" look-behind group entirely
            let mut look = String::from("(?");
            let mut depth = 1;
            for (_, k) in chars.by_ref() {
                look.push(k);
                if k == '(' {
                    depth += 1;
                }
                if k == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            if look.starts_with("(?<=") || look.starts_with("(?<!") {
                continue; // drop look-behind
            }
            sanitized.push_str(&look);
            continue;
        }
        sanitized.push(c);
    }
    regex::Regex::new(&sanitized).ok()
}

fn in_tree_root(binding_dir: Option<&Path>) -> Option<PathBuf> {
    let mut dir = binding_dir?;
    loop {
        if dir.join("schemas").is_dir() || dir.join("Makefile").is_file() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Schema root of a local dtschema python install, if any.
fn local_schema_roots() -> &'static Vec<PathBuf> {
    static ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut roots = Vec::new();
        if let Ok(dir) = std::env::var("DTSLS_SCHEMAS_DIR") {
            roots.push(PathBuf::from(dir));
        }
        let repo_schemas = schema_cache_dir().join("repo/schemas");
        if repo_schemas.is_dir() {
            roots.push(repo_schemas);
        }
        let output = std::process::Command::new("python3")
            .args(["-c", "import dtschema,os;print(os.path.dirname(dtschema.__file__))"])
            .output();
        if let Ok(output) = output
            && output.status.success() {
                let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !dir.is_empty() {
                    roots.push(PathBuf::from(dir).join("schemas"));
                }
            }
        roots
    })
}

fn schema_cache_dir() -> PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("dtsls/dt-schema")
}

/// Download and extract the dtschema repository once into the user cache.
/// `schemas/` supplies bindings for compatibles the kernel tree only
/// references (cache, operating-points-v2, ...); `meta-schemas/` supplies
/// the implicit property allowlist.
fn ensure_dtschema_repo() -> Option<PathBuf> {
    let root = schema_cache_dir().join("repo");
    if root.join("schemas").is_dir() {
        return Some(root);
    }
    let url = "https://github.com/devicetree-org/dt-schema/archive/refs/heads/main.tar.gz";
    let mut response = http_agent().get(url).call().ok()?;
    let reader = response.body_mut().as_reader();
    let decoder = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(decoder);
    let tmp = schema_cache_dir().join("repo.tmp");
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).ok()?;
    let mut extracted = 0usize;
    for entry in archive.entries().ok()? {
        let Ok(mut entry) = entry else { continue };
        let Ok(path) = entry.path() else { continue };
        // keep only dtschema-main/dtschema/{schemas,meta-schemas}/...
        let path = path.to_path_buf();
        let mut components = path.components();
        components.next(); // dtschema-main
        let Some(stripped) = components.next() else { continue };
        if stripped.as_os_str() != "dtschema" {
            continue;
        }
        let rest: PathBuf = components.collect();
        if rest.as_os_str().is_empty() {
            continue;
        }
        if !(rest.starts_with("schemas") || rest.starts_with("meta-schemas")) {
            continue;
        }
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(tmp.join(&rest)).ok();
            continue;
        }
        let dest = tmp.join(&rest);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        if entry.unpack(&dest).is_ok() {
            extracted += 1;
        }
    }
    if extracted == 0 {
        std::fs::remove_dir_all(&tmp).ok();
        return None;
    }
    std::fs::remove_dir_all(&root).ok();
    std::fs::rename(&tmp, &root).ok()?;
    tracing::info!(files = extracted, "downloaded dtschema repository");
    Some(root)
}

/// Union of `properties` keys and `patternProperties` from every
/// meta-schema document: properties any node may carry.
fn collect_meta_allowlist(dir: &Path, properties: &mut HashSet<String>, patterns: &mut Vec<regex::Regex>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_meta_allowlist(&path, properties, patterns);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(docs) = YamlLoader::load_from_str(&strip_yaml_directives(&text)) else {
            continue;
        };
        for doc in &docs {
            collect_property_declarations(doc, properties, patterns);
        }
    }
}

fn collect_property_declarations(
    node: &Yaml,
    properties: &mut HashSet<String>,
    patterns: &mut Vec<regex::Regex>,
) {
    match node {
        Yaml::Hash(hash) => {
            for (key, value) in hash {
                match key.as_str() {
                    Some("properties") => {
                        if let Some(map) = value.as_hash() {
                            for prop_key in map.keys() {
                                if let Some(name) = prop_key.as_str() {
                                    properties.insert(name.to_string());
                                }
                            }
                        }
                    }
                    Some("patternProperties") => {
                        if let Some(map) = value.as_hash() {
                            for pattern_key in map.keys() {
                                if let Some(pattern) = pattern_key.as_str()
                                    && let Some(re) = compile_pattern(pattern) {
                                        patterns.push(re);
                                    }
                            }
                        }
                    }
                    _ => collect_property_declarations(value, properties, patterns),
                }
            }
        }
        Yaml::Array(items) => {
            for item in items {
                collect_property_declarations(item, properties, patterns);
            }
        }
        _ => {}
    }
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        // never let a stalled connection wedge the whole binding scan
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .new_agent()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/bindings")
            .join(name)
    }

    #[test]
    fn parses_const_and_required() {
        let binding = parse_bindings(&fixture("acme,widget.yaml"))
            .into_iter()
            .next()
            .expect("binding");
        assert_eq!(binding.singles, vec!["acme,widget"]);
        assert!(binding.properties.contains("reg"));
        assert!(binding.properties.contains("clocks"));
        assert!(binding.required.contains(&"reg".to_string()));
        assert!(binding.strict);
        // resolved through $ref: base.yaml
        assert!(binding.properties.contains("status_probe"));
    }

    #[test]
    fn child_node_schemas_are_separate_bindings() {
        let yaml = r#"
properties:
  compatible:
    const: acme,parent
  reg: true
required:
  - reg

patternProperties:
  'child@[0-9]+$':
    properties:
      compatible:
        const: acme,child
      child-prop: true
    required:
      - child-prop
horizontal: true
"#;
        let docs = YamlLoader::load_from_str(yaml).unwrap();
        let doc = &docs[0];
        let uri = Url::parse("file:///t.yaml").unwrap();
        let path = Path::new("/tmp/t.yaml");
        let parent = {
            let mut s = Vec::new();
            let mut c = Vec::new();
            parse_compatible(
                yaml_get(doc, "properties").and_then(|p| yaml_get(p, "compatible")).unwrap(),
                &mut s,
                &mut c,
            );
            build_binding(&uri, path, doc, s, c)
        };
        assert_eq!(parent.required, vec!["reg"]);
        // child-schema properties are permissively merged into the parent's
        // allowed set (false negatives beat false positives), but required
        // lists must stay separate
        assert!(parent.properties.contains("child-prop"));

        let mut children = Vec::new();
        collect_child_schemas(doc, 0, &mut |schema| {
            let mut s = Vec::new();
            let mut c = Vec::new();
            if let Some(compatible) =
                yaml_get(schema, "properties").and_then(|p| yaml_get(p, "compatible"))
            {
                parse_compatible(compatible, &mut s, &mut c);
                children.push(build_binding(&uri, path, schema, s, c));
            }
        });
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].singles, vec!["acme,child"]);
        assert_eq!(children[0].required, vec!["child-prop"]);
        assert!(!children[0].properties.contains("reg"));
    }


    #[test]
    fn compound_compatible_parts_do_not_leak() {
        let yaml = r#"
properties:
  compatible:
    items:
      - enum:
          - fsl,imx28-pinctrl
      - const: simple-bus
"#;
        let docs = YamlLoader::load_from_str(yaml).unwrap();
        let doc = &docs[0];
        let node = yaml_get(doc, "properties")
            .and_then(|p| yaml_get(p, "compatible"))
            .unwrap();
        let mut singles = Vec::new();
        let mut compounds = Vec::new();
        parse_compatible(node, &mut singles, &mut compounds);
        assert!(singles.is_empty(), "compound parts leaked as singles");
        assert_eq!(compounds.len(), 1);

        let index = BindingIndex::default();
        index.insert(Binding {
            uri: Url::parse("file:///t.yaml").unwrap(),
            singles,
            compounds,
            properties: HashSet::new(),
            required: Vec::new(),
            strict: false,
            refs_complete: true,
            patterns: Vec::new(),
        });
        // a bare "simple-bus" node must NOT match this binding
        assert!(index.lookup(&["simple-bus".to_string()]).is_none());
        // the full compound list must
        assert!(index
            .lookup(&["fsl,imx28-pinctrl".to_string(), "simple-bus".to_string()])
            .is_some());
    }
}
