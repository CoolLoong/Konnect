//! Explicit closed/saved-board stackup edits, validated through KiCad itself.

use crate::{
    mcp::protocol::{CallToolResult, ToolContent},
    outcome::{self, OutcomeStatus},
    tool,
    tools::{
        get_path,
        live_board::{self, EditorLock},
        pcb_board::{attempt_ipc_write, BoardWrite},
        ToolContext, ToolDef,
    },
};
use anyhow::{ensure, Context, Result};
use konnect_sexp::{
    parse_sexp,
    writer::{
        find_balanced_block, find_direct_child_blocks, read_consistent, write_atomic_if_unchanged,
    },
    SexpNode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, io::Write, path::Path};

pub(crate) fn tool() -> ToolDef {
    tool!("set_stackup",
        "Preview or set the physical stackup of an explicitly SAVED and CLOSED PCB. KiCad 10's native stackup setter is unavailable: this tool NEVER edits a live board. Requires confirm_closed_saved=true, no editor lock, and safe editor-absence evidence. Give the complete physical layer order; copper count/layers cannot change. Preserves every unrelated source byte and unrequested stackup property. dry_run defaults true; apply requires its exact source-and-request plan_revision. Validates a candidate by KiCad CLI reopen and independent IPC-2581 layer readback before writing, then validates the written file again. Keeps an immutable preimage; no GUI undo. Post-write failure is uncertain and requires inspection before retry. Reopen the saved board in KiCad to continue editing.",
        json!({"type":"object","additionalProperties":false,"properties":{
            "board":{"type":"string"},"confirm_closed_saved":{"type":"boolean","const":true},
            "dry_run":{"type":"boolean","default":true},"expected_plan_revision":{"type":"string","pattern":"^[0-9a-f]{64}$"},
            "board_thickness_mm":{"type":"number","exclusiveMinimum":0,"description":"Optional declared nominal board thickness, separate from the sum of physical copper/dielectric layers"},
            "layers":{"type":"array","minItems":3,"items":{"type":"object","additionalProperties":false,"properties":{
                "name":{"type":"string","description":"Exact canonical name: F.SilkS, F.Paste, F.Mask, F.Cu, dielectric 1, In1.Cu, ..., B.Cu, B.Mask, B.Paste, B.SilkS"},
                "kind":{"type":"string","enum":["copper","core","prepreg","soldermask","silkscreen","solderpaste"]},
                "thickness_mm":{"type":"number","minimum":0},"material":{"type":"string","minLength":1},
                "epsilon_r":{"type":"number","exclusiveMinimum":0},"loss_tangent":{"type":"number","minimum":0}
            },"required":["name","kind","thickness_mm"]}}
        },"required":["board","confirm_closed_saved","layers"]}),
        |args, ctx| async move { handle_set_stackup(args, ctx).await }
    ).with_board_access(crate::tools::BoardAccess::ClosedBoardOnly)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Layer {
    name: String,
    kind: String,
    thickness_mm: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    material: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epsilon_r: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loss_tangent: Option<f64>,
}
struct Request {
    layers: Vec<Layer>,
    board_thickness_mm: Option<f64>,
    dry_run: bool,
    expected: Option<String>,
}
fn request(args: &Value) -> Result<Request> {
    let object = args.as_object().context("request must be object")?;
    ensure!(
        object.keys().all(|k| [
            "board",
            "confirm_closed_saved",
            "dry_run",
            "expected_plan_revision",
            "board_thickness_mm",
            "layers"
        ]
        .contains(&k.as_str())),
        "unknown request property"
    );
    ensure!(
        args["confirm_closed_saved"].as_bool() == Some(true),
        "confirm_closed_saved=true requires the caller to save and close this exact board first"
    );
    let layers: Vec<Layer> = serde_json::from_value(args["layers"].clone())?;
    let dry_run = match args.get("dry_run") {
        None => true,
        Some(v) => v.as_bool().context("dry_run must be boolean")?,
    };
    let expected = match args.get("expected_plan_revision") {
        None => None,
        Some(v) => Some(v.as_str().context("revision must be a string")?.to_string()),
    };
    if let Some(rev) = &expected {
        ensure!(
            rev.len() == 64
                && rev
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid revision"
        );
    }
    ensure!(
        dry_run || expected.is_some(),
        "apply requires expected_plan_revision from dry_run"
    );
    let board_thickness_mm = match args.get("board_thickness_mm") {
        None => None,
        Some(v) => Some(v.as_f64().context("board thickness must be number")?),
    };
    if let Some(value) = board_thickness_mm {
        ensure!(
            konnect_ipc::builders::try_mm_to_nm(value)? > 0,
            "nominal board thickness must be positive"
        );
    }
    let mut names = BTreeSet::new();
    for layer in &layers {
        ensure!(names.insert(&layer.name), "duplicate layer name");
        let nm = konnect_ipc::builders::try_mm_to_nm(layer.thickness_mm)?;
        ensure!(
            nm >= 0 && (nm > 0 || matches!(layer.kind.as_str(), "silkscreen" | "solderpaste")),
            "physical layer thickness must be positive except silk/paste"
        );
        ensure!(
            layer
                .material
                .as_ref()
                .is_none_or(|v| !v.is_empty() && !v.contains(['\n', '\r', '\0'])),
            "invalid material"
        );
        ensure!(
            layer.epsilon_r.is_none_or(|v| v.is_finite() && v > 0.0),
            "epsilon_r must be finite positive"
        );
        ensure!(
            layer.loss_tangent.is_none_or(|v| v.is_finite() && v >= 0.0),
            "loss_tangent must be finite nonnegative"
        );
        layer_type(layer)?;
    }
    Ok(Request {
        layers,
        board_thickness_mm,
        dry_run,
        expected,
    })
}
fn layer_type(layer: &Layer) -> Result<String> {
    Ok(match layer.kind.as_str() {
        "copper"
            if layer.name.ends_with(".Cu")
                && konnect_ipc::builders::try_layer_from_name(&layer.name).is_ok() =>
        {
            "copper".into()
        }
        "core" | "prepreg" if layer.name.starts_with("dielectric ") => layer.kind.clone(),
        "soldermask" if ["F.Mask", "B.Mask"].contains(&layer.name.as_str()) => format!(
            "{} Solder Mask",
            if layer.name.starts_with('F') {
                "Top"
            } else {
                "Bottom"
            }
        ),
        "silkscreen" if ["F.SilkS", "B.SilkS"].contains(&layer.name.as_str()) => format!(
            "{} Silk Screen",
            if layer.name.starts_with('F') {
                "Top"
            } else {
                "Bottom"
            }
        ),
        "solderpaste" if ["F.Paste", "B.Paste"].contains(&layer.name.as_str()) => format!(
            "{} Solder Paste",
            if layer.name.starts_with('F') {
                "Top"
            } else {
                "Bottom"
            }
        ),
        _ => anyhow::bail!("layer name/kind mismatch: {} / {}", layer.name, layer.kind),
    })
}
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
fn strict(source: &str) -> Result<SexpNode> {
    let start = source.len() - source.trim_start().len();
    let (_, end) = find_balanced_block(source, start).context("unbalanced board root")?;
    ensure!(
        source[end..].trim().is_empty(),
        "trailing data after board root"
    );
    ensure!(
        konnect_schematic_editor::sexp::parser::parse(source)?.tag() == Some("kicad_pcb"),
        "expected one kicad_pcb root"
    );
    Ok(parse_sexp(source)?)
}
fn child_range(source: &str, parent: &str, tag: &str) -> Result<Option<(usize, usize)>> {
    let ranges = find_direct_child_blocks(source, parent)
        .into_iter()
        .filter(|(a, b)| parse_sexp(&source[*a..*b]).is_ok_and(|n| n.head() == Some(tag)))
        .collect::<Vec<_>>();
    ensure!(ranges.len() <= 1, "duplicate {tag} in {parent}");
    Ok(ranges.first().copied())
}
fn patch(block: &str, parent: &str, tag: &str, value: &str) -> Result<String> {
    let new = format!("({tag} {value})");
    let (a, b) = child_range(block, parent, tag)?
        .unwrap_or((block.trim_end().len() - 1, block.trim_end().len() - 1));
    Ok(format!(
        "{}{}{}",
        &block[..a],
        if a == b {
            format!("\n\t\t{new}\n")
        } else {
            new
        },
        &block[b..]
    ))
}
fn canonical_order(tree: &SexpNode) -> Result<Vec<String>> {
    let layers = tree.find_all("layers");
    ensure!(layers.len() == 1, "one board layer table required");
    let mut copper = Vec::new();
    let mut enabled = BTreeSet::new();
    for layer in layers[0]
        .children()
        .context("invalid layers")?
        .iter()
        .skip(1)
    {
        let name = layer
            .get(1)
            .and_then(SexpNode::as_str)
            .context("invalid board layer name")?
            .to_string();
        if name.ends_with(".Cu") {
            copper.push(name.clone());
        }
        enabled.insert(name);
    }
    ensure!(
        copper.first().is_some_and(|s| s == "F.Cu")
            && copper.last().is_some_and(|s| s == "B.Cu")
            && copper.len() >= 2,
        "unsupported copper layer order"
    );
    let mut order = Vec::new();
    for name in ["F.SilkS", "F.Paste", "F.Mask"] {
        if enabled.contains(name) {
            order.push(name.into());
        }
    }
    for (i, name) in copper.into_iter().enumerate() {
        if i > 0 {
            order.push(format!("dielectric {i}"));
        }
        order.push(name);
    }
    for name in ["B.Mask", "B.Paste", "B.SilkS"] {
        if enabled.contains(name) {
            order.push(name.into());
        }
    }
    Ok(order)
}
struct Plan {
    candidate: String,
    revision: String,
    before: Value,
    after: Value,
}
fn saved_summary(tree: &SexpNode) -> Value {
    let stack = tree.find("setup").and_then(|n| n.find("stackup"));
    json!({"explicit_stackup":stack.is_some(),"declared_board_thickness_mm":tree.find("general").and_then(|n|n.find_f64("thickness")),"layers":stack.map(|s|s.find_all("layer").iter().map(|l|json!({"name":l.get(1).and_then(SexpNode::as_str),"type":l.find_str("type"),"thickness_mm":l.find_f64("thickness"),"material":l.find_str("material"),"epsilon_r":l.find_f64("epsilon_r"),"loss_tangent":l.find_f64("loss_tangent")})).collect::<Vec<_>>())})
}
fn plan(source: &str, request: &Request) -> Result<Plan> {
    let tree = strict(source)?;
    let order = canonical_order(&tree)?;
    ensure!(
        request
            .layers
            .iter()
            .map(|l| l.name.clone())
            .collect::<Vec<_>>()
            == order,
        "layers must give the exact complete physical order; copper count/layers cannot change"
    );
    let (a, b) = child_range(source, "kicad_pcb", "setup")?.context("board has no setup")?;
    let setup = &source[a..b];
    let mut stack = match child_range(setup, "setup", "stackup")? {
        Some((a, b)) => setup[a..b].to_string(),
        None => "(stackup)".into(),
    };
    let old = parse_sexp(&stack)?;
    let existing = old.find_all("layer");
    if !existing.is_empty() {
        ensure!(
            existing
                .iter()
                .map(|l| l.get(1).and_then(SexpNode::as_str).unwrap_or(""))
                .collect::<Vec<_>>()
                == order,
            "existing stackup order is unsupported; do not flatten or discard layers"
        );
    }
    for layer in &request.layers {
        let ranges = find_direct_child_blocks(&stack, "stackup");
        let matching = ranges
            .into_iter()
            .filter(|(a, b)| {
                parse_sexp(&stack[*a..*b]).is_ok_and(|n| {
                    n.head() == Some("layer")
                        && n.get(1).and_then(SexpNode::as_str) == Some(layer.name.as_str())
                })
            })
            .collect::<Vec<_>>();
        ensure!(matching.len() <= 1, "duplicate stackup layer");
        let range = matching.first().copied();
        let mut block = range
            .map(|(a, b)| stack[a..b].to_string())
            .unwrap_or_else(|| format!("(layer {})", quote(&layer.name)));
        block = patch(&block, "layer", "type", &quote(&layer_type(layer)?))?;
        let canonical = konnect_ipc::builders::nm_to_mm(konnect_ipc::builders::try_mm_to_nm(
            layer.thickness_mm,
        )?);
        block = patch(&block, "layer", "thickness", &canonical.to_string())?;
        if let Some(material) = &layer.material {
            block = patch(&block, "layer", "material", &quote(material))?;
        }
        if let Some(value) = layer.epsilon_r {
            block = patch(&block, "layer", "epsilon_r", &value.to_string())?;
        }
        if let Some(value) = layer.loss_tangent {
            block = patch(&block, "layer", "loss_tangent", &value.to_string())?;
        }
        let (a, b) = range.unwrap_or((stack.len() - 1, stack.len() - 1));
        stack = format!("{}\n{}\n{}", &stack[..a], block, &stack[b..]);
    }
    let next_setup = patch(setup, "setup", "stackup", "")?;
    // Replace the complete stackup block, preserving every byte outside it.
    let mut setup_candidate = next_setup;
    let (x, y) =
        child_range(&setup_candidate, "setup", "stackup")?.context("new stackup missing")?;
    setup_candidate.replace_range(x..y, &stack);
    let mut candidate = source.to_string();
    candidate.replace_range(a..b, &setup_candidate);
    if let Some(value) = request.board_thickness_mm {
        let (a, b) = child_range(&candidate, "kicad_pcb", "general")?
            .context("board has no general section")?;
        let replacement = patch(
            &candidate[a..b],
            "general",
            "thickness",
            &konnect_ipc::builders::nm_to_mm(konnect_ipc::builders::try_mm_to_nm(value)?)
                .to_string(),
        )?;
        candidate.replace_range(a..b, &replacement);
    }
    let next = strict(&candidate)?;
    // Compare the complete structure after removing only the explicitly
    // permitted stackup/nominal-thickness nodes. No board objects can disappear.
    ensure!(
        without_stackup(tree.clone(), request.board_thickness_mm.is_some())
            == without_stackup(next.clone(), request.board_thickness_mm.is_some()),
        "unrelated board structure changed"
    );
    let revision = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            source,
            &request.layers,
            request.board_thickness_mm
        ))?)
    );
    Ok(Plan {
        candidate,
        revision,
        before: saved_summary(&tree),
        after: saved_summary(&next),
    })
}
fn without_stackup(mut tree: SexpNode, thickness: bool) -> SexpNode {
    if let SexpNode::List(children) = &mut tree {
        for node in children {
            let tag = node.head().map(String::from);
            if let SexpNode::List(parts) = node {
                if tag.as_deref() == Some("setup") {
                    parts.retain(|p| p.head() != Some("stackup"));
                }
                if thickness && tag.as_deref() == Some("general") {
                    parts.retain(|p| p.head() != Some("thickness"));
                }
            }
        }
    }
    tree
}

async fn closed(ctx: &ToolContext, board: &Path) -> Result<Value> {
    ensure!(
        matches!(live_board::editor_lock(board), EditorLock::Absent),
        "exact board has an editor lock or its lock cannot be inspected; save and close it first"
    );
    match attempt_ipc_write(ctx, board, "closed-board stackup edit", |_| Ok(())).await? {
        BoardWrite::File(evidence) => Ok(evidence.evidence()),
        BoardWrite::Ipc(()) => {
            anyhow::bail!("requested board is open in KiCad; no live stackup setter is implemented")
        }
        BoardWrite::Refused(result) => anyhow::bail!(
            "editor absence could not be established: {}",
            result
                .content
                .iter()
                .find_map(|c| match c {
                    ToolContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("IPC refused")
        ),
    }
}
fn xml_name(name: &str) -> String {
    match name {
        "F.SilkS" => "F.Silkscreen".into(),
        "B.SilkS" => "B.Silkscreen".into(),
        n if n.starts_with("dielectric ") => format!("DIELECTRIC_{}", &n[11..]),
        _ => name.into(),
    }
}
fn numeric(actual: &str, wanted: f64, tolerance: f64) -> Result<()> {
    let value: f64 = actual.parse()?;
    ensure!(
        value.is_finite() && (value - wanted).abs() <= tolerance,
        "CLI value {actual} differs from requested {wanted}"
    );
    Ok(())
}
fn verify_xml(xml: &str, request: &Request) -> Result<Value> {
    let document = roxmltree::Document::parse(xml)?;
    let stackups = document
        .descendants()
        .filter(|n| n.has_tag_name("Stackup"))
        .collect::<Vec<_>>();
    ensure!(stackups.len() == 1, "CLI readback must contain one Stackup");
    let layers = stackups[0]
        .descendants()
        .filter(|n| n.has_tag_name("StackupLayer"))
        .collect::<Vec<_>>();
    ensure!(
        layers.len() == request.layers.len(),
        "CLI layer coverage differs from requested complete stack"
    );
    for (actual, wanted) in layers.iter().zip(&request.layers) {
        ensure!(
            actual.attribute("layerOrGroupRef") == Some(xml_name(&wanted.name).as_str()),
            "CLI layer order/name differs from request"
        );
        let expected = konnect_ipc::builders::nm_to_mm(konnect_ipc::builders::try_mm_to_nm(
            wanted.thickness_mm,
        )?);
        numeric(
            actual
                .attribute("thickness")
                .context("CLI layer thickness missing")?,
            expected,
            1e-6_f64.max(expected.abs() * 1e-5),
        )?;
        let spec_id = actual
            .children()
            .find(|n| n.has_tag_name("SpecRef"))
            .and_then(|n| n.attribute("id"))
            .context("CLI SpecRef missing")?;
        let specs = document
            .descendants()
            .filter(|n| n.has_tag_name("Spec") && n.attribute("name") == Some(spec_id))
            .collect::<Vec<_>>();
        ensure!(specs.len() == 1, "CLI material spec is ambiguous/missing");
        let spec = specs[0];
        if let Some(material) = &wanted.material {
            ensure!(spec.children().filter(|n|n.has_tag_name("General") && n.attribute("type")==Some("MATERIAL")).flat_map(|n|n.children()).any(|n|n.has_tag_name("Property") && n.attribute("text")==Some(material.as_str())),"CLI material differs from request");
        }
        for (kind, value, tolerance) in [
            ("DIELECTRIC_CONSTANT", wanted.epsilon_r, 0.0050001),
            (
                "LOSS_TANGENT",
                wanted.loss_tangent,
                wanted.loss_tangent.map_or(1e-12, |v| {
                    if v == 0.0 {
                        1e-12
                    } else {
                        0.500001 * 10_f64.powf(v.abs().log10().floor() - 2.0) + 1e-12
                    }
                }),
            ),
        ] {
            if let Some(value) = value {
                let properties = spec
                    .children()
                    .filter(|n| n.has_tag_name("Dielectric") && n.attribute("type") == Some(kind))
                    .flat_map(|n| n.children())
                    .filter(|n| n.has_tag_name("Property"))
                    .collect::<Vec<_>>();
                ensure!(
                    properties.len() == 1,
                    "CLI dielectric property missing/ambiguous"
                );
                numeric(
                    properties[0]
                        .attribute("value")
                        .context("CLI dielectric value missing")?,
                    value,
                    tolerance,
                )?;
            }
        }
        if wanted.kind == "core" || wanted.kind == "prepreg" {
            ensure!(
                spec.descendants().any(|n| n.has_tag_name("Property")
                    && n.attribute("text") == Some(format!("Type : {}", wanted.kind).as_str())),
                "CLI dielectric kind differs from request"
            );
        }
    }
    // KiCad exports the physical sum (including solder masks), not the
    // nominal general/thickness. Verify these as separate quantities: exact
    // nominal source is checked by whole-file comparison; XML proves the sum.
    let physical_sum = request
        .layers
        .iter()
        .map(|l| konnect_ipc::builders::try_mm_to_nm(l.thickness_mm))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .try_fold(0_i64, |sum, nm| {
            sum.checked_add(nm).context("stack thickness overflow")
        })?;
    let physical_mm = konnect_ipc::builders::nm_to_mm(physical_sum);
    numeric(
        stackups[0]
            .attribute("overallThickness")
            .context("CLI physical thickness missing")?,
        physical_mm,
        1e-6_f64.max(physical_mm.abs() * 1e-5),
    )?;
    Ok(
        json!({"format":"IPC-2581","layers_verified":layers.len(),"physical_layer_sum_mm":physical_mm,"nominal_thickness_source_verified_separately":request.board_thickness_mm,"thickness_tolerance_mm":"max(0.000001, requested*0.00001)","epsilon_r_export_tolerance":0.0050001,"loss_tangent_export_tolerance":"half last significant digit at three significant digits plus 1e-12"}),
    )
}
async fn cli_readback(ctx: &ToolContext, path: &Path, request: &Request) -> Result<Value> {
    let output = tempfile::Builder::new().suffix(".xml").tempfile()?;
    super::cli::export_ipc2581(&ctx.config.kicad_cli, path, output.path(), "mm", false).await?;
    verify_xml(&std::fs::read_to_string(output.path())?, request)
}
fn response(mut body: Value, board: &str, status: OutcomeStatus) -> CallToolResult {
    body["source"] =
        json!({"board":"closed_saved_file","readback":"complete_source_and_kicad_cli_IPC2581"});
    let complete = status == OutcomeStatus::Complete;
    let mut result = CallToolResult::json(&body);
    result.is_error = !complete;
    outcome::attach(
        result,
        outcome::summary(
            status,
            board,
            "closed_saved_file+kicad_cli",
            1,
            usize::from(complete),
            usize::from(!complete),
            if status == OutcomeStatus::Uncertain {
                Some(outcome::inspect_before_retry())
            } else if !complete {
                Some(outcome::retry_whole_request())
            } else {
                None
            },
        ),
    )
}
fn refused(board: &str, error: impl std::fmt::Display) -> CallToolResult {
    response(
        json!({"status":"conflict","applied":false,"stackups_edited":{"planned":0,"applied":0},"reason":error.to_string()}),
        board,
        OutcomeStatus::Failed,
    )
}
async fn handle_set_stackup(args: &Value, ctx: &ToolContext) -> Result<CallToolResult> {
    let target = args["board"].as_str().unwrap_or("unresolved board");
    let request = match request(args) {
        Ok(r) => r,
        Err(e) => return Ok(refused(target, e)),
    };
    let board = get_path(args, "board")?;
    let evidence = match closed(ctx, &board).await {
        Ok(e) => e,
        Err(e) => return Ok(refused(target, e)),
    };
    let source = read_consistent(&board)?;
    let plan = match plan(&source, &request) {
        Ok(p) => p,
        Err(e) => return Ok(refused(target, e)),
    };
    if !request.dry_run && request.expected.as_deref() != Some(plan.revision.as_str()) {
        return Ok(refused(
            target,
            "stale_plan_revision: saved source or request changed",
        ));
    }
    let candidate = tempfile::Builder::new().suffix(".kicad_pcb").tempfile()?;
    std::fs::write(candidate.path(), &plan.candidate)?;
    let candidate_readback = match cli_readback(ctx, candidate.path(), &request).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(refused(
                target,
                format!("candidate CLI reopen/readback failed: {e:#}"),
            ))
        }
    };
    if let Err(e) = closed(ctx, &board).await {
        return Ok(refused(target, e));
    }
    if read_consistent(&board)? != source {
        return Ok(refused(
            target,
            "saved board changed during candidate CLI validation; rerun dry_run",
        ));
    }
    if request.dry_run {
        return Ok(response(
            json!({"status":"ready","dry_run":true,"applied":false,"plan_revision":plan.revision,"before":plan.before,"after":plan.after,"closed_state_evidence":evidence,"candidate_cli_readback":candidate_readback,"stackups_edited":{"planned":usize::from(plan.candidate!=source),"applied":0}}),
            target,
            OutcomeStatus::Complete,
        ));
    }
    if let Err(e) = closed(ctx, &board).await {
        return Ok(refused(target, e));
    }
    let preimage = board.with_file_name(format!(
        "{}.stackup-{}.preimage",
        board
            .file_name()
            .context("filename missing")?
            .to_string_lossy(),
        format!("{:x}", Sha256::digest(source.as_bytes()))
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&preimage)
    {
        Ok(mut f) => {
            f.write_all(source.as_bytes())?;
            f.sync_all()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => ensure!(
            std::fs::read(&preimage)? == source.as_bytes(),
            "preimage collision"
        ),
        Err(e) => return Ok(refused(target, e)),
    }
    if plan.candidate == source {
        return Ok(response(
            json!({"status":"noop","applied":false,"plan_revision":plan.revision,"before":plan.before,"after":plan.after,"readback_verified":true,"candidate_cli_readback":candidate_readback,"stackups_edited":{"planned":0,"applied":0}}),
            target,
            OutcomeStatus::Complete,
        ));
    }
    if let Err(e) = write_atomic_if_unchanged(&board, &source, &plan.candidate) {
        if std::fs::read_to_string(&board).is_ok_and(|current| current == source) {
            return Ok(refused(target, e));
        }
        return Ok(response(
            json!({"status":"uncertain","applied":false,"potentially_applied":true,"stackups_edited":{"planned":1,"applied":0},"reason":e.to_string(),"preimage":preimage,"recovery":"Saved state changed or could not be read after the atomic write error; inspect before retrying, do not assume rollback."}),
            target,
            OutcomeStatus::Uncertain,
        ));
    }
    let verified = async {
        ensure!(
            std::fs::read_to_string(&board)? == plan.candidate,
            "complete saved-file readback differs from exact candidate"
        );
        strict(&std::fs::read_to_string(&board)?)?;
        closed(ctx, &board).await?;
        let cli = cli_readback(ctx, &board, &request).await?;
        ensure!(
            std::fs::read_to_string(&board)? == plan.candidate,
            "saved board changed during CLI readback"
        );
        Ok::<Value, anyhow::Error>(cli)
    }
    .await;
    Ok(match verified {
        Ok(cli) => response(
            json!({"status":"applied","applied":true,"plan_revision":plan.revision,"before":plan.before,"after":plan.after,"stackups_edited":{"planned":1,"applied":1},"readback_verified":true,"unrelated_source_bytes_preserved":true,"cli_readback":cli,"preimage":preimage,"reopen_required":true,"native_undo":false}),
            target,
            OutcomeStatus::Complete,
        ),
        Err(e) => response(
            json!({"status":"uncertain","applied":false,"potentially_applied":true,"stackups_edited":{"planned":1,"applied":0},"reason":format!("{e:#}"),"preimage":preimage,"recovery":"Inspect the saved source and CLI evidence before retrying. Do not overwrite later edits with the preimage or reopen a stale GUI copy automatically."}),
            target,
            OutcomeStatus::Uncertain,
        ),
    })
}

#[cfg(test)]
mod tests;
