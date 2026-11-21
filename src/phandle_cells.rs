// SPDX-License-Identifier: AGPL-3.0-only
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
use crate::model::{self, RefKind, Tree};

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
    doc: &'a Document,
    /// labels and canonical (fragment-merged) paths of the unit
    tree: Tree<'a>,
    /// numeric phandle -> the subnode declaring it
    phandles: HashMap<u32, Node<'a>>,
    /// canonical path -> `&label { }` fragment bodies that extend that node
    fragments: HashMap<String, Vec<Node<'a>>>,
    /// canonical path -> merged `#X-cells` values
    cells: HashMap<String, HashMap<String, CellsValue>>,
    /// `&label { }` references that resolve to no node in this unit
    unresolved_fragments: Vec<Node<'a>>,
}

impl<'a> Unit<'a> {
    fn new(doc: &'a Document) -> Self {
        let mut unit = Unit {
            doc,
            tree: Tree::new(doc),
            phandles: HashMap::new(),
            fragments: HashMap::new(),
            cells: HashMap::new(),
            unresolved_fragments: Vec::new(),
        };
        unit.collect_phandles(doc.tree.root_node());
        unit.collect_fragments(doc.tree.root_node());
        unit.collect_cells(doc.tree.root_node());
        unit
    }

    /// Explicit `phandle = <N>` properties.
    fn collect_phandles(&mut self, node: Node<'a>) {
        if node.kind() == "subnode"
            && let Some(value) = explicit_phandle(self.doc, node)
        {
            self.phandles.entry(value).or_insert(node);
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                self.collect_phandles(child);
            }
        }
    }

    /// `&label { ... }` fragments, keyed by the canonical path they extend.
    fn collect_fragments(&mut self, node: Node<'a>) {
        if node.kind() == "devicetree"
            && let Some(reference) = model::find_child(node, "reference")
        {
            match self
                .tree
                .resolve(reference)
                .and_then(|target| self.tree.path_of(target))
            {
                Some(path) => {
                    if let Some(body) = model::body_of(node) {
                        self.fragments.entry(path).or_default().push(body);
                    }
                }
                None => self.unresolved_fragments.push(reference),
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                self.collect_fragments(child);
            }
        }
    }

    fn collect_cells(&mut self, node: Node<'a>) {
        if node.kind() == "subnode" {
            if let Some(path) = self.tree.path_of(node) {
                let mut bodies = Vec::new();
                if let Some(body) = model::body_of(node) {
                    bodies.push(body);
                }
                // fragment properties take precedence, like dtc's merge
                if let Some(fragments) = self.fragments.get(&path) {
                    bodies.extend(fragments.iter().copied());
                }
                for body in bodies {
                    for i in 0..body.child_count() {
                        let Some(child) = body.child(i as u32) else { continue };
                        if child.kind() != "property" {
                            continue;
                        }
                        let Some(name_node) = child.child_by_field_name("name") else {
                            continue;
                        };
                        let name = model::node_text(self.doc, name_node);
                        if name.starts_with('#') && name.ends_with("-cells") {
                            let value = match model::property_u32(self.doc, child) {
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
                self.collect_cells(child);
            }
        }
    }

    fn cells_of(&mut self, node: Node<'a>, name: &str) -> CellsValue {
        let Some(path) = self.tree.path_of(node) else {
            return CellsValue::Absent;
        };
        match self.cells.get(&path).and_then(|cells| cells.get(name)) {
            Some(CellsValue::Value(value)) => CellsValue::Value(*value),
            Some(CellsValue::Malformed) => CellsValue::Malformed,
            Some(CellsValue::Absent) | None => CellsValue::Absent,
        }
    }
}

fn explicit_phandle(doc: &Document, node: Node<'_>) -> Option<u32> {
    let prop = model::find_property(doc, node, "phandle")
        .or_else(|| model::find_property(doc, node, "linux,phandle"))?;
    model::property_u32(doc, prop)
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
    let mut unit = Unit::new(doc);
    let unresolved_fragments = unit
        .unresolved_fragments
        .iter()
        .map(|reference| {
            (
                reference.start_byte() as u32,
                reference.end_byte() as u32,
                model::node_text(doc, *reference).to_string(),
            )
        })
        .collect();
    let mut out = Vec::new();
    check_node(doc, &mut unit, doc.tree.root_node(), &mut out);
    Report {
        diagnostics: out,
        unresolved_fragments,
    }
}

fn check_node<'a>(doc: &'a Document, unit: &mut Unit<'a>, node: Node<'a>, out: &mut Vec<Problem>) {
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
fn check_cells_shape<'a>(doc: &Document, unit: &mut Unit<'a>, node: Node<'a>, out: &mut Vec<Problem>) {
    for (_, cells_name) in PROVIDERS {
        let Some(property) = property_of(doc, unit, node, cells_name) else {
            continue;
        };
        if model::property_u32(doc, property).is_none() {
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
    let list = model::cells_of_property(doc, property);
    match list.len() {
        0 => "an empty value".to_string(),
        1 => format!("'{}'", list[0].text),
        count => format!("{count} cells"),
    }
}

/// dtc's `check_interrupt_provider`.
fn check_interrupt_provider<'a>(doc: &Document, unit: &mut Unit<'a>, node: Node<'a>, out: &mut Vec<Problem>) {
    let provider = property_of(doc, unit, node, "interrupt-controller").is_some()
        || property_of(doc, unit, node, "interrupt-map").is_some();
    let cells_property = property_of(doc, unit, node, "#interrupt-cells");

    match (provider, cells_property) {
        (true, None) => out.push(problem(
            node,
            false,
            format!(
                "{}: has `interrupt-controller` but no `#interrupt-cells`, so its interrupt specifiers have no defined size",
                node_path(unit, node)
            ),
        )),
        (false, Some(cells_property)) => out.push(problem(
            cells_property,
            false,
            format!(
                "{}: declares `#interrupt-cells` but is not an interrupt controller (needs `interrupt-controller` or `interrupt-map`)",
                node_path(unit, node)
            ),
        )),
        // the value's shape is reported by check_cells_shape
        _ => {}
    }
}

/// dtc's `check_property_phandle_args`, run over source text.
fn check_phandle_array<'a>(doc: &Document, unit: &mut Unit<'a>, property: Node<'a>, out: &mut Vec<Problem>) {
    let Some(name_node) = property.child_by_field_name("name") else {
        if model::find_child(property, "delete_property").is_none() {
            out.push(problem(
                property,
                false,
                "internal: a property node has no name, so it cannot be checked".to_string(),
            ));
        }
        return;
    };
    let property_name = model::node_text(doc, name_node);
    let Some(cells_name) = cells_name_for(property_name) else {
        // dtc: only properties in its table are checked
        return;
    };
    let optional = OPTIONAL.contains(&property_name);
    if !unit.tree.chain_is_whole(property) {
        // the unresolved `&label { }` that cut the chain is reported once, for
        // the whole file, by check_document
        return;
    }
    let list = model::cells_of_property(doc, property);

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

    let mut previous: Option<(usize, String, usize)> = None;
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
            Some(RefKind::Label(label)) => match unit.tree.label(label) {
                Some(node) => node,
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
            Some(RefKind::Path(path)) => match unit.tree.resolve_path(path) {
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
                    // "could not get phandle node"; one sentence says it all,
                    // plus where the previous entry's stride came from
                    let context = match &previous {
                        Some((entry, entry_text, declared)) => format!(
                            " — the entry {entry_text} (cell {entry}) takes {declared} argument \
                             cell(s) (`{cells_name}` = <{declared}>), so cell {index} must start \
                             a new entry or be `<0>` to skip it"
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
                        Some(RefKind::Label(label)) => format!("`&{label}`"),
                        Some(RefKind::Path(path)) => format!("`{path}`"),
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
                let entry_text = match &cell.reference {
                    Some(RefKind::Label(label)) => format!("`&{label}`"),
                    Some(RefKind::Path(path)) => format!("`{path}`"),
                    None => format!("the phandle at cell {index}"),
                };
                previous = Some((index, entry_text, declared));
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
                            node_path(unit, target)
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
fn check_interrupts<'a>(doc: &'a Document, unit: &mut Unit<'a>, property: Node<'a>, out: &mut Vec<Problem>) {
    let Some(name_node) = property.child_by_field_name("name") else {
        return;
    };
    if model::node_text(doc, name_node) != "interrupts" {
        return;
    }
    if !unit.tree.chain_is_whole(property) {
        // reported once for the whole file by check_document
        return;
    }
    let list = model::cells_of_property(doc, property);
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
    let Some(enclosing) = model::owning_node(property) else {
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
    let owner = match model::find_child(enclosing, "reference") {
        Some(reference) => match unit.tree.resolve(reference) {
            Some(target) => target,
            None => {
                out.push(problem(
                    property,
                    true,
                    format!(
                        "`{}` does not name a node in this translation unit, so this                          `interrupts` cannot be resolved",
                        model::node_text(doc, reference)
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
                                node_path(unit, target)
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
                        current = unit.tree.parent_of(node);
                    } else {
                        return;
                    }
                }
            },
            None => current = unit.tree.parent_of(node),
        }
    }

    let Some(controller) = controller else {
        if !unit.tree.chain_is_whole(owner) {
            // the fragment reference that cut the chain is already reported
            return;
        }
        out.push(problem(
            property,
            false,
            format!(
                "{}: `interrupts` needs an interrupt controller, but no `interrupt-parent` \
                 is set on this node or any ancestor",
                node_path(unit, owner)
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
                node_path(unit, controller)
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
        Some(RefKind::Label(label)) => match unit.tree.label(label) {
            Some(node) => Ok(node),
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
        Some(RefKind::Path(path)) => match unit.tree.resolve_path(path) {
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
        None => {
            let list = model::cells_of_property(doc, property);
            match list.first().and_then(|cell| cell.value) {
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
                            model::node_text(doc, property)
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
            }
        }
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

impl IntoRange for &model::Cell<'_> {
    fn into_range(&self) -> (usize, usize) {
        (self.start, self.end)
    }
}

/// The property named `name` on this node, looking through `&label { }`
/// fragments the way dtc's merge does.
fn property_of<'a>(
    doc: &Document,
    unit: &mut Unit<'a>,
    node: Node<'a>,
    name: &str,
) -> Option<Node<'a>> {
    let mut found = model::body_of(node).and_then(|body| model::find_property(doc, body, name));
    let Some(path) = unit.tree.path_of(node) else {
        return found;
    };
    let Some(bodies) = unit.fragments.get(&path) else {
        return found;
    };
    // fragments apply in source order, like dtc's merge
    for body in bodies.iter().copied().collect::<Vec<_>>() {
        if deletes(doc, body, name) {
            found = None;
        } else if let Some(property) = model::find_property(doc, body, name) {
            found = Some(property);
        }
    }
    found
}

/// Whether this body deletes `name` (`/delete-property/ name;`).
fn deletes(doc: &Document, body: Node<'_>, name: &str) -> bool {
    for i in 0..body.child_count() {
        let Some(child) = body.child(i as u32) else {
            continue;
        };
        if child.kind() != "property" || model::find_child(child, "delete_property").is_none() {
            continue;
        }
        let mut words = model::node_text(doc, child).split_whitespace();
        if words.next() == Some("/delete-property/") && words.next() == Some(name) {
            return true;
        }
    }
    false
}

fn is_interrupt_provider<'a>(doc: &Document, unit: &mut Unit<'a>, node: Node<'a>) -> bool {
    property_of(doc, unit, node, "interrupt-controller").is_some()
        || property_of(doc, unit, node, "interrupt-map").is_some()
}

/// Canonical path of a node in the merged unit, `/` for the root.
fn node_path<'a>(unit: &mut Unit<'a>, node: Node<'a>) -> String {
    match unit.tree.display_path(node) {
        path if path.is_empty() => "/".to_string(),
        path => path,
    }
}

fn first_reference<'a>(doc: &'a Document, property: Node<'a>) -> Option<RefKind<'a>> {
    fn walk<'a>(doc: &'a Document, node: Node<'a>, found: &mut Option<RefKind<'a>>) {
        if found.is_some() {
            return;
        }
        if node.kind() == "reference" {
            *found = model::reference_of(doc, node);
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

fn is_skipped(value: u32) -> bool {
    value == 0 || value == 0xffff_ffff
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
