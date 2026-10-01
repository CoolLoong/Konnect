//! Complete-document validation for property-only writes and recovery.
use anyhow::{ensure, Context, Result};
use konnect_sexp::{parse_sexp, writer::find_balanced_block, SexpNode};
use serde_json::Value;

pub(super) fn parse(source: &str) -> Result<SexpNode> {
    let start = source.len() - source.trim_start().len();
    let (_, end) = find_balanced_block(source, start).context("schematic root is unbalanced")?;
    ensure!(
        source[end..].trim().is_empty(),
        "trailing data after schematic root"
    );
    let strict = konnect_schematic_editor::sexp::parser::parse(source)?;
    ensure!(
        strict.tag() == Some("kicad_sch"),
        "expected one kicad_sch root"
    );
    let tree = parse_sexp(source)?;
    for symbol in tree.find_all("symbol") {
        ensure!(
            symbol.find_all("symbol").is_empty(),
            "nested placed symbol is invalid"
        );
        let mut names = std::collections::HashSet::new();
        for property in symbol.find_all("property") {
            let name = property
                .get(1)
                .and_then(SexpNode::as_str)
                .context("property name missing")?;
            ensure!(names.insert(name), "duplicate placed property {name}");
        }
    }
    Ok(tree)
}

fn without_properties(mut tree: SexpNode) -> SexpNode {
    if let SexpNode::List(children) = &mut tree {
        for child in children {
            if child.head() == Some("symbol") {
                if let SexpNode::List(parts) = child {
                    parts.retain(|part| part.head() != Some("property"));
                }
            }
        }
    }
    tree
}

pub(super) fn verify(before: &str, candidate: &str, changed: &[Value]) -> Result<()> {
    let old = parse(before)?;
    let new = parse(candidate)?;
    ensure!(
        without_properties(old.clone()) == without_properties(new.clone()),
        "property update changed geometry, identity, wiring or unrelated structure"
    );
    for symbol in old.find_all("symbol") {
        let id = symbol
            .find_str("uuid")
            .context("placed symbol has no UUID")?;
        let after = new
            .find_all("symbol")
            .into_iter()
            .find(|node| node.find_str("uuid") == Some(id))
            .context("placed symbol vanished")?;
        let reference = symbol
            .find_all("property")
            .into_iter()
            .find(|p| p.get(1).and_then(SexpNode::as_str) == Some("Reference"))
            .and_then(|p| p.get(2))
            .and_then(SexpNode::as_str)
            .context("placed symbol has no Reference")?;
        for property in symbol.find_all("property") {
            let name = property
                .get(1)
                .and_then(SexpNode::as_str)
                .context("property has no name")?;
            let mut wanted = property.clone();
            let replacement = changed
                .iter()
                .filter(|update| update["reference"] == reference)
                .flat_map(|update| update["fields"].as_array().into_iter().flatten())
                .find(|field| field["name"] == name);
            if let (Some(field), SexpNode::List(children)) = (replacement, &mut wanted) {
                children[2] = SexpNode::Str(
                    field["value"]
                        .as_str()
                        .context("field has no value")?
                        .into(),
                );
            }
            ensure!(
                after
                    .find_all("property")
                    .into_iter()
                    .any(|node| node == &wanted),
                "existing property {reference}/{name} geometry or unrelated value changed"
            );
        }
    }
    for update in changed {
        let reference = update["reference"]
            .as_str()
            .context("missing changed reference")?;
        let units = new
            .find_all("symbol")
            .into_iter()
            .filter(|symbol| {
                symbol.find_all("property").iter().any(|p| {
                    p.get(1).and_then(SexpNode::as_str) == Some("Reference")
                        && p.get(2).and_then(SexpNode::as_str) == Some(reference)
                })
            })
            .collect::<Vec<_>>();
        ensure!(!units.is_empty(), "changed reference {reference} vanished");
        for field in update["fields"]
            .as_array()
            .context("missing changed fields")?
        {
            let name = field["name"].as_str().context("missing field name")?;
            let expected = field["value"].as_str().context("missing field value")?;
            ensure!(
                units.len()
                    == field["updated_units"].as_u64().unwrap_or(0) as usize
                        + field["created_units"].as_u64().unwrap_or(0) as usize,
                "field unit coverage differs"
            );
            for symbol in &units {
                ensure!(
                    symbol
                        .find_all("property")
                        .iter()
                        .any(|p| p.get(1).and_then(SexpNode::as_str) == Some(name)
                            && p.get(2).and_then(SexpNode::as_str) == Some(expected)),
                    "{reference} {name} readback differs"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");
    #[test]
    fn refuses_incomplete_trailing_nested_and_unrelated_changes() {
        assert!(parse(&SOURCE[..SOURCE.len() - 3]).is_err());
        assert!(parse(&format!("{SOURCE} garbage")).is_err());
        let nested =
            SOURCE
                .replacen("(lib_id ", "(symbol (lib_id ", 1)
                .replacen("(unit 1)", "(unit 1))", 1);
        assert_ne!(nested, SOURCE);
        assert!(parse(&nested).is_err());
        let altered = SOURCE.replacen("(at 107.95 81.28 0)", "(at 108.95 81.28 0)", 1);
        if altered == SOURCE {
            let altered = SOURCE.replacen("(wire", "(bus", 1);
            assert_ne!(altered, SOURCE);
            assert!(verify(SOURCE, &altered, &[]).is_err());
        } else {
            assert!(verify(SOURCE, &altered, &[]).is_err());
        }
    }

    #[test]
    fn refuses_unrequested_property_and_wrong_created_field_values() {
        let symbol = konnect_sexp::writer::find_direct_child_blocks(SOURCE, "kicad_sch")
            .into_iter()
            .find(|&(start, _)| SOURCE[start..].starts_with("(symbol\n"))
            .unwrap();
        let start = SOURCE[symbol.0..symbol.1]
            .find("(property \"Value\" \"")
            .unwrap()
            + symbol.0
            + "(property \"Value\" \"".len();
        let end = start + SOURCE[start..].find('"').unwrap();
        let altered = konnect_sexp::writer::apply_edits(
            SOURCE.into(),
            vec![konnect_sexp::writer::SexpEdit::replace(
                start,
                end,
                "UNREQUESTED",
            )],
        );
        assert!(verify(SOURCE, &altered, &[]).is_err());
        let (candidate, counts) = super::super::sch_components::set_property_value(
            SOURCE,
            "U1",
            "New field",
            "actual value",
            true,
        )
        .unwrap();
        let changed = serde_json::json!({"reference":"U1","fields":[{"name":"New field","value":"WRONG VALUE","updated_units":counts.updated,"created_units":counts.added}]});
        assert!(verify(SOURCE, &candidate, &[changed]).is_err());
    }
}
