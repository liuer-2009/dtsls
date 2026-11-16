use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::document::Document;
use crate::util;

/// Property-lifetime analysis for a single document.
///
/// DTS semantics: within a merge scope, the last definition of a property
/// wins. A property dies when a later definition overrides it — either a
/// sibling in the same node, a property inside a same-file `&label { }` /
/// `&{/path} { }` extension of the node, or a following `/delete-property/`.
pub struct Analysis {
    /// Node ids of property syntax nodes whose value is dead.
    pub dead_properties: HashSet<usize>,
    /// Node labels with the full path of the labeled node, sorted by label.
    pub labels: Vec<(String, String)>,
}

/// (owner node id, property name) -> definitions in file order as (start byte, property node id).
type Contributions = HashMap<(usize, String), Vec<(usize, usize)>>;

struct Ctx<'a> {
    doc: &'a Document,
    labels: HashMap<String, Node<'a>>,
    root_devicetree: Option<Node<'a>>,
    contributions: Contributions,
    dead: HashSet<usize>,
}

pub fn analyze(doc: &Document) -> Analysis {
    let root = doc.tree.root_node();
    let mut ctx = Ctx {
        doc,
        labels: HashMap::new(),
        root_devicetree: find_root_devicetree(root),
        contributions: HashMap::new(),
        dead: HashSet::new(),
    };
    collect_labels(root, &mut ctx);
    walk(root, &mut ctx);

    let Ctx {
        doc,
        labels,
        dead,
        contributions,
        ..
    } = ctx;
    let mut dead = dead;
    for mut defs in contributions.into_values() {
        if defs.len() < 2 {
            continue;
        }
        defs.sort_by_key(|(start, _)| *start);
        for (_, id) in &defs[..defs.len() - 1] {
            dead.insert(*id);
        }
    }
    let mut label_list: Vec<(String, String)> = labels
        .into_iter()
        .map(|(label, node)| (label, node_path(doc, node)))
        .collect();
    label_list.sort();
    Analysis {
        dead_properties: dead,
        labels: label_list,
    }
}

/// Full path of a node (`/soc/serial@1000`), walking subnode ancestors up to
/// the root devicetree.
fn node_path(doc: &Document, mut node: Node) -> String {
    let mut parts: Vec<String> = Vec::new();
    loop {
        match node.kind() {
            "subnode" => {
                if let Some(name) = node.child_by_field_name("name") {
                    parts.push(util::node_text(doc, name).to_string());
                }
            }
            "devicetree" => {
                if let Some(reference) = util::find_child(node, "reference") {
                    parts.push(util::node_text(doc, reference).to_string());
                }
                break;
            }
            _ => {}
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    parts.reverse();
    if parts.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", parts.join("/"))
    }
}

fn find_root_devicetree(root: Node<'_>) -> Option<Node<'_>> {
    for i in 0..root.child_count() {
        if let Some(child) = root.child(i as u32)
            && child.kind() == "devicetree" && util::find_child(child, "reference").is_none() {
                return Some(child);
            }
    }
    None
}

fn collect_labels<'a>(node: Node<'a>, ctx: &mut Ctx<'a>) {
    if node.kind() == "subnode" {
        for i in 0..node.child_count() {
            if let Some(label) = node.child(i as u32)
                && label.kind() == "label" {
                    // Label token includes the trailing ':'.
                    let end = label.end_byte().saturating_sub(1);
                    let text = ctx.doc.slice(label.start_byte()..end).to_string();
                    ctx.labels.entry(text).or_insert(node);
                }
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_labels(child, ctx);
        }
    }
}

fn walk<'a>(node: Node<'a>, ctx: &mut Ctx<'a>) {
    match node.kind() {
        "devicetree" => {
            let owner = if util::find_child(node, "reference").is_some() {
                resolve_reference(node, ctx)
            } else {
                Some(node)
            };
            if let (Some(body), Some(target)) = (util::node_def_of(node), owner) {
                record_block(body, target.id(), ctx);
            }
        }
        "subnode" => {
            if let Some(body) = util::node_def_of(node) {
                record_block(body, node.id(), ctx);
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            walk(child, ctx);
        }
    }
}

fn record_block<'a>(container: Node<'a>, target_id: usize, ctx: &mut Ctx<'a>) {
    for i in 0..container.child_count() {
        let Some(child) = container.child(i as u32) else { continue };
        match child.kind() {
            "property" => {
                if let Some(delete) = util::find_child(child, "delete_property") {
                    kill_earlier(delete, target_id, child.start_byte(), ctx);
                } else if let Some(name) = child.child_by_field_name("name") {
                    let name = util::node_text(ctx.doc, name).to_string();
                    ctx.contributions
                        .entry((target_id, name))
                        .or_default()
                        .push((child.start_byte(), child.id()));
                }
            }
            // Properties and subnodes may live under #if blocks; the primary
            // branch is always recorded, alternatives are skipped.
            "preproc_if_in_node_def" | "preproc_ifdef_in_node_def" => {
                for j in 0..child.child_count() {
                    if let Some(inner) = child.child(j as u32) {
                        if inner.kind().contains("else") || inner.kind().contains("elif") {
                            continue;
                        }
                        record_block(inner, target_id, ctx);
                    }
                }
            }
            _ => {}
        }
    }
}

fn kill_earlier(delete: Node, target_id: usize, delete_start: usize, ctx: &mut Ctx) {
    let Some(name) = delete.child_by_field_name("name") else {
        return;
    };
    let name = util::node_text(ctx.doc, name).to_string();
    if let Some(defs) = ctx.contributions.get_mut(&(target_id, name)) {
        for (start, id) in defs.iter() {
            if *start < delete_start {
                ctx.dead.insert(*id);
            }
        }
    }
}

/// Resolve the node a `&label { }` / `&{/path} { }` extension targets.
/// Returns the referenced node, if it lives in this document.
fn resolve_reference<'a>(devicetree: Node<'a>, ctx: &Ctx<'a>) -> Option<Node<'a>> {
    let reference = util::find_child(devicetree, "reference")?;
    if let Some(label_ref) = util::find_child(reference, "label_ref") {
        let label = util::node_text(ctx.doc, label_ref).strip_prefix('&')?;
        return ctx.labels.get(label).copied();
    }
    if let Some(path_ref) = util::find_child(reference, "path_ref") {
        return resolve_path(util::node_text(ctx.doc, path_ref), ctx);
    }
    None
}

fn resolve_path<'a>(path_ref: &str, ctx: &Ctx<'a>) -> Option<Node<'a>> {
    let inner = path_ref.strip_prefix("&{")?.strip_suffix('}')?;
    let mut current = ctx.root_devicetree?;
    for segment in inner.split('/').filter(|s| !s.is_empty()) {
        let mut next = None;
        if let Some(body) = util::node_def_of(current) {
            for i in 0..body.child_count() {
                if let Some(candidate) = body.child(i as u32) {
                    if candidate.kind() != "subnode" {
                        continue;
                    }
                    let matches = candidate
                        .child_by_field_name("name")
                        .is_some_and(|n| util::node_text(ctx.doc, n) == segment);
                    if matches {
                        next = Some(candidate);
                        break;
                    }
                }
            }
        }
        current = next?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn property_ids(doc: &Document, name: &str) -> Vec<(usize, usize)> {
        // (start byte, node id) of every property node with the given name.
        let mut out = Vec::new();
        collect(doc, doc.tree.root_node(), name, &mut out);
        out.sort_by_key(|(start, _)| *start);
        out
    }

    fn collect(doc: &Document, node: Node, name: &str, out: &mut Vec<(usize, usize)>) {
        if node.kind() == "property"
            && let Some(prop_name) = node.child_by_field_name("name")
                && util::node_text(doc, prop_name) == name {
                    out.push((node.start_byte(), node.id()));
                }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                collect(doc, child, name, out);
            }
        }
    }

    #[test]
    fn later_same_node_definition_kills_earlier() {
        let text = "/dts-v1/;\n\n/ {\n    n {\n        a = <1>;\n        a = <2>;\n    };\n};\n";
        let doc = Document::open(text, 0);
        let analysis = analyze(&doc);
        let props = property_ids(&doc, "a");
        assert_eq!(props.len(), 2);
        assert!(analysis.dead_properties.contains(&props[0].1));
        assert!(!analysis.dead_properties.contains(&props[1].1));
    }

    #[test]
    fn delete_property_kills_earlier_definition() {
        let text = "/dts-v1/;\n\n/ {\n    n {\n        a = <1>;\n        /delete-property/ a;\n    };\n};\n";
        let doc = Document::open(text, 0);
        let analysis = analyze(&doc);
        let props = property_ids(&doc, "a");
        assert_eq!(props.len(), 1);
        assert!(analysis.dead_properties.contains(&props[0].1));
    }

    #[test]
    fn property_after_delete_survives() {
        let text = "/dts-v1/;\n\n/ {\n    n {\n        /delete-property/ a;\n        a = <1>;\n    };\n};\n";
        let doc = Document::open(text, 0);
        let analysis = analyze(&doc);
        let props = property_ids(&doc, "a");
        assert_eq!(props.len(), 1);
        assert!(!analysis.dead_properties.contains(&props[0].1));
    }

    #[test]
    fn overlay_extension_kills_base_property() {
        let text = "/dts-v1/;\n\n/ {\n    intc: n@1000 {\n        reg = <1>;\n    };\n};\n\n&intc {\n    reg = <2>;\n};\n";
        let doc = Document::open(text, 0);
        let analysis = analyze(&doc);
        let props = property_ids(&doc, "reg");
        assert_eq!(props.len(), 2);
        assert!(analysis.dead_properties.contains(&props[0].1));
        assert!(!analysis.dead_properties.contains(&props[1].1));
    }

    #[test]
    fn chained_extensions_last_wins() {
        let text = "/dts-v1/;\n\n/ {\n    intc: n@1000 {\n        reg = <1>;\n    };\n};\n\n&intc {\n    reg = <2>;\n};\n\n&intc {\n    reg = <3>;\n};\n";
        let doc = Document::open(text, 0);
        let analysis = analyze(&doc);
        let props = property_ids(&doc, "reg");
        assert_eq!(props.len(), 3);
        assert!(analysis.dead_properties.contains(&props[0].1));
        assert!(analysis.dead_properties.contains(&props[1].1));
        assert!(!analysis.dead_properties.contains(&props[2].1));
    }

    #[test]
    fn path_reference_resolves() {
        let text = "/dts-v1/;\n\n/ {\n    soc {\n        uart@1000 {\n            reg = <1>;\n        };\n    };\n};\n\n&{/soc/uart@1000} {\n    reg = <2>;\n};\n";
        let doc = Document::open(text, 0);
        let analysis = analyze(&doc);
        let props = property_ids(&doc, "reg");
        assert_eq!(props.len(), 2);
        assert!(analysis.dead_properties.contains(&props[0].1));
        assert!(!analysis.dead_properties.contains(&props[1].1));
    }
}
