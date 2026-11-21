use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use tower_lsp::lsp_types::Url;
use yaml_rust2::{Yaml, YamlLoader};

/// Completions for `compatible = "…"`: vendors, their devices, and the
/// compatibles that carry no vendor prefix (`operating-points-v2`, ...).
#[derive(Default)]
pub struct CompatibleIndex {
    /// vendor -> (full compatible -> defining file name)
    vendors: BTreeMap<String, BTreeMap<String, String>>,
    /// vendor -> human readable name, from `vendor-prefixes.yaml`
    vendor_names: BTreeMap<String, String>,
    /// compatibles without a vendor prefix
    vendor_less: BTreeSet<String>,
}

impl CompatibleIndex {
    fn add(&mut self, compatible: &str, file: &str) {
        match compatible.split_once(',') {
            Some((vendor, device)) if !vendor.is_empty() && !device.is_empty() => {
                self.vendors
                    .entry(vendor.to_string())
                    .or_default()
                    .entry(compatible.to_string())
                    .or_insert_with(|| file.to_string());
            }
            Some(_) => {}
            None if !compatible.is_empty() => {
                self.vendor_less.insert(compatible.to_string());
            }
            None => {}
        }
    }
}

/// One position of a compound compatible: either an enumerated value set or
/// a regex the value must match (dt-schema `pattern:`, e.g. Adreno's
/// `^qcom,adreno-[0-9a-f]{8}$`).
#[derive(Clone)]
pub enum PositionMatcher {
    Values(Vec<String>),
    Pattern(regex::Regex),
}

impl PositionMatcher {
    fn matches(&self, value: &str) -> bool {
        match self {
            PositionMatcher::Values(values) => values.iter().any(|v| v == value),
            PositionMatcher::Pattern(re) => re.is_match(value),
        }
    }
}

/// Compound compatible pattern: per position, the allowed values (or a
/// position regex).
type CompoundPattern = Vec<PositionMatcher>;

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
    /// groups where at least one member must be present: dt-schema rewrites
    /// `required: [interrupts]` into "interrupts or interrupts-extended"
    pub required_any: Vec<Vec<String>>,
    /// `additionalProperties: false` / `unevaluatedProperties: false`
    pub strict: bool,
    /// every `$ref` resolved locally; unknown-property checks are only safe
    /// when the allowed set is complete
    pub refs_complete: bool,
    /// compiled `patternProperties` matchers (e.g. `.*-supply$`)
    pub patterns: regex::RegexSet,
}

/// compatible string -> binding. First file wins (sorted scan order), so
/// lookups are deterministic.
pub struct BindingIndex {
    by_single: RwLock<HashMap<String, Arc<Binding>>>,
    /// first compatible string -> compound patterns
    by_compound: RwLock<CompoundBucket>,
    /// compound patterns whose FIRST position is a regex (nothing concrete
    /// to bucket by — the list is tiny, scanned as a fallback)
    pattern_compounds: RwLock<Vec<(CompoundPattern, Arc<Binding>)>>,
    count: AtomicUsize,
    /// meta-schema properties every node may carry (`pinctrl-*`, ...)
    implicit_properties: RwLock<HashSet<String>>,
    /// meta-schema property patterns (`-supply$`, `.*-names$`, ...)
    implicit_patterns: RwLock<regex::RegexSet>,
    /// vendors/devices/vendor-less compatibles for `compatible = "…"`
    compat: RwLock<CompatibleIndex>,
    /// bumped whenever the index contents change, so diagnostics caches keyed
    /// on binding answers can be dropped
    generation: AtomicU64,
    /// false while the background scan is running: a partially filled index
    /// would otherwise report "no binding" for every node
    ready: AtomicBool,
}

impl Default for BindingIndex {
    fn default() -> Self {
        Self {
            by_single: Default::default(),
            by_compound: Default::default(),
            pattern_compounds: Default::default(),
            count: AtomicUsize::default(),
            implicit_properties: Default::default(),
            implicit_patterns: Default::default(),
            compat: Default::default(),
            generation: AtomicU64::default(),
            ready: AtomicBool::new(true),
        }
    }
}

impl BindingIndex {
    pub fn compatible_vendors(&self) -> Vec<(String, Option<String>, usize)> {
        let compat = self.compat.read().unwrap();
        compat
            .vendors
            .iter()
            .map(|(vendor, devices)| {
                (
                    vendor.clone(),
                    compat.vendor_names.get(vendor).cloned(),
                    devices.len(),
                )
            })
            .collect()
    }

    pub fn compatible_devices(&self, vendor: &str) -> Vec<(String, String)> {
        self.compat
            .read()
            .unwrap()
            .vendors
            .get(vendor)
            .map(|devices| {
                devices
                    .iter()
                    .map(|(compatible, file)| (compatible.clone(), file.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn vendor_less_compatibles(&self) -> Vec<String> {
        self.compat
            .read()
            .unwrap()
            .vendor_less
            .iter()
            .cloned()
            .collect()
    }

    /// May the node carry this property according to the binding and the
    /// meta-schemas?
    pub fn is_property_allowed(&self, name: &str, binding: &Binding) -> bool {
        is_common_property(name)
            || binding.properties.contains(name)
            || binding.patterns.is_match(name)
            || self.implicit_properties.read().unwrap().contains(name)
            || self.implicit_patterns.read().unwrap().is_match(name)
    }

    fn set_implicit(&self, properties: HashSet<String>, patterns: regex::RegexSet) {
        *self.implicit_properties.write().unwrap() = properties;
        *self.implicit_patterns.write().unwrap() = patterns;
        self.bump_generation();
    }

    /// Whether the initial scan has finished; checks are skipped until then.
    pub fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn mark_ready(&self, ready: bool) {
        self.ready
            .store(ready, std::sync::atomic::Ordering::Relaxed);
    }

    /// Changes whenever binding answers may have changed.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn bump_generation(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
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
        let match_len = |pattern: &CompoundPattern| -> bool {
            pattern.len() == compatible_list.len()
                && pattern
                    .iter()
                    .zip(compatible_list)
                    .all(|(matcher, actual)| matcher.matches(actual))
        };
        {
            let compounds = self.by_compound.read().unwrap();
            if let Some(first) = compatible_list.first()
                && let Some(bucket) = compounds.get(first)
            {
                for (pattern, binding) in bucket {
                    if match_len(pattern) {
                        return Some(binding.clone());
                    }
                }
            }
        }
        // compounds whose first position is a regex can't be bucketed by
        // value: scan the (tiny) fallback list
        let pattern_compounds = self.pattern_compounds.read().unwrap();
        for (pattern, binding) in pattern_compounds.iter() {
            if match_len(pattern) {
                return Some(binding.clone());
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.count.load(Ordering::Relaxed) == 0
    }

    pub fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Test-only: insert without the scan.
    #[cfg(test)]
    pub fn insert_for_test(&self, binding: Binding) {
        self.insert(binding);
    }

    fn insert(&self, binding: Binding) {
        let shared = Arc::new(binding);
        let file = shared
            .uri
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("")
            .to_string();
        {
            let mut singles = self.by_single.write().unwrap();
            for compatible in &shared.singles {
                singles
                    .entry(compatible.clone())
                    .or_insert_with(|| shared.clone());
            }
        }
        {
            let mut compounds = self.by_compound.write().unwrap();
            let mut pattern_compounds = self.pattern_compounds.write().unwrap();
            for pattern in &shared.compounds {
                // bucket under EVERY allowed first-position value, otherwise
                // a node whose first string is not the first enum entry misses
                match pattern.first() {
                    Some(PositionMatcher::Values(allowed)) => {
                        for first in allowed {
                            compounds
                                .entry(first.clone())
                                .or_default()
                                .push((pattern.clone(), shared.clone()));
                        }
                    }
                    // a regex first position has nothing concrete to bucket
                    // by: keep it in the tiny fallback list
                    Some(PositionMatcher::Pattern(_)) => {
                        pattern_compounds.push((pattern.clone(), shared.clone()));
                    }
                    None => {}
                }
            }
        }
        {
            let mut compat = self.compat.write().unwrap();
            for compatible in &shared.singles {
                compat.add(compatible, &file);
            }
            for pattern in &shared.compounds {
                for matcher in pattern {
                    if let PositionMatcher::Values(allowed) = matcher {
                        for compatible in allowed {
                            compat.add(compatible, &file);
                        }
                    }
                }
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.bump_generation();
    }

    fn set_vendor_names(&self, names: BTreeMap<String, String>) {
        self.compat.write().unwrap().vendor_names = names;
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

/// Scan the kernel bindings tree plus the dtschema package installed on the
/// host. Runs on a worker thread at startup.
pub fn scan(index: &BindingIndex, dir: &Path) {
    index.mark_ready(false);
    let started = std::time::Instant::now();
    let package = dtschema_package_dir();
    if let Some(package) = package {
        let mut properties = HashSet::default();
        let mut patterns = Vec::new();
        collect_meta_allowlist(
            &package.join("meta-schemas"),
            &mut properties,
            &mut patterns,
        );
        tracing::info!(
            properties = properties.len(),
            patterns = patterns.len(),
            "meta-schema allowlist"
        );
        index.set_implicit(
            properties,
            regex::RegexSet::new(&patterns).unwrap_or_default(),
        );
    } else {
        tracing::warn!("dtschema python package not found; run `pip install dtschema`");
    }
    // human readable vendor names for completion details
    index.set_vendor_names(parse_vendor_prefixes(&dir.join("vendor-prefixes.yaml")));
    index.mark_ready(true);

    let mut files: Vec<PathBuf> = Vec::new();
    let mut scan_dirs = vec![dir.to_path_buf()];
    if let Some(package) = package {
        let schemas = package.join("schemas");
        if schemas.is_dir() {
            scan_dirs.push(schemas);
        }
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

/// `vendor-prefixes.yaml`: `"^ti,": description: Texas Instruments, Inc`.
fn parse_vendor_prefixes(path: &Path) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return names;
    };
    let Ok(docs) = YamlLoader::load_from_str(&strip_yaml_directives(&text)) else {
        return names;
    };
    let Some(hash) = docs.first().and_then(Yaml::as_hash) else {
        return names;
    };
    for (key, value) in hash {
        let Some(key) = key.as_str() else { continue };
        let vendor = key.trim_start_matches('^').trim_end_matches(',');
        // skip pattern entries like "^(.*),$"
        if vendor.is_empty() || !vendor.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            continue;
        }
        let Some(description) = value
            .as_hash()
            .and_then(|hash| hash.get(&Yaml::String("description".to_string())))
            .and_then(Yaml::as_str)
        else {
            continue;
        };
        names.insert(vendor.to_string(), description.trim().to_string());
    }
    names
}

/// dtschema's `fixup_node_props` (fixups.py): implicit properties the tooling
/// adds to a schema. Names and conditions are a direct port so the validator
/// sees the same allowed set dt-schema does.
fn fixup_node_props(
    schema: &Yaml,
    properties: &mut HashSet<String>,
    patterns: &mut Vec<String>,
) {
    let additional = yaml_get(schema, "additionalProperties");
    let unevaluated = yaml_get(schema, "unevaluatedProperties");
    // the fixup only runs when the schema says anything about other properties
    if additional.is_none() && unevaluated.is_none() {
        return;
    }
    if properties.contains("clocks") {
        for name in [
            "assigned-clocks",
            "assigned-clock-rates-u64",
            "assigned-clock-rates",
            "assigned-clock-parents",
            "assigned-clock-sscs",
        ] {
            properties.insert(name.to_string());
        }
    }
    if properties.contains("ranges") {
        properties.insert("dma-ranges".to_string());
    }
    // unrestricted schemas get nothing else
    let open = matches!(additional, Some(Yaml::Boolean(true)))
        || matches!(unevaluated, Some(Yaml::Boolean(true)));
    if open {
        return;
    }
    for name in [
        "phandle",
        "status",
        "secure-status",
        "$nodename",
        "bootph-pre-sram",
        "bootph-verify",
        "bootph-pre-ram",
        "bootph-some-ram",
        "bootph-all",
    ] {
        properties.insert(name.to_string());
    }
    let has_numbered_pinctrl = properties.iter().any(|name| {
        name.strip_prefix("pinctrl-")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|first| first.is_ascii_digit())
    });
    if !has_numbered_pinctrl {
        properties.insert("pinctrl-names".to_string());
        if let Some(pattern) = compile_pattern("^pinctrl-[0-9]+$") {
            patterns.push(pattern);
        }
    }
}

fn build_binding(
    uri: &Url,
    path: &Path,
    schema: &Yaml,
    singles: Vec<String>,
    compounds: Vec<CompoundPattern>,
) -> Binding {
    let mut properties: HashSet<String> = HashSet::default();
    if let Some(props) = yaml_get(schema, "properties").and_then(Yaml::as_hash) {
        for key in props.keys() {
            if let Some(name) = key.as_str() {
                properties.insert(name.to_string());
            }
        }
    }
    let mut pattern_strings: Vec<String> = Vec::new();
    if let Some(pattern_props) = yaml_get(schema, "patternProperties").and_then(Yaml::as_hash) {
        for (key, child) in pattern_props {
            if let Some(pattern) = key.as_str()
                && let Some(pattern) = compile_pattern(pattern)
            {
                pattern_strings.push(pattern);
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

    // dt-schema fixups (`fixup_interrupts` in dtschema): supporting
    // `interrupts` implies `interrupts-extended` and `interrupt-parent` are
    // supported too, and either interrupt form satisfies the requirement.
    if properties.contains("interrupts") {
        properties.insert("interrupts-extended".to_string());
    }
    if properties.contains("interrupts") || properties.contains("interrupt-controller") {
        properties.insert("interrupt-parent".to_string());
    }
    fixup_node_props(schema, &mut properties, &mut pattern_strings);

    let mut required = Vec::new();
    let mut required_any = Vec::new();
    if let Some(list) = yaml_get(schema, "required").and_then(Yaml::as_vec) {
        for item in list {
            let Some(name) = item.as_str() else { continue };
            if name == "interrupts" {
                required_any.push(vec![
                    "interrupts".to_string(),
                    "interrupts-extended".to_string(),
                ]);
            } else {
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
        required_any,
        strict,
        refs_complete,
        patterns: regex::RegexSet::new(&pattern_strings).unwrap_or_default(),
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
                    Some("pattern") => {
                        // a regex-compatible binding: single-position matcher
                        // (fail-open: an uncompilable regex matches anything)
                        if value.as_str().is_some() {
                            let re = value
                                .as_str()
                                .and_then(|s| regex::Regex::new(s).ok())
                                .unwrap_or_else(|| regex::Regex::new("").unwrap());
                            compounds.push(vec![PositionMatcher::Pattern(re)]);
                        }
                    }
                    Some("items") => {
                        if let Some(items) = value.as_vec() {
                            let mut pattern = Vec::new();
                            for item in items {
                                let matcher = collect_position(item);
                                let empty =
                                    matches!(&matcher, PositionMatcher::Values(v) if v.is_empty());
                                if empty {
                                    // a position with no constraint means the
                                    // branch cannot match: drop it entirely
                                    pattern.clear();
                                    break;
                                }
                                pattern.push(matcher);
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

/// One `items` entry of a compound compatible: an enumerated value set, or a
/// regex the string at that position must match (dt-schema `pattern:`).
fn collect_position(item: &Yaml) -> PositionMatcher {
    match item {
        Yaml::String(s) => PositionMatcher::Values(vec![s.clone()]),
        Yaml::Hash(hash) => {
            for (key, value) in hash {
                match key.as_str() {
                    Some("const") => {
                        if let Some(s) = value.as_str() {
                            return PositionMatcher::Values(vec![s.to_string()]);
                        }
                    }
                    Some("enum") => {
                        let mut values = Vec::new();
                        collect_value_strings(item, &mut values);
                        if !values.is_empty() {
                            return PositionMatcher::Values(values);
                        }
                    }
                    Some("pattern") => {
                        if let Some(s) = value.as_str() {
                            // fail-open: an uncompilable regex matches anything
                            return PositionMatcher::Pattern(
                                regex::Regex::new(s).unwrap_or_else(|_| regex::Regex::new("").unwrap()),
                            );
                        }
                    }
                    Some("oneOf") | Some("anyOf") | Some("allOf") => {
                        let mut values = Vec::new();
                        if let Some(branches) = value.as_vec() {
                            for branch in branches {
                                collect_value_strings(branch, &mut values);
                            }
                        }
                        if !values.is_empty() {
                            return PositionMatcher::Values(values);
                        }
                    }
                    _ => {}
                }
            }
            PositionMatcher::Values(Vec::new())
        }
        _ => PositionMatcher::Values(Vec::new()),
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

/// A compilable form of `pattern`. Rust's regex has no look-around; drop
/// look-behind groups (making the matcher slightly more permissive) first.
fn compile_pattern(pattern: &str) -> Option<String> {
    if regex::Regex::new(pattern).is_ok() {
        return Some(pattern.to_string());
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
    regex::Regex::new(&sanitized).ok().map(|_| sanitized)
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

/// Root of a dtschema python package installed on this host (`pip install
/// dtschema`, or `DTSLS_SCHEMAS_DIR` pointing at it). Nothing is downloaded:
/// without the package, `$ref`s into dtschema and dtschema-hosted compatibles
/// stay unresolved, and the server reports that at the top of each file.
pub fn dtschema_package_dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        if let Ok(dir) = std::env::var("DTSLS_SCHEMAS_DIR") {
            let dir = PathBuf::from(dir);
            if dir.is_dir() {
                return Some(dir);
            }
        }
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                "import dtschema,os;print(os.path.dirname(dtschema.__file__))",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!dir.is_empty()).then(|| PathBuf::from(dir))
    });
    dir.as_ref()
}

/// `schemas/` roots of the local dtschema install, used for `$ref` resolution.
fn local_schema_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(package) = dtschema_package_dir() {
        let schemas = package.join("schemas");
        if schemas.is_dir() {
            roots.push(schemas);
        } else if package.is_dir() {
            roots.push(package.clone());
        }
    }
    roots
}

/// Union of `properties` keys and `patternProperties` from every
/// meta-schema document: properties any node may carry.
fn collect_meta_allowlist(dir: &Path, properties: &mut HashSet<String>, patterns: &mut Vec<String>) {
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
    patterns: &mut Vec<String>,
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
                                    && let Some(pattern) = compile_pattern(pattern) {
                                        patterns.push(pattern);
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
            properties: HashSet::default(),
            required: Vec::new(),
            required_any: Vec::new(),
            strict: false,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        });
        // a bare "simple-bus" node must NOT match this binding
        assert!(index.lookup(&["simple-bus".to_string()]).is_none());
        // the full compound list must
        assert!(index
            .lookup(&["fsl,imx28-pinctrl".to_string(), "simple-bus".to_string()])
            .is_some());
    }

    #[test]
    fn pattern_positions_match_by_regex() {
        // the Adreno binding form: oneOf items with a regex position plus a
        // const position (Documentation/devicetree/bindings/display/msm/gpu.yaml)
        let yaml = r#"
properties:
  compatible:
    oneOf:
      - items:
          - pattern: '^qcom,adreno-[0-9a-f]{8}$'
          - const: qcom,adreno
      - items:
          - pattern: '^qcom,adreno-[3-7][0-9][0-9]\.[0-9]+$'
          - const: qcom,adreno
"#;
        let docs = YamlLoader::load_from_str(yaml).unwrap();
        let doc = &docs[0];
        let node = yaml_get(doc, "properties")
            .and_then(|p| yaml_get(p, "compatible"))
            .unwrap();
        let mut singles = Vec::new();
        let mut compounds = Vec::new();
        parse_compatible(node, &mut singles, &mut compounds);
        assert!(singles.is_empty(), "pattern parts leaked as singles");
        assert_eq!(compounds.len(), 2);

        let index = BindingIndex::default();
        index.insert(Binding {
            uri: Url::parse("file:///gpu.yaml").unwrap(),
            singles,
            compounds,
            properties: ["compatible", "reg"].into_iter().map(str::to_string).collect(),
            required: vec!["compatible".to_string()],
            required_any: Vec::new(),
            strict: true,
            refs_complete: true,
            patterns: regex::RegexSet::empty(),
        });

        // the sm8650 GPU node: 8-hex-chars form (first regex)
        assert!(index
            .lookup(&["qcom,adreno-43051401".to_string(), "qcom,adreno".to_string()])
            .is_some());
        // the gen4 dotted form (second regex)
        assert!(index
            .lookup(&["qcom,adreno-690.1".to_string(), "qcom,adreno".to_string()])
            .is_some());
        // first string not matching the pattern: no match
        assert!(index
            .lookup(&["qcom,not-adreno".to_string(), "qcom,adreno".to_string()])
            .is_none());
        // wrong arity: no match
        assert!(index.lookup(&["qcom,adreno".to_string()]).is_none());
    }

    #[test]
    fn real_kernel_gpu_binding_matches_the_adreno_node() {
        // regression guard against the actual kernel binding file
        let path = Path::new("/linux-7.2.2/Documentation/devicetree/bindings/display/msm/gpu.yaml");
        if !path.is_file() {
            return; // kernel tree not present on this machine
        }
        let bindings = parse_bindings(path);
        let index = BindingIndex::default();
        for binding in bindings {
            index.insert(binding);
        }
        assert!(index
            .lookup(&["qcom,adreno-43051401".to_string(), "qcom,adreno".to_string()])
            .is_some());
    }

    #[test]
    fn compatible_index_splits_vendors_and_vendor_less() {
        let index = BindingIndex::default();
        for (singles, file) in [
            (vec!["acme,widget", "operating-points-v2"], "widget.yaml"),
            (vec!["acme,gadget"], "gadget.yaml"),
            (vec!["ti,am335x"], "am335x.yaml"),
        ] {
            index.insert_for_test(Binding {
                uri: Url::parse(&format!("file:///{file}")).unwrap(),
                singles: singles.into_iter().map(str::to_string).collect(),
                compounds: Vec::new(),
                properties: HashSet::default(),
                required: Vec::new(),
                required_any: Vec::new(),
                strict: false,
                refs_complete: true,
                patterns: regex::RegexSet::empty(),
            });
        }
        let vendors = index.compatible_vendors();
        assert!(vendors.iter().any(|(v, _, count)| v == "acme" && *count == 2));
        assert!(vendors.iter().any(|(v, _, _)| v == "ti"));
        assert_eq!(
            index.compatible_devices("acme"),
            vec![
                ("acme,gadget".to_string(), "gadget.yaml".to_string()),
                ("acme,widget".to_string(), "widget.yaml".to_string()),
            ]
        );
        assert_eq!(index.vendor_less_compatibles(), vec!["operating-points-v2"]);
    }

    #[test]
    fn node_props_fixup_follows_dtschema() {
        let dir = std::env::temp_dir().join(format!("dtsls-nodeprops-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let head = "%YAML 1.2\n---\n$id: http://devicetree.org/schemas/t.yaml#\n\
                    $schema: http://devicetree.org/meta-schemas/core.yaml#\n\
                    title: t\nmaintainers: [{name: t, email: t@example.com}]\n\
                    properties:\n  compatible:\n    const: acme,t\n  clocks: true\n  ranges: true\n";

        let strict_path = dir.join("strict.yaml");
        std::fs::write(&strict_path, format!("{head}additionalProperties: false\n")).unwrap();
        let bindings = parse_bindings(&strict_path);
        let binding = bindings.first().expect("no binding");
        // clocks => assigned-clock* group, ranges => dma-ranges, strict =>
        // implicit node properties + the pinctrl group
        assert!(binding.properties.contains("assigned-clocks"));
        assert!(binding.properties.contains("assigned-clock-rates"));
        assert!(binding.properties.contains("dma-ranges"));
        assert!(binding.properties.contains("bootph-all"));
        assert!(binding.properties.contains("pinctrl-names"));
        assert!(binding.patterns.is_match("pinctrl-0"));

        let open_path = dir.join("open.yaml");
        std::fs::write(&open_path, format!("{head}additionalProperties: true\n")).unwrap();
        let bindings = parse_bindings(&open_path);
        let binding = bindings.first().expect("no binding");
        assert!(binding.properties.contains("assigned-clocks"));
        assert!(binding.properties.contains("dma-ranges"));
        assert!(!binding.properties.contains("bootph-all"));
        assert!(!binding.properties.contains("pinctrl-names"));

        let silent_path = dir.join("silent.yaml");
        std::fs::write(&silent_path, head).unwrap();
        let bindings = parse_bindings(&silent_path);
        let binding = bindings.first().expect("no binding");
        assert!(!binding.properties.contains("assigned-clocks"));
        assert!(!binding.properties.contains("dma-ranges"));

        for name in ["strict.yaml", "open.yaml", "silent.yaml"] {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }

    #[test]
    fn interrupts_fixup_follows_dtschema() {
        let dir = std::env::temp_dir().join(format!("dtsls-fixup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("acme,irq-test.yaml");
        std::fs::write(
            &path,
            "%YAML 1.2\n---\n$id: http://devicetree.org/schemas/acme,irq-test.yaml#\n\
             $schema: http://devicetree.org/meta-schemas/core.yaml#\n\
             title: irq test\nmaintainers: [{name: t, email: t@example.com}]\n\
             properties:\n  compatible:\n    const: acme,irq-test\n  interrupts:\n    maxItems: 1\n\
             required:\n  - compatible\n  - interrupts\nadditionalProperties: false\n",
        )
        .unwrap();
        let bindings = parse_bindings(&path);
        let binding = bindings.first().expect("no binding");
        // declaring `interrupts` also accepts `interrupts-extended` and
        // `interrupt-parent`, and either interrupt form satisfies required
        assert!(binding.properties.contains("interrupts-extended"));
        assert!(binding.properties.contains("interrupt-parent"));
        assert_eq!(binding.required, vec!["compatible"]);
        assert_eq!(
            binding.required_any,
            vec![vec![
                "interrupts".to_string(),
                "interrupts-extended".to_string()
            ]]
        );
        let _ = std::fs::remove_file(&path);
    }
}
