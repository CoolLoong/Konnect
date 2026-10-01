//! Explicit, previewed native footprint side changes. No file fallback.
use crate::{
    mcp::protocol::{CallToolResult, ToolContent},
    outcome::{self, OutcomeStatus},
    tool,
    tools::{
        get_path,
        pcb_board::{attempt_ipc_write, BoardWrite},
        pcb_live_snapshot::{raw_board, sorted_items, RawBoard},
        ToolContext, ToolDef,
    },
};
use anyhow::{ensure, Context, Result};
use konnect_ipc::{builders, gen::kiapi};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn tool() -> ToolDef {
    tool!("batch_flip_components", "Preview and apply explicit F.Cu/B.Cu sides for placed footprints using KiCad's native FlipItems, including pads, graphics, fields and 3D transforms. Each target requires its exact reference and UUID. Already-correct sides are preserved. Dry run defaults true; apply requires its whole-board/exact-request plan_revision. One native undo commit, independent full-board readback, no automatic save or file fallback. Requires the requested PCB open over native IPC with FlipItems support; uncertain results require inspection before retry.",
        json!({"type":"object","additionalProperties":false,"properties":{
            "board":{"type":"string","minLength":1},"dry_run":{"type":"boolean","default":true},
            "expected_plan_revision":{"type":"string","pattern":"^[0-9a-f]{64}$"},
            "changes":{"type":"array","minItems":1,"items":{"type":"object","additionalProperties":false,
                "properties":{"reference":{"type":"string","minLength":1},"footprint_uuid":{"type":"string","format":"uuid"},"layer":{"type":"string","enum":["F.Cu","B.Cu"]}},
                "required":["reference","footprint_uuid","layer"]}}},"required":["board","changes"]}),
        |args,ctx|async move{handle_native_flip(args,ctx).await}).with_board_access(crate::tools::BoardAccess::LiveOnly)
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Change {
    reference: String,
    footprint_uuid: String,
    layer: String,
}
struct Request {
    changes: Vec<Change>,
    dry_run: bool,
    expected_plan_revision: Option<String>,
}
fn request(args: &Value) -> Result<Request> {
    let object = args.as_object().context("request must be object")?;
    ensure!(
        object.keys().all(
            |k| ["board", "changes", "dry_run", "expected_plan_revision"].contains(&k.as_str())
        ),
        "unknown request property"
    );
    ensure!(
        !args["board"]
            .as_str()
            .context("board must be string")?
            .is_empty(),
        "empty board"
    );
    let changes: Vec<Change> = serde_json::from_value(args["changes"].clone())?;
    ensure!(!changes.is_empty(), "empty changes");
    let mut ids = BTreeSet::new();
    for c in &changes {
        ensure!(!c.reference.is_empty(), "empty reference");
        ensure!(
            uuid::Uuid::parse_str(&c.footprint_uuid)?.to_string() == c.footprint_uuid,
            "noncanonical UUID"
        );
        ensure!(ids.insert(&c.footprint_uuid), "duplicate flip UUID");
        ensure!(
            ["F.Cu", "B.Cu"].contains(&c.layer.as_str()),
            "invalid target layer"
        );
    }
    let dry_run = match args.get("dry_run") {
        None => true,
        Some(v) => v.as_bool().context("dry_run must be boolean")?,
    };
    let expected_plan_revision = match args.get("expected_plan_revision") {
        None => None,
        Some(v) => Some(
            v.as_str()
                .context("expected_plan_revision must be string")?
                .to_string(),
        ),
    };
    if let Some(r) = &expected_plan_revision {
        ensure!(
            r.len() == 64
                && r.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid revision"
        );
    }
    ensure!(
        dry_run || expected_plan_revision.is_some(),
        "apply requires preview revision"
    );
    Ok(Request {
        changes,
        dry_run,
        expected_plan_revision,
    })
}
fn text(f: &Option<kiapi::board::types::Field>) -> Option<&str> {
    Some(f.as_ref()?.text.as_ref()?.text.as_ref()?.text.as_str())
}
fn footprints(
    b: &RawBoard,
) -> Result<BTreeMap<String, (usize, kiapi::board::types::FootprintInstance)>> {
    let mut result = BTreeMap::new();
    let mut refs = BTreeSet::new();
    for (i, a) in b
        .items
        .iter()
        .enumerate()
        .filter(|(_, a)| builders::any_is(a, "kiapi.board.types.FootprintInstance"))
    {
        let fp = kiapi::board::types::FootprintInstance::decode(a.value.as_slice())?;
        let id = fp
            .id
            .as_ref()
            .context("footprint lacks UUID")?
            .value
            .clone();
        ensure!(
            refs.insert(
                text(&fp.reference_field)
                    .context("footprint lacks reference")?
                    .to_string()
            ),
            "ambiguous board reference"
        );
        ensure!(
            result.insert(id, (i, fp)).is_none(),
            "duplicate board footprint UUID"
        );
    }
    Ok(result)
}
struct Plan {
    targets: BTreeMap<String, (usize, kiapi::board::types::FootprintInstance, Change)>,
    ids: Vec<String>,
    preview: Vec<Value>,
}
fn plan(changes: &[Change], b: &RawBoard) -> Result<Plan> {
    let all = footprints(b)?;
    let mut child_pads = BTreeMap::new();
    for (_, fp) in all.values() {
        for pad in &fp
            .definition
            .as_ref()
            .context("footprint definition missing")?
            .items
        {
            if builders::any_is(pad, "kiapi.board.types.Pad") {
                let id = kiapi::board::types::Pad::decode(pad.value.as_slice())?
                    .id
                    .context("child pad lacks UUID")?
                    .value;
                ensure!(
                    child_pads.insert(id, pad).is_none(),
                    "duplicate physical pad UUID"
                );
            }
        }
    }
    let mut aliases = BTreeSet::new();
    for pad in b
        .items
        .iter()
        .filter(|a| builders::any_is(a, "kiapi.board.types.Pad"))
    {
        let id = kiapi::board::types::Pad::decode(pad.value.as_slice())?
            .id
            .context("catalogue pad lacks UUID")?
            .value;
        ensure!(aliases.insert(id.clone()), "duplicate catalogue pad UUID");
        ensure!(
            child_pads.get(&id) == Some(&pad),
            "catalogue/footprint pad evidence disagrees"
        );
    }
    let mut targets = BTreeMap::new();
    let mut ids = Vec::new();
    let mut preview = Vec::new();
    for c in changes {
        let (i, fp) = all
            .get(&c.footprint_uuid)
            .context("footprint UUID absent")?;
        ensure!(
            text(&fp.reference_field) == Some(c.reference.as_str()),
            "reference/UUID mismatch"
        );
        // FlipItems sends UUIDs only. Do not re-encode target footprints: native
        // negative-zero/default encodings and unknown fields stay with KiCad.
        // The complete returned raw payload must match independent readback.
        ensure!(
            [
                builders::layer_from_name("F.Cu") as i32,
                builders::layer_from_name("B.Cu") as i32
            ]
            .contains(&fp.layer),
            "footprint side is not F.Cu/B.Cu"
        );
        // Establish invariant evidence before any native mutation.
        identity(fp)?;
        let changed = fp.layer != builders::layer_from_name(&c.layer) as i32;
        preview.push(json!({"reference":c.reference,"footprint_uuid":c.footprint_uuid,"before":if fp.layer==builders::layer_from_name("F.Cu") as i32 {"F.Cu"}else{"B.Cu"},"after":c.layer,"changed":changed}));
        if changed {
            ids.push(c.footprint_uuid.clone());
            targets.insert(c.footprint_uuid.clone(), (*i, fp.clone(), c.clone()));
        }
    }
    Ok(Plan {
        targets,
        ids,
        preview,
    })
}
#[derive(Clone, PartialEq, Message)]
struct ItemIdentity {
    #[prost(message, optional, tag = "1")]
    id: Option<kiapi::common::types::Kiid>,
}
fn field_identity(field: &Option<kiapi::board::types::Field>) -> Result<Value> {
    match field {
        None => Ok(Value::Null),
        Some(f) => Ok(
            json!({"id":f.id.as_ref().map(Message::encode_to_vec),"name":f.name,"visible":f.visible,
        "text":f.text.as_ref().and_then(|t|t.text.as_ref()).context("field lacks text")?.text,
        "text_id":f.text.as_ref().and_then(|t|t.id.as_ref()).map(Message::encode_to_vec)}),
        ),
    }
}
/// Non-geometric identity must survive the native transform. Geometry remains
/// owned by KiCad; the entire returned native payload is independently read back.
fn identity(fp: &kiapi::board::types::FootprintInstance) -> Result<Value> {
    let def = fp
        .definition
        .as_ref()
        .context("footprint lacks definition")?;
    let mut children = Vec::new();
    for a in &def.items {
        let v = if builders::any_is(a, "kiapi.board.types.Pad") {
            let p = kiapi::board::types::Pad::decode(a.value.as_slice())?;
            json!({"type":a.type_url,"id":p.id.as_ref().context("pad lacks UUID")?.value,"number":p.number,"net":p.net.as_ref().map(Message::encode_to_vec),"locked":p.locked,"pad_type":p.r#type,
                "clearance":p.copper_clearance_override.as_ref().map(Message::encode_to_vec),"die_length":p.pad_to_die_length.as_ref().map(Message::encode_to_vec),"pin":p.symbol_pin.as_ref().map(Message::encode_to_vec),"delay":p.pad_to_die_delay.as_ref().map(Message::encode_to_vec),"parent":p.parent.as_ref().map(Message::encode_to_vec)})
        } else if builders::any_is(a, "kiapi.board.types.BoardGraphicShape") {
            let shape = kiapi::board::types::BoardGraphicShape::decode(a.value.as_slice())?;
            json!({"type":a.type_url,"id":shape.id.as_ref().context("graphic lacks UUID")?.value,
                "net":shape.net.as_ref().map(Message::encode_to_vec),"locked":shape.locked,"parent":shape.parent.as_ref().map(Message::encode_to_vec)})
        } else if builders::any_is(a, "kiapi.board.types.BoardText") {
            let text = kiapi::board::types::BoardText::decode(a.value.as_slice())?;
            json!({"type":a.type_url,"id":text.id.as_ref().context("text lacks UUID")?.value,
                "text":text.text.as_ref().context("text content missing")?.text,"locked":text.locked,"parent":text.parent.as_ref().map(Message::encode_to_vec)})
        } else if builders::any_is(a, "kiapi.board.types.Footprint3DModel") {
            let m = kiapi::board::types::Footprint3DModel::decode(a.value.as_slice())?;
            json!({"type":a.type_url,"filename":m.filename,"scale":m.scale.as_ref().map(Message::encode_to_vec),"visible":m.visible,"opacity":m.opacity})
        } else if builders::any_is(a, "kiapi.board.types.Field") {
            json!({"type":a.type_url,"field":field_identity(&Some(kiapi::board::types::Field::decode(a.value.as_slice())?))?})
        } else {
            let id = ItemIdentity::decode(a.value.as_slice())
                .with_context(|| format!("unsupported child {}", a.type_url))?
                .id
                .context("child lacks established UUID identity")?
                .value;
            uuid::Uuid::parse_str(&id)
                .with_context(|| format!("child UUID unsupported for {}", a.type_url))?;
            json!({"type":a.type_url,"id":id})
        };
        children.push(v);
    }
    children.sort_by_key(Value::to_string);
    Ok(
        json!({"id":fp.id.as_ref().map(Message::encode_to_vec),"position":fp.position.as_ref().map(Message::encode_to_vec),"locked":fp.locked,"attributes":fp.attributes.as_ref().map(Message::encode_to_vec),"overrides":fp.overrides.as_ref().map(Message::encode_to_vec),
        "symbol_path":fp.symbol_path.as_ref().map(Message::encode_to_vec),"sheet_name":fp.symbol_sheet_name,"sheet_file":fp.symbol_sheet_filename,"filters":fp.symbol_footprint_filters,"parent":fp.parent.as_ref().map(Message::encode_to_vec),
        "definition_id":def.id.as_ref().map(Message::encode_to_vec),"anchor":def.anchor.as_ref().map(Message::encode_to_vec),"def_attributes":def.attributes.as_ref().map(Message::encode_to_vec),"def_overrides":def.overrides.as_ref().map(Message::encode_to_vec),"net_ties":def.net_ties.iter().map(Message::encode_to_vec).collect::<Vec<_>>(),"jumpers":def.jumpers.as_ref().map(Message::encode_to_vec),
        "fields":[field_identity(&fp.reference_field)?,field_identity(&fp.value_field)?,field_identity(&fp.datasheet_field)?,field_identity(&fp.description_field)?,field_identity(&def.reference_field)?,field_identity(&def.value_field)?,field_identity(&def.datasheet_field)?,field_identity(&def.description_field)?],"children":children}),
    )
}
fn expected(before: &RawBoard, p: &Plan, native: Vec<prost_types::Any>) -> Result<RawBoard> {
    ensure!(native.len() == p.ids.len(), "incomplete native results");
    let mut next = before.clone();
    let mut ids = BTreeSet::new();
    let mut pads = BTreeMap::new();
    for a in native {
        ensure!(
            builders::any_is(&a, "kiapi.board.types.FootprintInstance"),
            "unexpected native item type"
        );
        let fp = kiapi::board::types::FootprintInstance::decode(a.value.as_slice())?;
        let id = fp
            .id
            .as_ref()
            .context("native target lacks UUID")?
            .value
            .clone();
        ensure!(ids.insert(id.clone()), "duplicate native target");
        let (i, old, c) = p.targets.get(&id).context("native target not requested")?;
        ensure!(
            fp.layer == builders::layer_from_name(&c.layer) as i32,
            "native target side differs"
        );
        ensure!(
            identity(old)? == identity(&fp)?,
            "native flip changed footprint identity, anchor, fields, attributes or pad nets"
        );
        for pad in &fp
            .definition
            .as_ref()
            .context("native definition missing")?
            .items
        {
            if builders::any_is(pad, "kiapi.board.types.Pad") {
                let id = kiapi::board::types::Pad::decode(pad.value.as_slice())?
                    .id
                    .context("native child pad UUID missing")?
                    .value;
                ensure!(
                    pads.insert(id, pad.clone()).is_none(),
                    "duplicate native child pad UUID"
                );
            }
        }
        next.items[*i] = a;
    }
    // GetItems(KOT_PCB_PAD) also returns the footprint's pads as independent
    // raw catalogue entries. They carry the same native serialization as the
    // children in the FlipItems payload; mirror those aliases, not other pads.
    for item in &mut next.items {
        if builders::any_is(item, "kiapi.board.types.Pad") {
            let id = kiapi::board::types::Pad::decode(item.value.as_slice())?
                .id
                .context("catalogue pad UUID missing")?
                .value;
            if let Some(native) = pads.get(&id) {
                *item = native.clone();
            }
        }
    }
    next.items = sorted_items(next.items);
    Ok(next)
}
fn revision(changes: &[Change], b: &RawBoard) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                changes,
                b.document.encode_to_vec(),
                b.items
                    .iter()
                    .map(|a| (&a.type_url, &a.value))
                    .collect::<Vec<_>>(),
                &b.nets
            ))
            .expect("serializable request")
        )
    )
}
fn verify_board(actual: &RawBoard, expected: &RawBoard) -> Result<()> {
    #[cfg(test)]
    if actual != expected {
        if let Ok(out) = std::env::var("KONNECT_FLIP_EVIDENCE") {
            let out = std::path::PathBuf::from(out);
            std::fs::create_dir_all(&out)?;
            for (label, b) in [("expected", expected), ("observed", actual)] {
                std::fs::write(
                    out.join(format!("debug-{label}.json")),
                    serde_json::to_vec_pretty(
                        &json!({"items":b.items.iter().map(|a|(&a.type_url,&a.value)).collect::<Vec<_>>(),"nets":b.nets}),
                    )?,
                )?;
                for (_, fp) in footprints(b)?.values() {
                    if [Some("J2"), Some("U2")].contains(&text(&fp.reference_field)) {
                        std::fs::write(
                            out.join(format!(
                                "debug-{label}-{}.txt",
                                text(&fp.reference_field).unwrap()
                            )),
                            format!("{fp:#?}"),
                        )?;
                    }
                }
            }
        }
    }
    ensure!(
        actual == expected,
        "full-board readback differs from native flip result"
    );
    Ok(())
}
fn response(mut v: Value, board: &str, status: OutcomeStatus) -> CallToolResult {
    v["source"] = json!({"board":"live_kicad_ipc","mutation":"native_FlipItems","readback":"independent_complete_board"});
    let complete = status == OutcomeStatus::Complete;
    let mut r = CallToolResult::json(&v);
    r.is_error = !complete;
    outcome::attach(
        r,
        outcome::summary(
            status,
            board,
            "live_kicad_ipc",
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
fn refused(board: &str, e: impl std::fmt::Display) -> CallToolResult {
    response(
        json!({"status":"conflict","applied":false,"footprints_flipped":{"planned":0,"applied":0},"reason":e.to_string()}),
        board,
        OutcomeStatus::Failed,
    )
}
async fn handle_native_flip(args: &Value, ctx: &ToolContext) -> Result<CallToolResult> {
    let name = args["board"].as_str().unwrap_or("unresolved board");
    let r = match request(args) {
        Ok(r) => r,
        Err(e) => return Ok(refused(name, e)),
    };
    let board = get_path(args, "board")?;
    let ipc_board = board.clone();
    let target = name.to_string();
    let outcome=attempt_ipc_write(ctx,&board,"native batch footprint flip",move|client|{
        let before=match raw_board(client,&ipc_board){Ok(b)=>b,Err(e)=>return Ok(refused(&target,e))};
        let p=match plan(&r.changes,&before){Ok(p)=>p,Err(e)=>return Ok(refused(&target,e))};
        let rev=revision(&r.changes,&before);
        if !r.dry_run&&r.expected_plan_revision.as_deref()!=Some(rev.as_str()){return Ok(refused(&target,"stale_plan_revision: board or exact requested sides changed; rerun dry_run"));}
        if r.dry_run{return Ok(response(json!({"status":"ready","dry_run":true,"applied":false,"plan_revision":rev,"preview":p.preview,"footprints_flipped":{"planned":p.ids.len(),"applied":0},"live_items_covered":before.items.len(),"capability":"native FlipItems support is established on apply, never inferred from version"}),&target,OutcomeStatus::Complete));}
        let mut attempted=false;
        let result=if p.ids.is_empty(){Ok(before.clone())}else{client.run_commit("Flip explicit footprint sides",|client|{
            ensure!(raw_board(client,&ipc_board)?==before,"live board changed before native flip");
            attempted=true;
            let native=client.flip_footprints_native_in(before.document.clone(),&p.ids)?;
            let next=expected(&before,&p,native)?;
            verify_board(&raw_board(client,&ipc_board)?,&next)?;
            Ok(next)
        })};
        let verified=result.and_then(|next|verify_board(&raw_board(client,&ipc_board)?,&next));
        match verified {
            Ok(())=>Ok(response(json!({"status":if p.ids.is_empty(){"noop"}else{"applied"},"applied":!p.ids.is_empty(),"plan_revision":rev,"preview":p.preview,"footprints_flipped":{"planned":p.ids.len(),"applied":p.ids.len()},"live_items_covered":before.items.len(),"readback_verified":true,"identity_and_pad_nets_verified":true,"unrelated_items_and_routing_verified":true,"native_undo":!p.ids.is_empty(),"board_saved":false}),&target,OutcomeStatus::Complete)),
            Err(e) if !attempted=>Ok(refused(&target,e)),
            Err(e)=>Ok(response(json!({"status":"uncertain","applied":false,"potentially_applied":true,"preview":p.preview,"footprints_flipped":{"planned":p.ids.len(),"applied":0},"reason":format!("{e:#}"),"recovery":"Inspect live footprint sides, geometry and unrelated board state before retry; never assume rollback or save automatically."}),&target,OutcomeStatus::Uncertain)),
        }
    }).await?;
    Ok(match outcome {
        BoardWrite::Ipc(r) => r,
        BoardWrite::File(reason) => refused(
            name,
            format!(
                "{} native flip requires live IPC; no file fallback",
                reason.premise()
            ),
        ),
        BoardWrite::Refused(r) => refused(
            name,
            r.content
                .iter()
                .find_map(|c| match c {
                    ToolContent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or("IPC refused"),
        ),
    })
}
#[cfg(test)]
mod tests;
