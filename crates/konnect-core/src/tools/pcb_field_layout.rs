//! Previewed instance-field layout edits with exact, independent board readback.

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
    tool!("edit_footprint_field_layout",
        "Batch-edit placed Reference/Value field position (absolute board mm), absolute angle, text size and visibility. Exact footprint UUID and reference required for each edit. Defaults to preview; apply requires its plan_revision covering the entire live PCB and exact edits. One native undo commit, no automatic save. Independent full-board readback must prove only the requested field layout changed; never moves pads, copper, footprint pose or library graphics. Uncertain results require inspection before retry. Requires the requested board open over IPC.",
        json!({"type":"object","additionalProperties":false,"properties":{
            "board":{"type":"string"},"dry_run":{"type":"boolean","default":true},
            "expected_plan_revision":{"type":"string","pattern":"^[0-9a-f]{64}$"},
            "changes":{"type":"array","minItems":1,"items":{"type":"object","additionalProperties":false,
                "properties":{"reference":{"type":"string","minLength":1},"footprint_uuid":{"type":"string","format":"uuid"},
                    "field":{"type":"string","enum":["Reference","Value"]},
                    "position":{"type":"object","additionalProperties":false,"properties":{"x":{"type":"number"},"y":{"type":"number"}},"required":["x","y"]},
                    "angle_deg":{"type":"number"},
                    "size":{"type":"object","additionalProperties":false,"properties":{"x":{"type":"number","exclusiveMinimum":0},"y":{"type":"number","exclusiveMinimum":0}},"required":["x","y"]},
                    "visible":{"type":"boolean"}},"required":["reference","footprint_uuid","field"],
                "anyOf":[{"required":["position"]},{"required":["angle_deg"]},{"required":["size"]},{"required":["visible"]}]}}
        },"required":["board","changes"]}),
        |args, ctx| async move { handle_field_layout(args, ctx).await }
    ).with_board_access(crate::tools::BoardAccess::LiveOnly)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Point {
    x: f64,
    y: f64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
enum Field {
    Reference,
    Value,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Change {
    reference: String,
    footprint_uuid: String,
    field: Field,
    #[serde(skip_serializing_if = "Option::is_none")]
    position: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    angle_deg: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<Point>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visible: Option<bool>,
}
struct Request {
    board: String,
    changes: Vec<Change>,
    dry_run: bool,
    expected_plan_revision: Option<String>,
}
fn parse_request(args: &Value) -> Result<Request> {
    let object = args.as_object().context("request must be an object")?;
    ensure!(
        object.keys().all(
            |k| ["board", "changes", "dry_run", "expected_plan_revision"].contains(&k.as_str())
        ),
        "unknown request property"
    );
    let board = args["board"]
        .as_str()
        .context("board must be a string")?
        .to_string();
    let changes = args["changes"]
        .as_array()
        .context("changes must be an array")?;
    for change in changes {
        let object = change
            .as_object()
            .context("each change must be an object")?;
        ensure!(
            !object.values().any(Value::is_null),
            "present change properties cannot be null"
        );
    }
    let changes = serde_json::from_value(args["changes"].clone())?;
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
    Ok(Request {
        board,
        changes,
        dry_run,
        expected_plan_revision,
    })
}

fn nm(point: &Point) -> Result<kiapi::common::types::Vector2> {
    Ok(kiapi::common::types::Vector2 {
        x_nm: builders::try_mm_to_nm(point.x)?,
        y_nm: builders::try_mm_to_nm(point.y)?,
    })
}
fn validate(request: &Request) -> Result<()> {
    ensure!(
        !request.board.is_empty() && !request.changes.is_empty(),
        "board and nonempty changes required"
    );
    if let Some(revision) = &request.expected_plan_revision {
        ensure!(
            revision.len() == 64
                && revision
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid plan revision"
        );
    }
    ensure!(
        request.dry_run || request.expected_plan_revision.is_some(),
        "apply requires expected_plan_revision from current dry_run"
    );
    let mut keys = BTreeSet::new();
    for change in &request.changes {
        ensure!(!change.reference.is_empty(), "reference is empty");
        ensure!(
            uuid::Uuid::parse_str(&change.footprint_uuid)?.to_string() == change.footprint_uuid,
            "footprint UUID must be canonical"
        );
        ensure!(
            keys.insert((&change.footprint_uuid, &change.field)),
            "duplicate field edits are ambiguous"
        );
        ensure!(
            change.position.is_some()
                || change.angle_deg.is_some()
                || change.size.is_some()
                || change.visible.is_some(),
            "field edit has no layout change"
        );
        if let Some(position) = &change.position {
            nm(position)?;
        }
        if let Some(size) = &change.size {
            let size = nm(size)?;
            ensure!(
                size.x_nm > 0 && size.y_nm > 0,
                "text size must round to positive nanometers"
            );
        }
        if let Some(angle) = change.angle_deg {
            ensure!(angle.is_finite(), "angle must be finite");
        }
    }
    Ok(())
}

struct Plan {
    next: RawBoard,
    updates: Vec<prost_types::Any>,
    preview: Vec<Value>,
    changed: usize,
}
fn text(field: &Option<kiapi::board::types::Field>) -> Option<&str> {
    Some(field.as_ref()?.text.as_ref()?.text.as_ref()?.text.as_str())
}
fn layout(field: &kiapi::board::types::Field) -> Result<Value> {
    let text = field
        .text
        .as_ref()
        .and_then(|t| t.text.as_ref())
        .context("field has no text")?;
    let attributes = text
        .attributes
        .as_ref()
        .context("field has no text attributes")?;
    let point = |p: &Option<kiapi::common::types::Vector2>| {
        p.as_ref().map(|p|json!({"x_nm":p.x_nm,"y_nm":p.y_nm,"x":builders::nm_to_mm(p.x_nm),"y":builders::nm_to_mm(p.y_nm)}))
    };
    Ok(
        json!({"position":point(&text.position),"angle_deg":attributes.angle.as_ref().map(|a|a.value_degrees),"size":point(&attributes.size),"visible":field.visible}),
    )
}
fn plan(changes: &[Change], before: &RawBoard) -> Result<Plan> {
    let mut footprints = BTreeMap::new();
    let mut references = BTreeSet::new();
    for (index, item) in before.items.iter().enumerate() {
        if !builders::any_is(item, "kiapi.board.types.FootprintInstance") {
            continue;
        }
        let fp = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())?;
        let id = fp
            .id
            .as_ref()
            .context("footprint has no UUID")?
            .value
            .clone();
        ensure!(
            references.insert(
                text(&fp.reference_field)
                    .context("footprint has no reference")?
                    .to_string()
            ),
            "ambiguous board reference"
        );
        ensure!(
            footprints.insert(id, (index, fp)).is_none(),
            "duplicate footprint UUID"
        );
    }
    let mut preview = Vec::new();
    let mut changed = 0;
    let mut touched = BTreeSet::new();
    for change in changes {
        let (index, fp) = footprints
            .get_mut(&change.footprint_uuid)
            .context("requested footprint UUID not present")?;
        if touched.insert(change.footprint_uuid.clone()) {
            ensure!(
                fp.encode_to_vec() == before.items[*index].value,
                "target has unsupported protobuf fields; refuse lossy edit"
            );
        }
        ensure!(
            text(&fp.reference_field) == Some(change.reference.as_str()),
            "UUID does not uniquely match the requested reference"
        );
        let field = match change.field {
            Field::Reference => &mut fp.reference_field,
            Field::Value => &mut fp.value_field,
        };
        let field = field.as_mut().context("requested placed field is absent")?;
        let old = field.clone();
        let old_layout = layout(&old)?;
        let text = field
            .text
            .as_mut()
            .and_then(|t| t.text.as_mut())
            .context("field has no text")?;
        if let Some(position) = &change.position {
            text.position = Some(nm(position)?);
        }
        let attributes = text
            .attributes
            .as_mut()
            .context("field has no attributes")?;
        if let Some(angle) = change.angle_deg {
            let angle = angle.rem_euclid(360.0);
            attributes.angle = Some(kiapi::common::types::Angle {
                value_degrees: if angle == 0.0 { 0.0 } else { angle },
            });
        }
        if let Some(size) = &change.size {
            attributes.size = Some(nm(size)?);
        }
        if let Some(visible) = change.visible {
            field.visible = visible;
        }
        let differs = *field != old;
        changed += usize::from(differs);
        preview.push(json!({"reference":change.reference,"footprint_uuid":change.footprint_uuid,"field":change.field,"changed":differs,"before":old_layout,"after":layout(field)?}));
    }
    let mut next = before.clone();
    let mut updates = Vec::new();
    for id in touched {
        let (index, fp) = &footprints[&id];
        let item = builders::pack_any(fp, "kiapi.board.types.FootprintInstance");
        if item != before.items[*index] {
            next.items[*index] = item.clone();
            updates.push(item);
        }
    }
    next.items = sorted_items(next.items);
    Ok(Plan {
        next,
        updates,
        preview,
        changed,
    })
}
fn revision(changes: &[Change], board: &RawBoard) -> String {
    let bytes = serde_json::to_vec(&(
        changes,
        board.document.encode_to_vec(),
        board
            .items
            .iter()
            .map(|a| (&a.type_url, &a.value))
            .collect::<Vec<_>>(),
        &board.nets,
    ))
    .expect("validated finite request and board");
    format!("{:x}", Sha256::digest(bytes))
}
fn response(mut value: Value, board: &str, status: OutcomeStatus) -> CallToolResult {
    value["source"] = json!({"board":"live_kicad_ipc", "readback":"independent_complete_board"});
    let complete = status == OutcomeStatus::Complete;
    let mut result = CallToolResult::json(&value);
    result.is_error = !complete;
    outcome::attach(
        result,
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
fn refused(board: &str, error: impl std::fmt::Display) -> CallToolResult {
    response(
        json!({"status":"conflict","applied":false,"fields_edited":{"planned":0,"applied":0},"reason":error.to_string()}),
        board,
        OutcomeStatus::Failed,
    )
}
async fn handle_field_layout(args: &Value, ctx: &ToolContext) -> Result<CallToolResult> {
    let target = args["board"].as_str().unwrap_or("unresolved board");
    let request: Request = match parse_request(args) {
        Ok(r) => r,
        Err(e) => return Ok(refused(target, e)),
    };
    if let Err(error) = validate(&request) {
        return Ok(refused(target, error));
    }
    let board = get_path(args, "board")?;
    let ipc_board = board.clone();
    let name = target.to_string();
    let outcome=attempt_ipc_write(ctx,&board,"field layout edit",move |client| {
        let before=match raw_board(client,&ipc_board) {Ok(b)=>b,Err(e)=>return Ok(refused(&name,e))};
        let plan=match plan(&request.changes,&before) {Ok(p)=>p,Err(e)=>return Ok(refused(&name,e))};
        let revision=revision(&request.changes,&before);
        if !request.dry_run && request.expected_plan_revision.as_deref()!=Some(revision.as_str()) {return Ok(refused(&name,"stale_plan_revision: board or exact edits changed; rerun dry_run"));}
        if request.dry_run {return Ok(response(json!({"status":"ready","dry_run":true,"applied":false,"plan_revision":revision,"preview":plan.preview,"live_items_covered":before.items.len(),"fields_edited":{"planned":plan.changed,"applied":0},"footprints_edited":plan.updates.len()}),&name,OutcomeStatus::Complete));}
        let mut attempted=false;
        let result=if plan.updates.is_empty() {Ok(())} else {client.run_commit("Edit placed footprint field layout",|client| {
            ensure!(raw_board(client,&ipc_board)?==before,"live board changed before update");
            attempted=true;client.update_items_in(before.document.clone(),plan.updates.clone())?;Ok(())
        })};
        let verified=result.and_then(|()| {ensure!(raw_board(client,&ipc_board)?==plan.next,"full-board readback differs from the exact field-only expected result");Ok(())});
        match verified {
            Ok(())=>Ok(response(json!({"status":"applied","applied":true,"plan_revision":revision,"preview":plan.preview,"fields_edited":{"planned":plan.changed,"applied":plan.changed},"footprints_edited":plan.updates.len(),"live_items_covered":before.items.len(),"readback_verified":true,"unrelated_items_and_routing_verified":true,"board_saved":false}),&name,OutcomeStatus::Complete)),
            Err(e) if !attempted=>Ok(refused(&name,e)),
            Err(e)=>Ok(response(json!({"status":"uncertain","applied":false,"potentially_applied":true,"fields_edited":{"planned":plan.changed,"applied":0},"preview":plan.preview,"reason":e.to_string(),"recovery":"Inspect the live fields and unrelated board state before retrying; do not assume rollback or save automatically."}),&name,OutcomeStatus::Uncertain)),
        }
    }).await?;
    Ok(match outcome {
        BoardWrite::Ipc(r) => r,
        BoardWrite::File(reason) => refused(
            target,
            format!("{} field editing requires live IPC", reason.premise()),
        ),
        BoardWrite::Refused(r) => refused(
            target,
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
