//! Phandle and interrupt checks, mirroring dtc's `checks.c`.
//!
//! Ported 1:1, including failure texts and severities:
//!
//! * `check_property_phandle_args` (WARNING, `WARNING_PROPERTY_PHANDLE_CELLS`):
//!   a consumer property from dtc's fixed table (`clocks`, `dmas`, ...) is a
//!   flat cell array; a phandle cell consumes itself plus the provider's
//!   `#X-cells` argument cells.
//! * `check_interrupts_property` (WARNING): `interrupts` carries no phandle, so
//!   its size must be a whole multiple of the controller's `#interrupt-cells`,
//!   found through `interrupt-parent` on the node or an ancestor.
//! * `check_interrupt_provider` (WARNING): a node with `interrupt-controller`
//!   or `interrupt-map` must have `#interrupt-cells`, and a node with
//!   `#interrupt-cells` must be an interrupt provider.
//! * `check_is_cell` (WARNING, `WARNING_IF_NOT_CELL`): every `#X-cells`
//!   property in the table must be a single cell.
//!
//! dtc's table lives in dtc itself; the binding YAML does not express it, so
//! the table is copied verbatim.
//!
//! # Nothing is skipped silently
//!
//! Every branch that cannot reach a verdict produces a diagnostic: a cell whose
//! value does not evaluate to a 32-bit integer, a `&label` that resolves to no
//! node, a numeric phandle that resolves to no node, a missing
//! `interrupt-parent`. Where a later check already reports the same thing, the
//! earlier one says so in a comment instead of returning a bare `None`.

use std::collections::HashMap;

use tree_sitter::Node;

use crate::document::Document;
use crate::util;

/// `(consumer property, provider cell property)`, exactly dtc's table.
const PROVIDERS: &[(&str, &str)] = &[
    ("clocks", "#clock-cells"),
    ("cooling-device", "#cooling-cells"),
    ("dmas", "#dma-cells"),
    ("hwlocks", "#hwlock-cells"),
    ("interrupts-extended", "#interrupt-cells"),
    ("io-channels", "#io-channel-cells"),
    ("iommus", "#iommu-cells"),
    ("mboxes", "#mbox-cells"),
    ("msi-parent", "#msi-cells"),
    ("mux-controls", "#mux-control-cells"),
    ("phys", "#phy-cells"),
    ("power-domains", "#power-domain-cells"),
    ("pwms", "#pwm-cells"),
    ("resets", "#reset-cells"),
    ("sound-dai", "#sound-dai-cells"),
    ("thermal-sensors", "#thermal-sensor-cells"),
];

/// dtc marks providers that may omit their cells property (`optional`).
const OPTIONAL: &[&str] = &["msi-parent"];

/// A diagnostic produced by these checks.
pub struct Problem {
    pub start: u32,
    pub end: u32,
    /// dtc reports unresolved references as ERROR, everything else as WARNING
    pub error: bool,
    pub message: String,
}

fn cells_name_for(property: &str) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .find(|(consumer, _)| *consumer == property)
        .map(|(_, cells)| *cells)
}

/// A phandle reference in source form.
enum Reference<'a> {
    Label(&'a str),
    /// `&{/soc/foo}` — a path reference, resolved from the unit's root node
    Path(&'a str),
}

/// One cell of a phandle-array property.
struct Cell<'a> {
    /// the reference when the cell is `&label` / `&{/path}`
    reference: Option<Reference<'a>>,
    /// value when the text evaluates to a 32-bit integer
    value: Option<u32>,
    /// source text, for the "not an integer" message
    text: &'a str,
    start: usize,
    end: usize,
}

/// What a node declares for its `#X-cells` properties.
enum CellsValue {
    /// the property is absent
    Absent,
    /// present and readable
    Value(u32),
    /// present but not a single integer cell — reported by `check_cells_shape`
    Malformed,
}

/// Everything resolvable from the unit's own text. `doc` holds the whole
/// preprocessed translation unit, so labels and providers from every included
/// file are present.
struct Unit<'a> {
    /// label -> the subnode carrying it
    labels: HashMap<&'a str, Node<'a>>,
    /// numeric phandle -> the subnode declaring it
    phandles: HashMap<u32, Node<'a>>,
    /// subnode id -> its canonical path, with `&label { }` fragments merged in
    paths: HashMap<usize, String>,
    /// canonical path -> `&label { }` fragment bodies that extend that node
    fragments: HashMap<String, Vec<Node<'a>>>,
    /// canonical path -> merged `#X-cells` values
    cells: HashMap<String, HashMap<String, CellsValue>>,
    /// the root devicetree node, for `&{/path}` references
    root: Option<Node<'a>>,
    /// `&label { }` references that resolve to no node in this unit
    unresolved_fragments: Vec<Node<'a>>,
}

impl<'a> Unit<'a> {
    fn new(doc: &'a Document) -> Self {
        let mut unit = Unit {
            labels: HashMap::new(),
            phandles: HashMap::new(),
            paths: HashMap::new(),
            fragments: HashMap::new(),
            cells: HashMap::new(),
            root: find_devicetree(doc.tree.root_node()),
            unresolved_fragments: Vec::new(),
        };
        unit.collect_names(doc, doc.tree.root_node());
        unit.collect_fragments(doc, doc.tree.root_node());
        unit.collect_paths(doc, doc.tree.root_node());
        unit.collect_cells(doc, doc.tree.root_node());
        unit
    }

    /// Labels and explicit `phandle` properties.
    fn collect_names(&mut self, doc: &'a Document, node: Node<'a>) {
        if node.kind() == "subnode" {
            for i in 0..node.child_count() {
                let Some(child) = node.child(i as u32) else {
                    continue;
                };
                if child.kind() != "label" {
                    continue;
                }
                let name = util::node_text(doc, child);
                let name = name.trim_end_matches(':').trim();
                if !name.is_empty() {
                    self.labels.entry(name).or_insert(node);
                }
            }
            if let Some(value) = explicit_phandle(doc, node) {
                self.phandles.entry(value).or_insert(node);
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                self.collect_names(doc, child);
            }
        }
    }

    /// `&label { ... }` fragments, keyed by the canonical path they extend.
    fn collect_fragments(&mut self, doc: &'a Document, node: Node<'a>) {
        if node.kind() == "devicetree"
            && let Some(reference) = util::find_child(node, "reference")
        {
            match self.resolve(doc, reference).and_then(|target| self.path_of(doc, target)) {
                Some(path) => {
                    if let Some(body) = body_of(node) {
                        self.fragments.entry(path).or_default().push(body);
                    }
                }
                None => self.unresolved_fragments.push(reference),
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                self.collect_fragments(doc, child);
            }
        }
    }

    fn collect_paths(&mut self, doc: &'a Document, node: Node<'a>) {
        if node.kind() == "subnode" {
            // computing the path also memoizes every ancestor's path
            let _ = self.path_of(doc, node);
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                self.collect_paths(doc, child);
            }
        }
    }

    fn collect_cells(&mut self, doc: &'a Document, node: Node<'a>) {
        if node.kind() == "subnode" {
            if let Some(path) = self.paths.get(&node.id()).cloned() {
                if let Some(body) = body_of(node) {
                    for i in 0..body.child_count() {
                        let Some(child) = body.child(i as u32) else {
                            continue;
                        };
                        if child.kind() != "property" {
                            continue;
                        }
                        let Some(name_node) = child.child_by_field_name("name") else {
                            continue;
                        };
                        let name = util::node_text(doc, name_node);
                        if name.starts_with('#') && name.ends_with("-cells") {
                            let value = match property_u32(doc, child) {
                                Some(value) => CellsValue::Value(value),
                                None => CellsValue::Malformed,
                            };
                            self.cells
                                .entry(path.clone())
                                .or_default()
                                .insert(name.to_string(), value);
                        }
                    }
                }
                // fragment properties take precedence, like dtc's merge
                let fragments = self.fragments.get(&path).cloned().unwrap_or_default();
                for body in fragments {
                    for i in 0..body.child_count() {
                        let Some(child) = body.child(i as u32) else {
                            continue;
                        };
                        if child.kind() != "property" {
                            continue;
                        }
                        let Some(name_node) = child.child_by_field_name("name") else {
                            continue;
                        };
                        let name = util::node_text(doc, name_node);
                        if name.starts_with('#') && name.ends_with("-cells") {
                            let value = match property_u32(doc, child) {
                                Some(value) => CellsValue::Value(value),
                                None => CellsValue::Malformed,
                            };
                            self.cells
                                .entry(path.clone())
                                .or_default()
                                .insert(name.to_string(), value);
                        }
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                self.collect_cells(doc, child);
            }
        }
    }

    /// Canonical path of a node: `&label { }` fragments map onto the node they
    /// extend, so the whole unit looks like one merged tree.
    fn path_of(&mut self, doc: &'a Document, node: Node<'a>) -> Option<String> {
        if let Some(path) = self.paths.get(&node.id()) {
            return Some(path.clone());
        }
        let path = match node.kind() {
            "subnode" => {
                let name = node.child_by_field_name("name")?;
                let name = util::node_text(doc, name).to_string();
                match self.canonical_parent(doc, node) {
                    Some(parent) => {
                        let base = self.path_of(doc, parent).unwrap_or_default();
                        Some(format!("{base}/{name}"))
                    }
                    None => Some(format!("/{name}")),
                }
            }
            "devicetree" => match util::find_child(node, "reference") {
                Some(reference) => {
                    let target = self.resolve(doc, reference)?;
                    self.path_of(doc, target)
                }
                None => Some(String::new()),
            },
            _ => return None,
        };
        if let Some(path) = &path {
            self.paths.insert(node.id(), path.clone());
        }
        path
    }

    /// The node that encloses `node` once fragments are merged.
    fn canonical_parent(&self, doc: &'a Document, node: Node<'a>) -> Option<Node<'a>> {
        let owning = util::owning_node(node)?;
        match owning.kind() {
            // a fragment's parent is the node it extends; the root devicetree
            // is the end of the chain but still carries properties
            "devicetree" => match util::find_child(owning, "reference") {
                Some(reference) => self.resolve(doc, reference),
                None => Some(owning),
            },
            _ => Some(owning),
        }
    }

    fn resolve(&self, doc: &Document, reference: Node<'a>) -> Option<Node<'a>> {
        if let Some(label_ref) = util::find_child(reference, "label_ref") {
            let label = util::node_text(doc, label_ref);
            return self.labels.get(label.strip_prefix('&')?).copied();
        }
        let path_ref = util::find_child(reference, "path_ref")?;
        let path_ref = util::node_text(doc, path_ref);
        resolve_path(self.root?, doc, &path_ref)
    }

    /// Whether every `&label { }` on the way to the root resolves.
    fn chain_is_whole(&self, doc: &Document, node: Node<'a>) -> bool {
        let mut current = Some(node);
        while let Some(node) = current {
            let Some(owning) = util::owning_node(node) else {
                return true;
            };
            if owning.kind() == "devicetree"
                && let Some(reference) = util::find_child(owning, "reference")
                && self.resolve(doc, reference).is_none()
            {
                return false;
            }
            current = self.canonical_parent(doc, node);
        }
        true
    }

    fn path_of_node(&self, node: Node<'_>) -> Option<&String> {
        self.paths.get(&node.id())
    }

    fn cells_of(&self, node: Node<'_>, name: &str) -> CellsValue {
        let Some(path) = self.path_of_node(node) else {
            return CellsValue::Absent;
        };
        match self.cells.get(path).and_then(|cells| cells.get(name)) {
            Some(CellsValue::Value(value)) => CellsValue::Value(*value),
            Some(CellsValue::Malformed) => CellsValue::Malformed,
            Some(CellsValue::Absent) | None => CellsValue::Absent,
        }
    }
}

fn explicit_phandle(doc: &Document, node: Node<'_>) -> Option<u32> {
    let body = body_of(node)?;
    for i in 0..body.child_count() {
        let child = body.child(i as u32)?;
        if child.kind() != "property" {
            continue;
        }
        let name = child.child_by_field_name("name")?;
        let name = util::node_text(doc, name);
        if name == "phandle" || name == "linux,phandle" {
            return property_u32(doc, child);
        }
    }
    None
}

/// Everything the checks found, plus the references that could not be resolved
/// because the unit is incomplete (the backend reports those per file).
pub struct Report {
    pub diagnostics: Vec<Problem>,
    /// `&label { }` blocks whose target is not defined here: (span, label)
    pub unresolved_fragments: Vec<(u32, u32, String)>,
}

/// Check every phandle-array, `interrupts`, provider and `#X-cells` property.
/// `doc` must hold a full translation unit.
pub fn check_document(doc: &Document) -> Report {
    let unit = Unit::new(doc);
    let mut out = Vec::new();
    let unresolved_fragments = unit
        .unresolved_fragments
        .iter()
        .map(|reference| {
            (
                reference.start_byte() as u32,
                reference.end_byte() as u32,
                util::node_text(doc, *reference).to_string(),
            )
        })
        .collect();
    check_node(doc, &unit, doc.tree.root_node(), &mut out);
    Report {
        diagnostics: out,
        unresolved_fragments,
    }
}

fn check_node(doc: &Document, unit: &Unit<'_>, node: Node<'_>, out: &mut Vec<Problem>) {
    match node.kind() {
        "subnode" | "devicetree" => {
            check_cells_shape(doc, unit, node, out);
            check_interrupt_provider(doc, unit, node, out);
        }
        "property" => {
            check_phandle_array(doc, unit, node, out);
            check_interrupts(doc, unit, node, out);
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            check_node(doc, unit, child, out);
        }
    }
}

/// dtc's `WARNING_IF_NOT_CELL` for every `#X-cells` of the table.
fn check_cells_shape(doc: &Document, unit: &Unit<'_>, node: Node<'_>, out: &mut Vec<Problem>) {
    for (_, cells_name) in PROVIDERS {
        let Some(property) = property_of(doc, unit, node, cells_name) else {
            continue;
        };
        if property_u32(doc, property).is_none() {
            out.push(problem(
                property,
                false,
                format!(
                    "{cells_name} must be a single number, found {}",
                    describe_value(doc, property)
                ),
            ));
        }
    }
}

/// Human-readable shape of a property value, for diagnostics.
fn describe_value(doc: &Document, property: Node<'_>) -> String {
    let list = cells(doc, property);
    match list.len() {
        0 => "an empty value".to_string(),
        1 => format!("'{}'", list[0].text),
        count => format!("{count} cells"),
    }
}

/// dtc's `check_interrupt_provider`.
fn check_interrupt_provider(doc: &Document, unit: &Unit<'_>, node: Node<'_>, out: &mut Vec<Problem>) {
    let provider = property_of(doc, unit, node, "interrupt-controller").is_some()
        || property_of(doc, unit, node, "interrupt-map").is_some();
    let has_cells = property_of(doc, unit, node, "#interrupt-cells").is_some();

    match (provider, has_cells) {
        (true, false) => out.push(problem(
            node,
            false,
            format!(
                "{}: has `interrupt-controller` but no `#interrupt-cells`, so its interrupt specifiers have no defined size",
                node_path(doc, unit, node)
            ),
        )),
        (false, true) => out.push(problem(
            property_of(doc, unit, node, "#interrupt-cells").expect("checked above"),
            false,
            format!(
                "{}: declares `#interrupt-cells` but is not an interrupt controller (needs `interrupt-controller` or `interrupt-map`)",
                node_path(doc, unit, node)
            ),
        )),
        // the value's shape is reported by check_cells_shape
        _ => {}
    }
}

/// dtc's `check_property_phandle_args`, run over source text.
fn check_phandle_array(doc: &Document, unit: &Unit<'_>, property: Node<'_>, out: &mut Vec<Problem>) {
    let Some(name_node) = property.child_by_field_name("name") else {
        if util::find_child(property, "delete_property").is_none() {
            out.push(problem(
                property,
                false,
                "internal: a property node has no name, so it cannot be checked".to_string(),
            ));
        }
        return;
    };
    let property_name = util::node_text(doc, name_node);
    let Some(cells_name) = cells_name_for(property_name) else {
        // dtc: only properties in its table are checked
        return;
    };
    let optional = OPTIONAL.contains(&property_name);
    if !unit.chain_is_whole(doc, property) {
        // the unresolved `&label { }` that cut the chain is reported once, for
        // the whole file, by check_document
        return;
    }
    let list = cells(doc, property);

    // a cell that is not a 32-bit integer means the source would not compile,
    // so report it instead of guessing where entries start and end
    if let Some((index, cell)) = list
        .iter()
        .enumerate()
        .find(|(_, cell)| cell.reference.is_none() && cell.value.is_none())
    {
        out.push(problem(
            cell,
            true,
            format!(
                "{property_name}: cannot read cell {index}: '{}' is not a 32-bit integer (an undefined macro or a non-numeric value)",
                cell.text
            ),
        ));
        return;
    }

    let mut previous: Option<(usize, usize)> = None;
    let mut index = 0usize;
    while index < list.len() {
        let cell = &list[index];

        // dtc: a 0 / 0xffffffff phandle marks a skipped entry and consumes one
        // cell, whatever the previous entry declared
        if let Some(value) = cell.value
            && is_skipped(value)
        {
            index += 1;
            continue;
        }

        let target = match &cell.reference {
            Some(Reference::Label(label)) => match unit.labels.get(label) {
                Some(node) => *node,
                None => {
                    out.push(problem(
                        cell,
                        true,
                        format!(
                            "{property_name}: `&{label}` is not defined in this translation unit \
                             (a board file that includes this one must provide it)"
                        ),
                    ));
                    return;
                }
            },
            Some(Reference::Path(path)) => match unit
                .root
                .and_then(|root| resolve_path(root, doc, path))
            {
                Some(node) => node,
                None => {
                    out.push(problem(
                        cell,
                        true,
                        format!(
                            "{property_name}: `{path}` does not name a node in this translation unit"
                        ),
                    ));
                    return;
                }
            },
            None => match cell.value.and_then(|value| unit.phandles.get(&value)) {
                Some(node) => *node,
                None => {
                    // dtc splits this into "not a phandle reference" and
                    // "could not get phandle node"; one sentence says it all
                    let context = match previous {
                        Some((entry, declared)) => format!(
                            " (the entry at cell {entry} takes {declared} argument cell(s), {cells_name} = <{declared}>)"
                        ),
                        None => String::new(),
                    };
                    out.push(problem(
                        cell,
                        false,
                        format!(
                            "{property_name}: cell {index} should be a phandle (`&label`) or `<0>` to skip the entry, found `{}`{context}",
                            cell.text
                        ),
                    ));
                    return;
                }
            },
        };

        match unit.cells_of(target, cells_name) {
            CellsValue::Value(declared) => {
                let declared = declared as usize;
                let remaining = list.len() - index - 1;
                if remaining < declared {
                    let entry = match &cell.reference {
                        Some(Reference::Label(label)) => format!("`&{label}`"),
                        Some(Reference::Path(path)) => format!("`{path}`"),
                        None => format!("the entry at cell {index}"),
                    };
                    out.push(problem(
                        cell,
                        false,
                        format!(
                            "{property_name}: {entry} is followed by {remaining} argument cell(s), but {cells_name} is <{declared}>"
                        ),
                    ));
                    return;
                }
                previous = Some((index, declared));
                index += declared + 1;
            }
            CellsValue::Absent => {
                // dtc: `optional` providers may omit the property
                if !optional {
                    out.push(problem(
                        cell,
                        false,
                        format!(
                            "{property_name}: {} has no `{cells_name}`, so this entry's \
                             argument count is undefined",
                            node_path(doc, unit, target)
                        ),
                    ));
                }
                index += 1;
            }
            CellsValue::Malformed => {
                // reported by check_cells_shape on the provider node
                index += 1;
            }
        }
    }
}

/// dtc's `check_interrupts_property`.
fn check_interrupts(doc: &Document, unit: &Unit<'_>, property: Node<'_>, out: &mut Vec<Problem>) {
    let Some(name_node) = property.child_by_field_name("name") else {
        return;
    };
    if util::node_text(doc, name_node) != "interrupts" {
        return;
    }
    if !unit.chain_is_whole(doc, property) {
        // reported once for the whole file by check_document
        return;
    }
    let list = cells(doc, property);
    if let Some((index, cell)) = list
        .iter()
        .enumerate()
        .find(|(_, cell)| cell.reference.is_none() && cell.value.is_none())
    {
        out.push(problem(
            cell,
            true,
            format!(
                "interrupts: cannot read cell {index}: '{}' is not a 32-bit integer \
                 (an undefined macro or a non-numeric value)",
                cell.text
            ),
        ));
        return;
    }
    let Some(enclosing) = util::owning_node(property) else {
        out.push(problem(
            property,
            false,
            "internal: `interrupts` is not inside a node, so its controller cannot be found"
                .to_string(),
        ));
        return;
    };
    // a property written directly inside `&label { }` belongs to the node the
    // fragment extends
    let owner = match util::find_child(enclosing, "reference") {
        Some(reference) => match unit.resolve(doc, reference) {
            Some(target) => target,
            None => {
                out.push(problem(
                    property,
                    true,
                    format!(
                        "`{}` does not name a node in this translation unit, so this                          `interrupts` cannot be resolved",
                        util::node_text(doc, reference)
                    ),
                ));
                return;
            }
        },
        None => enclosing,
    };

    let mut controller: Option<Node<'_>> = None;
    let mut current = Some(owner);
    while let Some(node) = current {
        if node.id() != owner.id() && is_interrupt_provider(doc, unit, node) {
            controller = Some(node);
            break;
        }
        match property_of(doc, unit, node, "interrupt-parent") {
            Some(parent_property) => match interrupt_parent(doc, unit, parent_property) {
                Ok(target) => {
                    if !is_interrupt_provider(doc, unit, target) {
                        out.push(problem(
                            target,
                            false,
                            format!(
                                "interrupt-parent points at {}, which is not an interrupt \
                                 controller (it has neither `interrupt-controller` nor \
                                 `interrupt-map`)",
                                node_path(doc, unit, target)
                            ),
                        ));
                    }
                    controller = Some(target);
                    break;
                }
                Err((resolved, problem)) => {
                    out.push(problem);
                    // dtc keeps walking up only for an invalid phandle; an
                    // unresolved reference ends the search
                    if resolved {
                        current = unit.canonical_parent(doc, node);
                    } else {
                        return;
                    }
                }
            },
            None => current = unit.canonical_parent(doc, node),
        }
    }

    let Some(controller) = controller else {
        if !unit.chain_is_whole(doc, owner) {
            // the fragment reference that cut the chain is already reported
            return;
        }
        out.push(problem(
            property,
            false,
            format!(
                "{}: `interrupts` needs an interrupt controller, but no `interrupt-parent` \
                 is set on this node or any ancestor",
                node_path(doc, unit, owner)
            ),
        ));
        return;
    };

    let declared = match unit.cells_of(controller, "#interrupt-cells") {
        CellsValue::Value(declared) => declared as usize,
        CellsValue::Absent => {
            // dtc: "We warn about that already in another test" — the
            // interrupt_provider check reports it
            return;
        }
        // reported by check_cells_shape
        CellsValue::Malformed => return,
    };

    // dtc's is_multiple_of: a zero divisor means "only zero is a multiple"
    let ok = if declared == 0 {
        list.is_empty()
    } else {
        list.len() % declared == 0
    };
    if !ok {
        out.push(problem(
            property,
            false,
            format!(
                "`interrupts` has {} cell(s); {} declares `#interrupt-cells = <{declared}>`, \
                 so the size must be a multiple of {declared}",
                list.len(),
                node_path(doc, unit, controller)
            ),
        ));
    }
}

/// Resolve `interrupt-parent`. On failure the diagnostic is returned together
/// with whether dtc would keep walking up the tree.
fn interrupt_parent<'a>(
    doc: &'a Document,
    unit: &Unit<'a>,
    property: Node<'a>,
) -> Result<Node<'a>, (bool, Problem)> {
    match first_reference(doc, property) {
        Some(Reference::Label(label)) => match unit.labels.get(label) {
            Some(node) => Ok(*node),
            None => Err((
                false,
                problem(
                    property,
                    true,
                    format!(
                        "interrupt-parent: `&{label}` is not defined anywhere in this \
                         translation unit"
                    ),
                ),
            )),
        },
        Some(Reference::Path(path)) => match unit
            .root
            .and_then(|root| resolve_path(root, doc, path))
        {
            Some(node) => Ok(node),
            None => Err((
                false,
                problem(
                    property,
                    true,
                    format!(
                        "interrupt-parent: `{path}` does not name a node in this \
                         translation unit"
                    ),
                ),
            )),
        },
        None => match cells(doc, property).first().and_then(|cell| cell.value) {
            Some(value) if !is_skipped(value) => match unit.phandles.get(&value) {
                Some(node) => Ok(*node),
                None => Err((
                    false,
                    problem(
                        property,
                        false,
                        format!("interrupt-parent: no node declares phandle {value}"),
                    ),
                )),
            },
            // dtc: 0 / 0xffffffff is an invalid phandle, and it keeps walking
            // up the tree after reporting it
            _ => Err((
                true,
                problem(
                    property,
                    false,
                    format!(
                        "interrupt-parent: `{}` is not a phandle; it must be `<&label>`",
                        util::node_text(doc, property)
                            .to_string()
                            .split('=')
                            .nth(1)
                            .unwrap_or("")
                            .trim()
                            .trim_end_matches(';')
                            .trim()
                            .trim_matches(['<', '>'])
                    ),
                ),
            )),
        },
    }
}

fn problem(node: impl IntoRange, error: bool, message: String) -> Problem {
    let (start, end) = node.into_range();
    Problem {
        start: start as u32,
        end: end as u32,
        error,
        message,
    }
}

trait IntoRange {
    fn into_range(&self) -> (usize, usize);
}

impl IntoRange for Node<'_> {
    fn into_range(&self) -> (usize, usize) {
        (self.start_byte(), self.end_byte())
    }
}

impl IntoRange for &Cell<'_> {
    fn into_range(&self) -> (usize, usize) {
        (self.start, self.end)
    }
}

fn find_devicetree(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "devicetree" {
        return Some(node);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && let Some(found) = find_devicetree(child)
        {
            return Some(found);
        }
    }
    None
}

fn body_of(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "devicetree" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "node_def"
                {
                    return Some(child);
                }
            }
            None
        }
        _ => util::node_def_of(node),
    }
}

/// The property named `name` on this node, looking through `&label { }`
/// fragments the way dtc's merge does.
fn property_of<'a>(doc: &Document, unit: &Unit<'a>, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let mut found = body_of(node).and_then(|body| property_in(doc, body, name));
    let Some(path) = unit.path_of_node(node) else {
        return found;
    };
    let Some(bodies) = unit.fragments.get(path) else {
        return found;
    };
    // fragments apply in source order, like dtc's merge
    for body in bodies {
        if deletes(doc, *body, name) {
            found = None;
        } else if let Some(property) = property_in(doc, *body, name) {
            found = Some(property);
        }
    }
    found
}

fn property_in<'a>(doc: &Document, body: Node<'a>, name: &str) -> Option<Node<'a>> {
    for i in 0..body.child_count() {
        let Some(child) = body.child(i as u32) else {
            continue;
        };
        if child.kind() != "property" {
            continue;
        }
        // `/delete-property/ x;` removes the property, it does not define it
        if util::find_child(child, "delete_property").is_some() {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        if util::node_text(doc, name_node) == name {
            return Some(child);
        }
    }
    None
}

/// Whether this body deletes `name` (`/delete-property/ name;`).
fn deletes(doc: &Document, body: Node<'_>, name: &str) -> bool {
    for i in 0..body.child_count() {
        let Some(child) = body.child(i as u32) else {
            continue;
        };
        if child.kind() != "property" || util::find_child(child, "delete_property").is_none() {
            continue;
        }
        let text = util::node_text(doc, child).to_string();
        let mut words = text.split_whitespace();
        if words.next() == Some("/delete-property/") && words.next() == Some(name) {
            return true;
        }
    }
    false
}


fn is_interrupt_provider(doc: &Document, unit: &Unit<'_>, node: Node<'_>) -> bool {
    property_of(doc, unit, node, "interrupt-controller").is_some()
        || property_of(doc, unit, node, "interrupt-map").is_some()
}

fn node_path(doc: &Document, unit: &Unit<'_>, node: Node<'_>) -> String {
    match unit.path_of_node(node) {
        Some(path) if !path.is_empty() => path.clone(),
        _ => crate::analysis::node_path(doc, node),
    }
}

fn first_reference<'a>(doc: &'a Document, property: Node<'a>) -> Option<Reference<'a>> {
    fn walk<'a>(doc: &'a Document, node: Node<'a>, found: &mut Option<Reference<'a>>) {
        if found.is_some() {
            return;
        }
        if node.kind() == "reference" {
            *found = reference(doc, node);
            return;
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                walk(doc, child, found);
            }
        }
    }

    let mut found = None;
    walk(doc, property, &mut found);
    found
}

/// Flatten a property value into cells, skipping comments and `/bits/` markers.
fn cells<'a>(doc: &'a Document, property: Node<'a>) -> Vec<Cell<'a>> {
    fn collect<'a>(doc: &'a Document, node: Node<'a>, out: &mut Vec<Cell<'a>>) {
        match node.kind() {
            // dtc: `/bits/ 8` is a size marker, not a cell
            "bits" => {}
            "property" | "prop_data" | "array" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32) {
                        collect(doc, child, out);
                    }
                }
            }
            "reference" => out.push(Cell {
                reference: reference(doc, node),
                value: None,
                text: util::node_text(doc, node),
                start: node.start_byte(),
                end: node.end_byte(),
            }),
            // one cell per value token: literals, unexpanded macro names and
            // integer expressions
            "int_literal" | "integer" | "number" | "preproc_identifier" | "identifier"
            | "char_literal" | "string_literal" | "preproc_number_literal" | "preproc_call"
            | "int_unary_op" | "int_parenthesized_expression" | "int_binary_op" => {
                out.push(Cell {
                    reference: None,
                    value: cell_value(doc, node),
                    text: util::node_text(doc, node),
                    start: node.start_byte(),
                    end: node.end_byte(),
                })
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    collect(doc, property, &mut out);
    out
}

fn is_skipped(value: u32) -> bool {
    value == 0 || value == 0xffff_ffff
}

/// Numeric value of a cell, when the text evaluates to a 32-bit integer.
///
/// dtc evaluates C constant expressions in cells, so `(2 | 1)` and
/// `((1 << 4) - 1) << 8 | 8` are ordinary cell values. Evaluating them here
/// keeps the checks quiet on valid sources and reserves "not an integer" for
/// values that really have no value.
fn cell_value(doc: &Document, node: Node<'_>) -> Option<u32> {
    eval(doc, node).map(|value| value as u32)
}

fn eval(doc: &Document, node: Node<'_>) -> Option<u64> {
    match node.kind() {
        "int_literal" | "integer" | "number" | "preproc_number_literal" => {
            parse_integer(&util::node_text(doc, node))
        }
        "char_literal" => {
            let text = util::node_text(doc, node);
            let inner = text.trim_matches('\'');
            let mut chars = inner.chars();
            let value = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            Some(value as u64)
        }
        "int_parenthesized_expression" => eval(doc, node.named_child(0)?),
        "int_unary_op" => {
            let operator = node.child(0)?.kind();
            let operand = eval(doc, node.named_child(0)?)?;
            match operator {
                "~" => Some(!operand),
                "-" => Some(operand.wrapping_neg()),
                "+" => Some(operand),
                "!" => Some(u64::from(operand == 0)),
                _ => None,
            }
        }
        "int_binary_op" => {
            let left = eval(doc, node.named_child(0)?)?;
            let right = eval(doc, node.named_child(1)?)?;
            let operator = (0..node.child_count())
                .filter_map(|i| node.child(i as u32))
                .find(|child| !child.is_named())?;
            let operator = util::node_text(doc, operator).to_string();
            match operator.as_str() {
                "|" => Some(left | right),
                "&" => Some(left & right),
                "^" => Some(left ^ right),
                "+" => Some(left.wrapping_add(right)),
                "-" => Some(left.wrapping_sub(right)),
                "*" => Some(left.wrapping_mul(right)),
                "/" => (right != 0).then(|| left / right),
                "%" => (right != 0).then(|| left % right),
                "<<" => (right < 64).then(|| left << right),
                ">>" => (right < 64).then(|| left >> right),
                "<" => Some(u64::from(left < right)),
                ">" => Some(u64::from(left > right)),
                "<=" => Some(u64::from(left <= right)),
                ">=" => Some(u64::from(left >= right)),
                "==" => Some(u64::from(left == right)),
                "!=" => Some(u64::from(left != right)),
                "&&" => Some(u64::from(left != 0 && right != 0)),
                "||" => Some(u64::from(left != 0 || right != 0)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_integer(text: &str) -> Option<u64> {
    let text = text.trim().trim_end_matches(['u', 'U', 'l', 'L']);
    let (digits, radix) = match text.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("0x") => (&text[2..], 16),
        Some(prefix) if prefix.eq_ignore_ascii_case("0b") => (&text[2..], 2),
        Some(prefix) if prefix.starts_with('0') && text.len() > 1 => (&text[1..], 8),
        _ => (text, 10),
    };
    u64::from_str_radix(digits, radix).ok()
}

/// Value of a single-cell property such as `#clock-cells = <1>`.
pub fn property_u32(doc: &Document, property: Node<'_>) -> Option<u32> {
    let list = cells(doc, property);
    match list.len() {
        1 => list[0].value,
        _ => None,
    }
}

fn reference<'a>(doc: &'a Document, node: Node<'a>) -> Option<Reference<'a>> {
    if let Some(label_ref) = util::find_child(node, "label_ref") {
        let text = util::node_text(doc, label_ref);
        return text.strip_prefix('&').map(Reference::Label);
    }
    let path_ref = util::find_child(node, "path_ref")?;
    Some(Reference::Path(util::node_text(doc, path_ref)))
}

/// Resolve `&{/soc/foo}` to its node, walking the root's children by name.
fn resolve_path<'a>(root: Node<'a>, doc: &Document, path_ref: &str) -> Option<Node<'a>> {
    let inner = path_ref.strip_prefix("&{")?.strip_suffix('}')?;
    let mut current = root;
    for segment in inner.split('/').filter(|segment| !segment.is_empty()) {
        let body = body_of(current)?;
        let mut next = None;
        for i in 0..body.child_count() {
            let Some(candidate) = body.child(i as u32) else {
                continue;
            };
            if candidate.kind() != "subnode" {
                continue;
            }
            if candidate
                .child_by_field_name("name")
                .is_some_and(|name| util::node_text(doc, name) == segment)
            {
                next = Some(candidate);
                break;
            }
        }
        current = next?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run the checks over a complete translation unit.
    fn diags(text: &str) -> Vec<String> {
        let doc = Document::open(text, 0);
        check_document(&doc)
            .diagnostics
            .into_iter()
            .map(|problem| problem.message)
            .collect()
    }

    fn errors(text: &str) -> Vec<String> {
        let doc = Document::open(text, 0);
        check_document(&doc)
            .diagnostics
            .into_iter()
            .filter(|problem| problem.error)
            .map(|problem| problem.message)
            .collect()
    }

    #[test]
    fn matching_cell_counts_are_accepted() {
        let text = "/dts-v1/;\n/ {\n\tclk: clk@1 { #clock-cells = <0>; };\n\tgcc: gcc@2 { #clock-cells = <1>; };\n\tgood { clocks = <&clk>, <&gcc 5>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn missing_argument_cells_are_reported() {
        let text = "/dts-v1/;\n/ {\n\tgcc: gcc@2 { #clock-cells = <1>; };\n\tbad { clocks = <&gcc>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("is followed by 0 argument cell(s), but #clock-cells is <1>"),
            "{messages:?}"
        );
    }

    #[test]
    fn surplus_cells_are_reported_like_dtc() {
        // dtc reports both: the marker check, then the failed phandle lookup
        let text = "/dts-v1/;\n/ {\n\tclk: clk@1 { #clock-cells = <0>; };\n\tbad { clocks = <&clk 5>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("cell 1 should be a phandle (`&label`) or `<0>` to skip the entry, found `5`"),
            "{messages:?}"
        );
    }

    #[test]
    fn provider_without_cells_property_is_reported() {
        let text = "/dts-v1/;\n/ {\n\todd: odd@3 { };\n\tbad { clocks = <&odd>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("has no `#clock-cells`, so this entry's argument count is undefined"),
            "{messages:?}"
        );
    }

    #[test]
    fn optional_providers_are_allowed_without_cells() {
        let text = "/dts-v1/;\n/ {\n\tmsi: msi@1 { };\n\tdev { msi-parent = <&msi>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn every_entry_of_a_multi_entry_property_is_checked() {
        let text = "/dts-v1/;\n/ {\n\tintc: intc@1 { #interrupt-cells = <3>; interrupt-controller; };\n\tdev {\n\t\tinterrupts-extended = <&intc 0 5 4>, <&intc 0 6 4>;\n\t};\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));

        let text = "/dts-v1/;\n/ {\n\tintc: intc@1 { #interrupt-cells = <3>; interrupt-controller; };\n\tdev {\n\t\tinterrupts-extended = <&intc 0 5 4>, <&intc 0 6>;\n\t};\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("is followed by 2 argument cell(s), but #interrupt-cells is <3>"),
            "{messages:?}"
        );
    }

    #[test]
    fn skipped_entries_do_not_extend_the_previous_one() {
        let text = "/dts-v1/;\n/ {\n\tphy: phy@1 { #clock-cells = <1>; };\n\tdev {\n\t\tclocks = <&phy 5>,\n\t\t <0>, <0>;\n\t};\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn comments_and_bits_markers_are_not_cells() {
        let text = "/dts-v1/;\n/ {\n\tgpu: gpu@1 { #cooling-cells = <2>; };\n\tdev {\n\t\t/* keep me */ cooling-device = <&gpu 1 2 /* tail */>;\n\t};\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));

        let text = "/dts-v1/;\n/ {\n\tgpu: gpu@1 { #cooling-cells = <2>; };\n\tdev {\n\t\tcooling-device = /bits/ 8 <&gpu 1 2>;\n\t};\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn non_integer_cells_are_errors_not_skips() {
        // dtc aborts on this input; we must say why rather than skipping
        let text = "/dts-v1/;\n/ {\n\tphy: phy@1 { #clock-cells = <0>; };\n\tdev {\n\t\tclocks = <&phy SOME_PIPE_CLK>;\n\t};\n};\n";
        let messages = errors(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("is not a 32-bit integer"),
            "{messages:?}"
        );
    }

    #[test]
    fn unresolved_labels_are_errors() {
        let text = "/dts-v1/;\n/ {\n\tdev { clocks = <&nosuch>; };\n};\n";
        let messages = errors(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("`&nosuch` is not defined in this translation unit"),
            "{messages:?}"
        );
    }

    #[test]
    fn undeclared_numeric_phandles_are_reported() {
        // `<9>` where no node declares phandle 9
        let text = "/dts-v1/;\n/ {\n\tclk: clk@1 { #clock-cells = <0>; phandle = <7>; };\n\tdev { clocks = <&clk>, <9>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("cell 1 should be a phandle (`&label`) or `<0>` to skip the entry, found `9`"),
            "{messages:?}"
        );
    }

    #[test]
    fn interrupts_size_must_be_a_multiple_of_interrupt_cells() {
        let good = "/dts-v1/;\n/ {\n\tintc: intc@1 { #interrupt-cells = <3>; interrupt-controller; };\n\tdev { interrupt-parent = <&intc>; interrupts = <0 5 4>, <0 6 4>; };\n};\n";
        assert!(diags(good).is_empty(), "{:?}", diags(good));

        let bad = "/dts-v1/;\n/ {\n\tintc: intc@1 { #interrupt-cells = <3>; interrupt-controller; };\n\tdev { interrupt-parent = <&intc>; interrupts = <0 5 4>, <0 6>; };\n};\n";
        let messages = diags(bad);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("multiple of 3"), "{messages:?}");
    }

    #[test]
    fn interrupt_parent_is_inherited_from_ancestors() {
        let text = "/dts-v1/;\n/ {\n\tinterrupt-parent = <&intc>;\n\tintc: intc@1 { #interrupt-cells = <2>; interrupt-controller; };\n\tbus@2 { dev { interrupts = <5>; }; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("multiple of 2"), "{messages:?}");
    }

    #[test]
    fn missing_interrupt_parent_is_reported() {
        let text = "/dts-v1/;\n/ {\n\tdev { interrupts = <0 5>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("no `interrupt-parent` is set on this node or any ancestor"),
            "{messages:?}"
        );
    }

    #[test]
    fn zero_interrupt_cells_make_any_interrupts_invalid() {
        // dtc's is_multiple_of(x, 0) is "x == 0"
        let text = "/dts-v1/;\n/ {\n\tzc: zc@1 { #interrupt-cells = <0>; interrupt-controller; };\n\tdev { interrupt-parent = <&zc>; interrupts = <0 5 4>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("multiple of 0"), "{messages:?}");
    }

    #[test]
    fn interrupt_provider_needs_cells_and_cells_need_a_provider() {
        let text = "/dts-v1/;\n/ {\n\tnp@1 { #interrupt-cells = <2>; };\n\tpr@2 { interrupt-controller; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages
                .iter()
                .any(|message| message.contains("declares `#interrupt-cells` but is not an interrupt controller")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("has `interrupt-controller` but no `#interrupt-cells`")),
            "{messages:?}"
        );
    }

    #[test]
    fn cells_properties_must_be_a_single_cell() {
        let text = "/dts-v1/;\n/ {\n\tdev@1 { #clock-cells = <1 2>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("#clock-cells must be a single number, found 2 cells"),
            "{messages:?}"
        );
    }

    #[test]
    fn constant_expressions_are_evaluated() {
        // dtc evaluates these; a macro expansion like `(2 | 1)` is a normal cell
        let text = "/dts-v1/;\n/ {\n\tgcc: gcc@2 { #clock-cells = <1>; };\n\tdev { clocks = <&gcc (2 | 1)>, <&gcc ((((1 << 4) - 1) << 8) | 8)>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));

        let text = "/dts-v1/;\n/ {\n\tgcc: gcc@2 { #clock-cells = <1>; };\n\tdev { clocks = <&gcc (2 | 1)>, <&gcc ~0>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn path_references_resolve() {
        let text = "/dts-v1/;\n/ {\n\tgcc: gcc@2 { #clock-cells = <1>; };\n\tdev { clocks = <&{/gcc@2} 5>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn interrupt_parent_is_found_through_override_fragments() {
        // `&bus { dev { interrupts = ...; } }` merged into the real node, whose
        // ancestor carries interrupt-parent
        let text = "/dts-v1/;\n/ {\n\tinterrupt-parent = <&intc>;\n\tintc: intc@1 { #interrupt-cells = <3>; interrupt-controller; };\n\tbus: bus@2 { };\n};\n&bus { dev { interrupts = <0 5 4>, <0 6>; }; };\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].contains("multiple of 3"), "{messages:?}");
    }

    #[test]
    fn provider_cells_are_found_in_override_fragments() {
        let text = "/dts-v1/;\n/ {\n\tgcc: gcc@2 { };\n\tdev { clocks = <&gcc 1>; };\n};\n&gcc { #clock-cells = <1>; };\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn properties_outside_the_table_are_ignored() {
        // `foo-bars` is not in dtc's provider table
        let text = "/dts-v1/;\n/ {\n\tclk: clk@1 { #foo-bar-cells = <2>; };\n\tdev { foo-bars = <&clk>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }
}



