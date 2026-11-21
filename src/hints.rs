// SPDX-License-Identifier: AGPL-3.0-only
use std::collections::HashMap;

use tower_lsp::lsp_types::{
    InlayHint, InlayHintKind, InlayHintLabel, InlayHintTooltip, MarkupContent, MarkupKind,
    Position, Range, Url,
};
use tree_sitter::Node;

use crate::document::Document;
use crate::model::{self, RefKind, Tree};
use crate::unit::TuData;

// Range pruning bounds the work per request; this is only a pathological-
// input guard.
const MAX_HINTS: usize = 10_000;
const DEFAULT_ADDRESS_CELLS: u32 = 2;
const DEFAULT_SIZE_CELLS: u32 = 2;

/// One leaf inside a value: a cell, a phandle reference, a macro...
struct Unit {
    start: usize,
    end: usize,
    /// where a hint annotating this unit goes: after the enclosing array's
    /// `>` when the unit is a cell, else the unit's own end
    hint_after: usize,
    /// `&label` when this unit is a phandle reference
    ref_label: Option<String>,
}

/// `textDocument/inlayHint`: for every `X` / `X-names` property pair, annotate
/// each entry of `X` with its name and each name with its entry — both
/// directions, matching dtc's merged view. Entry boundaries come from the
/// phandle target's `#*-cells` when resolvable, else from dividing the units
/// evenly among the names.
pub fn inlay_hints(
    doc: &Document,
    uri: &Url,
    unit: Option<&TuData>,
    range: &Range,
) -> Vec<InlayHint> {
    let ctx = HintCtx {
        doc,
        uri,
        unit,
        range,
        range_start: model::position_to_byte(doc, range.start),
        range_end: model::position_to_byte(doc, range.end),
    };
    let mut out = Vec::new();
    collect(doc.tree.root_node(), &ctx, &mut out);
    out
}

struct HintCtx<'a> {
    doc: &'a Document,
    uri: &'a Url,
    unit: Option<&'a TuData>,
    range: &'a Range,
    range_start: usize,
    range_end: usize,
}

fn collect(node: Node, ctx: &HintCtx, out: &mut Vec<InlayHint>) {
    if out.len() >= MAX_HINTS {
        return;
    }
    // prune subtrees that cannot contribute hints inside the range
    if node.end_byte() < ctx.range_start || node.start_byte() > ctx.range_end {
        return;
    }
    let (doc, uri, unit, range) = (ctx.doc, ctx.uri, ctx.unit, ctx.range);
    if node.kind() == "node_def" {
        let mut props: HashMap<String, Node> = HashMap::new();
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                if child.kind() != "property" {
                    continue;
                }
                if let Some(name) = child.child_by_field_name("name") {
                    props.insert(model::node_text(doc, name).to_string(), child);
                }
            }
        }
        for (name, prop) in &props {
            if name == "interrupts" {
                emit_interrupt_parent_hint(*prop, doc, unit, range, out);
                if out.len() >= MAX_HINTS {
                    return;
                }
            }
            let Some(base) = name.strip_suffix("-names") else {
                continue;
            };
            let companion = [base.to_string(), format!("{base}s"), format!("{base}-gpios")]
                .into_iter()
                .find_map(|candidate| props.get(&candidate).copied());
            if let Some(x_prop) = companion {
                emit_pair(x_prop, *prop, doc, uri, unit, range, out);
                if out.len() >= MAX_HINTS {
                    return;
                }
            }
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect(child, ctx, out);
        }
    }
}

/// A value entry: one `prop_data` group (a `<...>` array, a reference, ...).
struct Group<'a> {
    node: Node<'a>,
    units: Vec<Unit>,
    text: &'a str,
}

fn groups_of<'a>(prop: Node<'a>, doc: &'a Document) -> Vec<Group<'a>> {
    let mut groups = Vec::new();
    for i in 0..prop.child_count() {
        let Some(data) = prop.child(i as u32) else { continue };
        if data.kind() != "prop_data" {
            continue;
        }
        let units = units_of_group(data, doc);
        if units.is_empty() {
            continue;
        }
        let start = units[0].start;
        let end = units[units.len() - 1].end;
        groups.push(Group {
            node: data,
            units,
            text: doc.slice(start..end),
        });
    }
    groups
}

fn units_of_group(data: Node, doc: &Document) -> Vec<Unit> {
    let mut units = Vec::new();
    if let Some(array) = model::find_child(data, "array") {
        // cells are annotated after the array's closing `>`
        let array_end = array.end_byte();
        for i in 0..array.child_count() {
            let Some(cell) = array.child(i as u32) else { continue };
            push_unit(cell, doc, &mut units, array_end);
        }
        return units;
    }
    for i in 0..data.child_count() {
        let Some(child) = data.child(i as u32) else { continue };
        let before = units.len();
        push_unit(child, doc, &mut units, child.end_byte());
        if units.len() > before {
            return units;
        }
    }
    units
}

fn push_unit(node: Node, doc: &Document, units: &mut Vec<Unit>, hint_after: usize) {
    if !node.is_named() {
        return; // `<`, `>`, `,` tokens
    }
    let ref_label = match node.kind() {
        "label" => return, // cell labels are not cells
        "reference" => model::find_child(node, "label_ref").map(|lr| {
            model::node_text(doc, lr)
                .strip_prefix('&')
                .unwrap_or_default()
                .to_string()
        }),
        _ => None,
    };
    units.push(Unit {
        start: node.start_byte(),
        end: node.end_byte(),
        hint_after,
        ref_label,
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_pair(
    x_prop: Node,
    names_prop: Node,
    doc: &Document,
    uri: &Url,
    unit: Option<&TuData>,
    range: &Range,
    out: &mut Vec<InlayHint>,
) {
    let groups = groups_of(x_prop, doc);
    let mut names: Vec<(String, Position)> = Vec::new();
    for i in 0..names_prop.child_count() {
        let Some(data) = names_prop.child(i as u32) else { continue };
        if data.kind() != "prop_data" {
            continue;
        }
        if let Some(string) = model::find_child(data, "string_literal") {
            let name = model::node_text(doc, string).trim_matches('"').to_string();
            let pos = model::byte_to_position(doc, string.end_byte());
            names.push((name, pos));
        }
    }
    if groups.is_empty() || names.is_empty() {
        return;
    }

    let prop_name = x_prop
        .child_by_field_name("name")
        .map(|n| model::node_text(doc, n).to_string())
        .unwrap_or_default();

    let mut forward: Vec<(Position, String)> = Vec::new();
    let mut reverse: Vec<(Position, String)> = Vec::new();

    if names.len() == groups.len() {
        // One name per group: hint after the group's closing `>`.
        for (i, group) in groups.iter().enumerate() {
            let end = group
                .units
                .last()
                .map(|u| u.hint_after)
                .unwrap_or_else(|| group.node.end_byte());
            forward.push((model::byte_to_position(doc, end), names[i].0.clone()));
            reverse.push((names[i].1, format!("<{}>", group.text)));
        }
    } else {
        // Packed cells (`<GIC_SPI 32 4 GIC_SPI 33 4>`) or uneven groups:
        // slice the flat unit list by stride.
        let units: Vec<&Unit> = groups.iter().flat_map(|g| g.units.iter()).collect();
        let cells_name = cells_prop_name(&prop_name);
        let interrupt_parent = prop_name == "interrupts";
        let mut pos = 0usize;
        for (i, (name, _)) in names.iter().enumerate() {
            if pos >= units.len() {
                break;
            }
            let stride = units[pos]
                .ref_label
                .as_ref()
                .and_then(|label| cells_of_target(unit, uri, doc, label, &cells_name))
                .map(|c| 1 + c)
                .or_else(|| {
                    if interrupt_parent {
                        interrupt_cells(doc, unit, x_prop)
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    if prop_name == "reg" {
                        reg_cells(doc, x_prop, unit)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| {
                    let remaining_units = units.len() - pos;
                    let remaining_names = names.len() - i;
                    if remaining_units.is_multiple_of(remaining_names) {
                        (remaining_units / remaining_names) as u32
                    } else {
                        1
                    }
                })
                .max(1) as usize;
            if pos + stride > units.len() {
                break;
            }
            let last = &units[pos + stride - 1];
            forward.push((model::byte_to_position(doc, last.hint_after), name.clone()));
            let slice = doc.slice(units[pos].start..last.end);
            reverse.push((names[i].1, format!("<{slice}>")));
            pos += stride;
        }
    }

    for (position, label) in forward {
        if position_in_range(&position, range) {
            out.push(hint(position, label));
        }
    }
    for (position, label) in reverse {
        if position_in_range(&position, range) {
            out.push(hint(position, label));
        }
    }
}

fn hint(position: Position, label: String) -> InlayHint {
    InlayHint {
        position,
        label: InlayHintLabel::String(label),
        kind: Some(InlayHintKind::PARAMETER),
        text_edits: None,
        tooltip: None,
        padding_left: Some(true),
        padding_right: None,
        data: None,
    }
}

fn position_in_range(position: &Position, range: &Range) -> bool {
    (position.line, position.character)
        >= (range.start.line, range.start.character)
        && (position.line, position.character)
            <= (range.end.line, range.end.character)
}

/// `clocks` -> `#clock-cells`, `dmas` -> `#dma-cells` (singular fallback).
fn cells_prop_name(prop_name: &str) -> String {
    let direct = format!("#{prop_name}-cells");
    // `power-domains` -> `#power-domain-cells`, not `#power-domains-cells`
    if let Some(stripped) = direct.strip_suffix("ss-cells") {
        return stripped.to_string() + "-cells";
    }
    direct
}

fn cells_of_target(
    unit: Option<&TuData>,
    uri: &Url,
    doc: &Document,
    label: &str,
    cells_name: &str,
) -> Option<u32> {
    if let Some(unit) = unit
        && let Some(def) = unit.labels.get(label)
    {
        if uri.to_file_path().ok().as_deref() == Some(def.file.as_path())
            && let Some(value) = cells_at(doc, &def.path, cells_name)
        {
            return Some(value);
        }
        return unit
            .cells
            .iter()
            .find(|cell| cell.path == def.path && cell.name == cells_name)
            .map(|cell| cell.value);
    }
    // no unit: the label must be defined in this buffer
    let mut tree = Tree::new(doc);
    let path = tree.display_path(tree.label(label)?);
    cells_at(doc, &path, cells_name)
}

/// `#*-cells` value of the node at a canonical path in this document.
fn cells_at(doc: &Document, path: &str, cells_name: &str) -> Option<u32> {
    let node = Tree::new(doc).node_at(path)?;
    let prop = model::find_property(doc, node, cells_name)?;
    model::property_u32(doc, prop)
}

/// `reg` entries are `#address-cells + #size-cells` wide, as advertised by
/// the parent bus node (spec defaults: 2 and 2).
fn reg_cells(doc: &Document, x_prop: Node, unit: Option<&TuData>) -> Option<u32> {
    let owner = model::owning_node(x_prop)?;
    // the bus is the parent node: a subnode or the root devicetree
    let bus = owner
        .parent()
        .and_then(|p| p.parent())
        .filter(|p| p.kind() == "subnode" || p.kind() == "devicetree")?;
    let path = Tree::new(doc).display_path(bus);
    let cell = |name: &str| {
        unit.and_then(|unit| {
            unit.cells
                .iter()
                .find(|cell| cell.path == path && cell.name == name)
                .map(|cell| cell.value)
        })
        .or_else(|| cells_at(doc, &path, name))
    };
    let address = cell("#address-cells").unwrap_or(DEFAULT_ADDRESS_CELLS);
    let size = cell("#size-cells").unwrap_or(DEFAULT_SIZE_CELLS);
    Some(address + size)
}

/// The `interrupt-parent` reference of this node or the nearest ancestor, in
/// display form: `&intc` for a label ref, `&{/path}` for a path ref. The
/// nearest `interrupt-parent` wins, like dtc's lookup.
fn interrupt_parent_ref(doc: &Document, x_prop: Node) -> Option<String> {
    let mut owner = model::owning_node(x_prop)?;
    loop {
        if let Some(prop) = model::find_property(doc, owner, "interrupt-parent") {
            let reference = model::find_descendant(prop, "reference");
            if let Some(reference) = reference
                && let Some(kind) = model::reference_of(doc, reference)
            {
                return Some(match kind {
                    RefKind::Label(label) => format!("&{label}"),
                    RefKind::Path(path) => path.to_string(),
                });
            }
        }
        // inherit: walk up to the parent node (a subnode or the root devicetree)
        let parent = owner
            .parent()
            .and_then(|p| p.parent())
            .filter(|p| p.kind() == "subnode" || p.kind() == "devicetree");
        match parent {
            Some(p) => owner = p,
            None => return None,
        }
    }
}

/// `interrupts` takes its stride from the interrupt-parent's
/// `#interrupt-cells`, which may be inherited from an ancestor.
fn interrupt_cells(doc: &Document, unit: Option<&TuData>, x_prop: Node) -> Option<u32> {
    let display = interrupt_parent_ref(doc, x_prop)?;
    let label = display.strip_prefix('&')?;
    if label.starts_with('{') {
        // `&{/path}` parent: the cells lookup below is label-keyed
        return None;
    }
    if let Some(unit) = unit
        && let Some(def) = unit.labels.get(label)
        && let Some(cell) = unit
            .cells
            .iter()
            .find(|cell| cell.path == def.path && cell.name == "#interrupt-cells")
    {
        return Some(cell.value);
    }
    let mut tree = Tree::new(doc);
    let path = tree.display_path(tree.label(label)?);
    cells_at(doc, &path, "#interrupt-cells")
}

/// `interrupts` specifiers belong to the controller named by
/// `interrupt-parent` (on this node or an ancestor). Render that controller
/// as an inlay hint at the value start so the reader sees who the cells are
/// addressed to. A trailing phandle cell in the value (GICv3's 4-cell form)
/// selects a PPI partition, not the controller — the hint still shows the
/// interrupt-parent, which is exactly the disambiguation a reader needs there.
fn emit_interrupt_parent_hint(
    x_prop: Node,
    doc: &Document,
    unit: Option<&TuData>,
    range: &Range,
    out: &mut Vec<InlayHint>,
) {
    if out.len() >= MAX_HINTS {
        return;
    }
    let Some(display) = interrupt_parent_ref(doc, x_prop) else {
        return;
    };
    // position: the start of the value (the first prop_data, i.e. the `<`)
    let Some(value_byte) = (0..x_prop.child_count()).find_map(|i| {
        let child = x_prop.child(i as u32)?;
        (child.kind() == "prop_data").then_some(child.start_byte())
    }) else {
        return;
    };
    let position = model::byte_to_position(doc, value_byte);
    if !position_in_range(&position, range) {
        return;
    }

    let mut tooltip_lines = vec![format!("`{display}` from `interrupt-parent`")];
    let bare = display.strip_prefix('&').unwrap_or(&display);
    let controller = match (unit, bare.starts_with('{')) {
        (Some(unit), false) => unit.labels.get(bare).and_then(|def| {
            let cells = unit
                .cells
                .iter()
                .find(|cell| cell.path == def.path && cell.name == "#interrupt-cells")
                .map(|cell| cell.value);
            Some((def.path.clone(), cells))
        }),
        (_, false) => {
            let mut tree = Tree::new(doc);
            tree.label(bare).map(|node| {
                let path = tree.display_path(node);
                let cells = model::find_property(doc, node, "#interrupt-cells")
                    .and_then(|prop| model::property_u32(doc, prop));
                (path, cells)
            })
        }
        _ => None,
    };
    if let Some((path, cells)) = controller {
        tooltip_lines.push(format!("controller at `{path}`"));
        if let Some(value) = cells {
            tooltip_lines.push(format!("`#interrupt-cells` = <{value}>"));
        }
    }

    out.push(InlayHint {
        position,
        label: InlayHintLabel::String(display),
        kind: Some(InlayHintKind::PARAMETER),
        tooltip: Some(InlayHintTooltip::MarkupContent(MarkupContent {
            kind: MarkupKind::Markdown,
            value: tooltip_lines.join("  \n"),
        })),
        padding_left: Some(true),
        padding_right: Some(true),
        text_edits: None,
        data: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn hints_for(text: &str) -> Vec<(u32, String)> {
        let doc = Document::open(text, 0);
        let full = Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: u32::MAX, character: 0 },
        };
        inlay_hints(&doc, &"file:///t.dts".parse().unwrap(), None, &full)
            .into_iter()
            .map(|h| (h.position.line + 1, match h.label {
                InlayHintLabel::String(s) => s,
                _ => String::new(),
            }))
            .collect()
    }

    #[test]
    fn clocks_bidirectional_group_mode() {
        let text = "/dts-v1/;\n\n/ {\n    dev {\n        clocks = <&clk 5>, <&clk 7>;\n        clock-names = \"core\", \"bus\";\n    };\n};\n";
        let hints = hints_for(text);
        // clocks line: two name hints; names line: two cell hints
        let clocks: Vec<_> = hints.iter().filter(|(l, _)| *l == 5).collect();
        let names: Vec<_> = hints.iter().filter(|(l, _)| *l == 6).collect();
        assert_eq!(clocks.len(), 2);
        assert_eq!(clocks[0].1, "core");
        assert_eq!(clocks[1].1, "bus");
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].1, "<&clk 5>");
        assert_eq!(names[1].1, "<&clk 7>");
    }

    #[test]
    fn packed_interrupts_use_interrupt_parent_cells() {
        let text = "/dts-v1/;\n\n/ {\n    intc: interrupt-controller@1000 {\n        #interrupt-cells = <3>;\n    };\n\n    uart {\n        interrupt-parent = <&intc>;\n        interrupts = <GIC_SPI 32 4 GIC_SPI 33 4>;\n        interrupt-names = \"rx\", \"tx\";\n    };\n};\n";
        let hints = hints_for(text);
        let ints: Vec<_> = hints
            .iter()
            .filter(|(l, s)| *l == 10 && (*s == "rx" || *s == "tx"))
            .map(|(_, s)| s)
            .collect();
        assert_eq!(ints.len(), 2, "hints: {:?}", hints);
        assert_eq!(ints[0], "rx");
        assert_eq!(ints[1], "tx");
        let names: Vec<_> = hints.iter().filter(|(l, _)| *l == 11).map(|(_, s)| s).collect();
        assert_eq!(names[0], "<GIC_SPI 32 4>");
        assert_eq!(names[1], "<GIC_SPI 33 4>");
    }

    #[test]
    fn interrupts_hint_shows_the_interrupt_parent() {
        let text = "/dts-v1/;\n\n/ {\n    intc: interrupt-controller@1000 {\n        #interrupt-cells = <3>;\n    };\n\n    uart {\n        interrupt-parent = <&intc>;\n        interrupts = <GIC_SPI 0 4>;\n    };\n};\n";
        let hints = hints_for(text);
        let parent: Vec<_> = hints
            .iter()
            .filter(|(l, s)| *l == 10 && s.starts_with('&'))
            .map(|(_, s)| s)
            .collect();
        assert_eq!(parent.len(), 1, "hints: {:?}", hints);
        assert_eq!(parent[0], "&intc");
    }

    #[test]
    fn interrupts_hint_inherits_from_an_ancestor() {
        // interrupt-parent sits on the root, interrupts two levels deep
        let text = "/dts-v1/;\n\n/ {\n    interrupt-parent = <&intc>;\n\n    intc: interrupt-controller@1000 {\n        #interrupt-cells = <2>;\n    };\n\n    bus {\n        dev {\n            interrupts = <5 6>;\n        };\n    };\n};\n";
        let hints = hints_for(text);
        let parent: Vec<_> = hints
            .iter()
            .filter(|(l, s)| *l == 12 && s.starts_with('&'))
            .map(|(_, s)| s)
            .collect();
        assert_eq!(parent.len(), 1, "{hints:?}");
        assert_eq!(parent[0], "&intc");
    }

    #[test]
    fn partition_phandle_does_not_suppress_the_parent_hint() {
        // GICv3 4-cell form: the trailing &ppi_cluster0 selects a PPI
        // partition, not the controller — the hint shows the inherited
        // interrupt-parent (&intc), which is the actual disambiguation
        let text = "/dts-v1/;\n\n/ {\n    interrupt-parent = <&intc>;\n\n    intc: interrupt-controller@1000 {\n        #interrupt-cells = <4>;\n\n        ppi_cluster0: partition-0 {\n            affinity = <&cpu0>;\n        };\n    };\n\n    pmu-a520 {\n        interrupts = <GIC_PPI 7 4 &ppi_cluster0>;\n    };\n};\n";
        let hints = hints_for(text);
        // interrupts is on 0-based line 14
        let parent: Vec<_> = hints
            .iter()
            .filter(|(l, s)| *l == 15 && s.starts_with('&'))
            .map(|(_, s)| s.clone())
            .collect();
        assert_eq!(parent, ["&intc"], "{hints:?}");
    }

    #[test]
    fn reg_uses_parent_cells_when_packed() {
        // two reg entries packed into one array: address+size = 2 cells each
        let text = "/dts-v1/;\n\n/ {\n    #address-cells = <1>;\n    #size-cells = <1>;\n\n    mem@100 {\n        reg = <0x100 0x10 0x200 0x20>;\n        reg-names = \"a\", \"b\";\n    };\n};\n";
        let hints = hints_for(text);
        let reg: Vec<_> = hints.iter().filter(|(l, _)| *l == 8).map(|(_, s)| s).collect();
        assert_eq!(reg.len(), 2, "hints: {:?}", hints);
        assert_eq!(reg[0], "a");
        assert_eq!(reg[1], "b");
        let names: Vec<_> = hints.iter().filter(|(l, _)| *l == 9).map(|(_, s)| s).collect();
        assert_eq!(names[0], "<0x100 0x10>");
        assert_eq!(names[1], "<0x200 0x20>");
    }
}
