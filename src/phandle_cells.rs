//! Phandle argument-count checks, mirroring dtc's `checks.c`
//! (`check_property_phandle_args`).
//!
//! dtc validates "consumer property vs provider `#*-cells`" for a fixed table
//! of property names (`WARNING_PROPERTY_PHANDLE_CELLS`). That table lives in
//! dtc itself — the binding YAML does not express it — so this is a direct
//! port of the same table and the same algorithm:
//!
//! * a property value is one flat cell array, scanned left to right;
//! * a phandle cell contributes itself plus the provider's `#X-cells` value as
//!   argument cells;
//! * a cell whose value is 0 or 0xffffffff is a skipped entry: it consumes one
//!   cell no matter what the previous entry declared;
//! * running past the end while an entry still expects arguments is dtc's
//!   "property size too small for cell size";
//! * where a phandle is expected, a known non-zero cell is dtc's "cell N is not
//!   a phandle reference" (dtc only checks that when the property carries at
//!   least one `&label` marker).
//!
//! When a cell's value cannot be determined (an unexpanded macro name) the
//! scan stays quiet: dtc evaluates the preprocessed value and we cannot, so
//! reporting would be a guess.

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

/// Cell values that make dtc skip an entry instead of following the phandle.
const SKIPPED_CELLS: &[u32] = &[0, 0xffff_ffff];

fn cells_name_for(property: &str) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .find(|(consumer, _)| *consumer == property)
        .map(|(_, cells)| *cells)
}

/// Provider data for a whole translation unit (so cross-file references
/// resolve): label -> node path, and node path -> `#X-cells` values.
pub struct UnitProviders<'a> {
    pub labels: &'a HashMap<String, String>,
    pub cells: &'a HashMap<String, HashMap<String, u32>>,
}

/// One cell of a phandle-array property.
struct Cell<'a> {
    /// label when the cell is a `&label` reference
    label: Option<&'a str>,
    /// numeric value when the text determines it
    value: Option<u32>,
    start: usize,
    end: usize,
}

/// Check every phandle-argument property of a document against the providers
/// declared in its unit. Returns (start, end, message) byte spans.
pub fn check_document(doc: &Document, providers: &UnitProviders<'_>) -> Vec<(u32, u32, String)> {
    let labels = label_nodes(doc);
    let mut out = Vec::new();
    check_node(doc, doc.tree.root_node(), providers, &labels, &mut out);
    out
}

fn check_node(
    doc: &Document,
    node: Node<'_>,
    providers: &UnitProviders<'_>,
    labels: &HashMap<String, Node<'_>>,
    out: &mut Vec<(u32, u32, String)>,
) {
    if node.kind() == "property" {
        check_property(doc, node, providers, out);
        check_interrupts(doc, node, labels, out);
        return;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            check_node(doc, child, providers, labels, out);
        }
    }
}

/// label -> subnode, for resolving `interrupt-parent` (and other single-label
/// phandles) to the node itself rather than to its path.
fn label_nodes(doc: &Document) -> HashMap<String, Node<'_>> {
    fn walk<'a>(doc: &Document, node: Node<'a>, out: &mut HashMap<String, Node<'a>>) {
        if node.kind() == "subnode" {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "label"
                {
                    let text = util::node_text(doc, child);
                    let name = text.trim_end_matches(':').trim();
                    if !name.is_empty() {
                        out.entry(name.to_string()).or_insert(node);
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                walk(doc, child, out);
            }
        }
    }

    let mut out = HashMap::new();
    walk(doc, doc.tree.root_node(), &mut out);
    out
}

/// dtc's `check_property_phandle_args`, run over the source text instead of
/// the compiled DTB.
fn check_property(
    doc: &Document,
    property: Node<'_>,
    providers: &UnitProviders<'_>,
    out: &mut Vec<(u32, u32, String)>,
) {
    let Some(name_node) = property.child_by_field_name("name") else {
        return;
    };
    let property_name = util::node_text(doc, name_node);
    let Some(cells_name) = cells_name_for(property_name) else {
        return;
    };
    let optional = OPTIONAL.contains(&property_name);

    let cells = cells(doc, property);
    let has_markers = cells.iter().any(|cell| cell.label.is_some());
    let mut index = 0usize;
    while index < cells.len() {
        let cell = &cells[index];
        let Some(label) = cell.label else {
            // a numeric cell: a skipped 0/0xffffffff entry, or a phandle we
            // cannot resolve from text
            index += 1;
            continue;
        };

        let Some(path) = providers.labels.get(label) else {
            index += 1;
            continue;
        };
        let declared = providers
            .cells
            .get(path)
            .and_then(|declared| declared.get(cells_name));
        let Some(declared) = declared else {
            if !optional {
                out.push((
                    cell.start as u32,
                    cell.end as u32,
                    format!(
                        "{}: {label} (node {path}): missing property '{cells_name}'",
                        property_name
                    ),
                ));
            }
            index += 1;
            continue;
        };
        let declared = *declared as usize;

        let remaining = cells.len() - index - 1;
        if remaining < declared {
            out.push((
                cell.start as u32,
                cells.last().map(|last| last.end).unwrap_or(cell.end) as u32,
                format!(
                    "{}: {label}: expected {declared} argument cell(s) after the phandle \
                     ({cells_name} = <{declared}>), found {remaining}",
                    property_name
                ),
            ));
            return;
        }

        // the cell right after this entry's arguments must itself be a phandle
        // (or a skipped 0/0xffffffff entry)
        let next = index + declared + 1;
        if has_markers
            && let Some(next_cell) = cells.get(next)
            && next_cell.label.is_none()
            && let Some(value) = next_cell.value
            && !SKIPPED_CELLS.contains(&value)
        {
            out.push((
                next_cell.start as u32,
                next_cell.end as u32,
                format!(
                    "{}: cell {next} is not a phandle reference (the entry at cell {index} \
                     takes {declared} argument cell(s), {cells_name} = <{declared}>)",
                    property_name
                ),
            ));
            return;
        }

        index += declared + 1;
    }
}

/// dtc's `check_interrupts_property`: `interrupts` takes no phandle, so its
/// size must be a whole multiple of the interrupt controller's
/// `#interrupt-cells`. The controller is found through `interrupt-parent` on
/// this node or on an ancestor, or by an ancestor that is itself an interrupt
/// provider — exactly dtc's search order.
fn check_interrupts(
    doc: &Document,
    property: Node<'_>,
    labels: &HashMap<String, Node<'_>>,
    out: &mut Vec<(u32, u32, String)>,
) {
    let Some(name_node) = property.child_by_field_name("name") else {
        return;
    };
    if util::node_text(doc, name_node) != "interrupts" {
        return;
    }
    let cells = cells(doc, property);
    if cells.is_empty() {
        return;
    }
    let Some(owner) = util::owning_node(property) else {
        return;
    };

    let mut current = Some(owner);
    let mut controller = None;
    while let Some(node) = current {
        if node.id() != owner.id() && is_interrupt_provider(doc, node) {
            controller = Some(node);
            break;
        }
        if let Some(parent_property) = property_of(doc, node, "interrupt-parent") {
            if let Some(label) = first_reference_label(doc, parent_property) {
                controller = labels.get(label).copied();
            }
            break;
        }
        current = parent_node(node);
    }

    // an unresolvable parent (`&label` from another unit, a phandle of 0) is
    // left alone rather than guessed at
    let Some(controller) = controller else {
        return;
    };
    let Some(declared) = property_of(doc, controller, "#interrupt-cells")
        .and_then(|declared| property_u32(doc, declared))
    else {
        return;
    };
    let declared = declared as usize;
    if declared == 0 || cells.len() % declared == 0 {
        return;
    }

    out.push((
        property.start_byte() as u32,
        property.end_byte() as u32,
        format!(
            "interrupts: size is {} cell(s), expected a multiple of {declared} \
             (#interrupt-cells = <{declared}>)",
            cells.len()
        ),
    ));
}

/// The node body that holds this node's properties.
fn body<'a>(doc: &Document, node: Node<'a>) -> Option<Node<'a>> {
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
        _ => {
            let _ = doc;
            util::node_def_of(node)
        }
    }
}

fn property_of<'a>(doc: &Document, node: Node<'a>, name: &str) -> Option<Node<'a>> {
    let body = body(doc, node)?;
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
        if util::node_text(doc, name_node) == name {
            return Some(child);
        }
    }
    None
}

/// The enclosing node (subnode, or the root devicetree).
fn parent_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut current = node.parent();
    while let Some(node) = current {
        match node.kind() {
            "subnode" | "devicetree" => return Some(node),
            _ => {}
        }
        current = node.parent();
    }
    None
}

fn is_interrupt_provider(doc: &Document, node: Node<'_>) -> bool {
    property_of(doc, node, "interrupt-controller").is_some()
        || property_of(doc, node, "interrupt-map").is_some()
}

fn first_reference_label<'a>(doc: &'a Document, property: Node<'a>) -> Option<&'a str> {
    fn walk<'a>(doc: &'a Document, node: Node<'a>, found: &mut Option<&'a str>) {
        if found.is_some() {
            return;
        }
        if node.kind() == "reference" {
            *found = reference_label(doc, node);
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

/// Flatten the property value into cells, skipping comments and `/bits/`
/// markers.
fn cells<'a>(doc: &'a Document, property: Node<'a>) -> Vec<Cell<'a>> {
    fn collect<'a>(doc: &'a Document, node: Node<'a>, out: &mut Vec<Cell<'a>>) {
        match node.kind() {
            // `/bits/ 8` is a size marker, not a cell
            "bits" => {}
            "property" | "prop_data" | "array" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i as u32) {
                        collect(doc, child, out);
                    }
                }
            }
            "reference" => out.push(Cell {
                label: reference_label(doc, node),
                value: None,
                start: node.start_byte(),
                end: node.end_byte(),
            }),
            // one cell per value token: literals, unexpanded macro names and
            // integer expressions
            "int_literal" | "integer" | "number" | "preproc_identifier" | "identifier"
            | "char_literal" | "string_literal" | "preproc_number_literal" | "preproc_call"
            | "int_unary_op" | "int_parenthesized_expression" | "int_binary_op" => {
                out.push(Cell {
                    label: None,
                    value: cell_value(doc, node),
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

/// Numeric value of a cell, when the text determines it.
fn cell_value(doc: &Document, node: Node<'_>) -> Option<u32> {
    let text = util::node_text(doc, node);
    match node.kind() {
        "int_literal" | "integer" | "number" | "preproc_number_literal" => parse_integer(&text),
        "int_parenthesized_expression" => cell_value(doc, node.named_child(0)?),
        "int_unary_op" => {
            if node.child(0)?.kind() != "~" {
                return None;
            }
            cell_value(doc, node.named_child(0)?).map(|value| !value)
        }
        _ => None,
    }
}

fn parse_integer(text: &str) -> Option<u32> {
    let text = text.trim().trim_end_matches(['u', 'U', 'l', 'L']);
    let (digits, radix) = match text.get(..2) {
        Some(prefix) if prefix.eq_ignore_ascii_case("0x") => (&text[2..], 16),
        Some(prefix) if prefix.eq_ignore_ascii_case("0b") => (&text[2..], 2),
        _ => (text, 10),
    };
    u32::from_str_radix(digits, radix).ok()
}

/// Value of a single-cell property such as `#clock-cells = <1>`.
pub fn property_u32(doc: &Document, property: Node<'_>) -> Option<u32> {
    cells(doc, property)
        .into_iter()
        .find_map(|cell| cell.value)
}

fn reference_label<'a>(doc: &'a Document, reference: Node<'a>) -> Option<&'a str> {
    let label_ref = util::find_child(reference, "label_ref")?;
    let text = util::node_text(doc, label_ref);
    text.strip_prefix('&')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis;

    fn diags(text: &str) -> Vec<String> {
        let doc = Document::open(text, 0);
        let mut labels = HashMap::new();
        let mut cells: HashMap<String, HashMap<String, u32>> = HashMap::new();
        collect(&doc, doc.tree.root_node(), &mut labels, &mut cells);
        check_document(
            &doc,
            &UnitProviders {
                labels: &labels,
                cells: &cells,
            },
        )
        .into_iter()
        .map(|(_, _, message)| message)
        .collect()
    }

    fn collect(
        doc: &Document,
        node: Node<'_>,
        labels: &mut HashMap<String, String>,
        cells: &mut HashMap<String, HashMap<String, u32>>,
    ) {
        if node.kind() == "subnode" {
            let path = analysis::node_path(doc, node);
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32)
                    && child.kind() == "label"
                {
                    labels.insert(
                        util::node_text(doc, child).trim_end_matches(':').to_string(),
                        path.clone(),
                    );
                }
            }
            let body = util::node_def_of(node).unwrap_or(node);
            for i in 0..body.child_count() {
                if let Some(child) = body.child(i as u32)
                    && child.kind() == "property"
                    && let Some(name) = child.child_by_field_name("name")
                {
                    let name = util::node_text(doc, name);
                    if name.starts_with('#')
                        && name.ends_with("-cells")
                        && let Some(value) = property_u32(doc, child)
                    {
                        cells
                            .entry(path.clone())
                            .or_default()
                            .insert(name.to_string(), value);
                    }
                }
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                collect(doc, child, labels, cells);
            }
        }
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
            messages[0].contains("expected 1 argument cell(s)"),
            "{messages:?}"
        );
    }

    #[test]
    fn surplus_cells_are_reported_like_dtc() {
        // dtc warns "cell N is not a phandle reference" when a known, non-zero
        // cell sits where the next phandle should be
        let text = "/dts-v1/;\n/ {\n\tclk: clk@1 { #clock-cells = <0>; };\n\tbad { clocks = <&clk 5>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("cell 1 is not a phandle reference"),
            "{messages:?}"
        );
    }

    #[test]
    fn provider_without_cells_property_is_reported() {
        let text = "/dts-v1/;\n/ {\n\todd: odd@3 { };\n\tbad { clocks = <&odd>; };\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("missing property '#clock-cells'"),
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
        let text = "/dts-v1/;\n/ {\n\tintc: intc@1 { #interrupt-cells = <3>; };\n\tdev {\n\t\tinterrupts-extended = <&intc 0 5 4>, <&intc 0 6 4>;\n\t};\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));

        let text = "/dts-v1/;\n/ {\n\tintc: intc@1 { #interrupt-cells = <3>; };\n\tdev {\n\t\tinterrupts-extended = <&intc 0 5 4>, <&intc 0 6>;\n\t};\n};\n";
        let messages = diags(text);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].contains("expected 3 argument cell(s)"),
            "{messages:?}"
        );
    }

    #[test]
    fn skipped_entries_do_not_extend_the_previous_one() {
        // `<&phy 5>, <0>, <0>`: the `<0>` placeholders are skipped entries
        let text = "/dts-v1/;\n/ {\n\tphy: phy@1 { #clock-cells = <1>; };\n\tdev {\n\t\tclocks = <&phy 5>,\n\t\t         <0>, <0>;\n\t};\n};\n";
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
    fn unknown_cell_values_are_not_guessed() {
        // an unexpanded macro may be 0, which dtc skips silently — stay quiet
        let text = "/dts-v1/;\n/ {\n\tphy: phy@1 { #clock-cells = <0>; };\n\tdev {\n\t\tclocks = <&phy SOME_PIPE_CLK>;\n\t};\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
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
    fn interrupts_without_a_resolvable_controller_are_left_alone() {
        let text = "/dts-v1/;\n/ {\n\tdev { interrupts = <0 5>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }

    #[test]
    fn properties_outside_the_table_are_ignored() {
        // `foo-bars` is not in dtc's provider table
        let text = "/dts-v1/;\n/ {\n\tclk: clk@1 { #foo-bar-cells = <2>; };\n\tdev { foo-bars = <&clk>; };\n};\n";
        assert!(diags(text).is_empty(), "{:?}", diags(text));
    }
}
