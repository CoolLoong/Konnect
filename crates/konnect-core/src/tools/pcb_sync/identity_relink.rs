//! Explicit, electrically equivalent identity replacement; never reference adoption.

use super::super::pcb_live_snapshot::{raw_board, sorted_items, RawBoard};
use super::*;
use crate::{
    outcome::{self, OutcomeStatus},
    tool,
    tools::ToolDef,
};
use anyhow::ensure;
use konnect_ipc::gen::kiapi;
use serde_json::{json, Value};

pub(crate) fn tool() -> ToolDef {
    tool!(
        "relink_pcb_footprint_to_schematic",
        "Explicitly replace ONE existing footprint's schematic symbol path after a symbol \
         UUID replacement. Requires exact reference, footprint UUID, old and new complete \
         symbol paths. Proves the old identity is absent from the saved schematic, the new \
         identity is unique, the assigned footprint matches, and EVERY physical pad keeps \
         its net. Changes only symbol_path in one live KiCad undo commit; never creates, \
         deletes, refreshes, saves or rewrites fields/pads. Defaults to dry_run; apply needs \
         its exact plan_revision, covering all live PCB items and saved hierarchy bytes. \
         Independently reads back the target, every unrelated item and routing; unexpected \
         post-write state is uncertain, never assumed rolled back. Ordinary sync still \
         refuses identity conflicts. Requires the requested PCB open over native IPC.",
        json!({"type":"object", "additionalProperties":false, "properties":{
            "schematic":{"type":"string","description":"Saved root .kicad_sch"},
            "board":{"type":"string","description":"Exact .kicad_pcb open in KiCad"},
            "reference":{"type":"string","minLength":1},
            "footprint_uuid":{"type":"string","format":"uuid"},
            "old_symbol_path":{"type":"string","minLength":37,"description":"Exact slash-separated UUID path currently on this footprint; leaf UUID alone is insufficient"},
            "new_symbol_path":{"type":"string","minLength":37,"description":"Exact new exported symbol instance path, optionally including the saved root UUID"},
            "dry_run":{"type":"boolean","default":true},
            "expected_plan_revision":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"Required for apply: revision from this exact mapping's current dry_run"}
        },"required":["schematic","board","reference","footprint_uuid","old_symbol_path","new_symbol_path"]}),
        |args, ctx| async move { handle_identity_relink(args, ctx).await }
    ).with_board_access(crate::tools::BoardAccess::LiveOnly)
}

#[derive(Clone, Debug, Serialize)]
struct Mapping {
    reference: String,
    footprint_uuid: String,
    old_symbol_path: String,
    new_symbol_path: String,
}

fn uuid_path(path: &str) -> Result<()> {
    let parts = path
        .strip_prefix('/')
        .context("identity path must start with /")?;
    ensure!(!parts.is_empty(), "identity path has no symbol UUID");
    for part in parts.split('/') {
        ensure!(
            uuid::Uuid::parse_str(part)?.to_string() == part,
            "identity path requires canonical UUID segments"
        );
    }
    Ok(())
}

fn mapping(args: &Value) -> Result<Mapping> {
    let m = Mapping {
        reference: args["reference"]
            .as_str()
            .context("reference must be a string")?
            .to_string(),
        footprint_uuid: args["footprint_uuid"]
            .as_str()
            .context("footprint_uuid must be a string")?
            .to_string(),
        old_symbol_path: args["old_symbol_path"]
            .as_str()
            .context("old_symbol_path must be a string")?
            .to_string(),
        new_symbol_path: args["new_symbol_path"]
            .as_str()
            .context("new_symbol_path must be a string")?
            .to_string(),
    };
    ensure!(!m.reference.is_empty(), "reference is empty");
    ensure!(
        uuid::Uuid::parse_str(&m.footprint_uuid)?.to_string() == m.footprint_uuid,
        "footprint UUID is not canonical"
    );
    uuid_path(&m.old_symbol_path)?;
    uuid_path(&m.new_symbol_path)?;
    ensure!(
        m.old_symbol_path != m.new_symbol_path,
        "old and new identities must differ"
    );
    Ok(m)
}

#[derive(Clone)]
struct Sources(BTreeMap<PathBuf, Vec<u8>>);

impl Sources {
    fn read(schematic: &Path) -> Result<Self> {
        let paths = saved_hierarchy_files(schematic)?;
        Ok(Self(
            paths
                .into_iter()
                .map(|path| {
                    let bytes = std::fs::read(&path)?;
                    super::super::schematic_property_integrity::parse(std::str::from_utf8(
                        &bytes,
                    )?)?;
                    Ok((path, bytes))
                })
                .collect::<Result<_>>()?,
        ))
    }
    fn check(&self, schematic: &Path) -> Result<()> {
        ensure!(
            Sources::read(schematic)?.0 == self.0,
            "saved schematic hierarchy changed; rerun dry_run"
        );
        Ok(())
    }
}

struct RelinkPlan {
    next: RawBoard,
    item: prost_types::Any,
    physical_pads: usize,
}

fn plan(m: &Mapping, design: &ExportedDesign, before: &RawBoard) -> Result<RelinkPlan> {
    let old = design.relative_symbol_path(&m.old_symbol_path);
    let new = design.relative_symbol_path(&m.new_symbol_path);
    ensure!(
        old != new,
        "mapping names aliases of the same schematic identity"
    );
    let mut references = HashSet::new();
    let mut identities = HashSet::new();
    for (reference, path) in design
        .components
        .iter()
        .map(|c| (&c.reference, &c.symbol_path))
        .chain(
            design
                .skipped
                .iter()
                .map(|c| (&c.reference, &c.symbol_path)),
        )
        .chain(
            design
                .unassigned
                .iter()
                .map(|c| (&c.reference, &c.symbol_path)),
        )
    {
        let path = design.relative_symbol_path(path);
        ensure!(
            references.insert(reference) && identities.insert(path),
            "ambiguous schematic reference or identity"
        );
        ensure!(
            path != old,
            "old identity still exists in the saved schematic"
        );
    }
    let component = design.components.iter().find(|c| c.reference == m.reference && design.relative_symbol_path(&c.symbol_path) == new)
        .context("new identity does not uniquely match an on-board, footprint-assigned schematic reference")?;
    let mut target = None;
    let mut board_references = HashSet::new();
    let mut board_paths = HashSet::new();
    let mut board_ids = HashSet::new();
    for (index, item) in before.items.iter().enumerate() {
        if !konnect_ipc::builders::any_is(item, "kiapi.board.types.FootprintInstance") {
            continue;
        }
        let fp = kiapi::board::types::FootprintInstance::decode(item.value.as_slice())?;
        let id = fp
            .id
            .as_ref()
            .context("footprint has no UUID")?
            .value
            .as_str();
        ensure!(
            board_ids.insert(id.to_string()),
            "duplicate board footprint UUID"
        );
        let reference = field_text(&fp.reference_field);
        // Unrelated board-only references can be intentionally duplicated.
        if reference == m.reference {
            ensure!(
                board_references.insert(reference.clone()),
                "ambiguous target board reference"
            );
        }
        if let Some(path) = board_symbol_path(fp.symbol_path.as_ref()) {
            let relative = design.relative_symbol_path(&path);
            ensure!(
                board_paths.insert(relative.to_string()),
                "ambiguous board identity"
            );
            ensure!(
                relative != new,
                "new identity is already associated with a board footprint"
            );
        }
        if id == m.footprint_uuid {
            ensure!(
                reference == m.reference,
                "footprint UUID belongs to another reference"
            );
            ensure!(
                board_symbol_path(fp.symbol_path.as_ref()).as_deref()
                    == Some(m.old_symbol_path.as_str()),
                "old identity does not exactly match the live footprint"
            );
            target = Some((index, fp));
        }
    }
    let (index, mut fp) =
        target.context("requested footprint UUID is absent from the live board")?;
    ensure!(
        fp.encode_to_vec() == before.items[index].value,
        "target footprint contains unmodeled protobuf data"
    );
    let definition = fp
        .definition
        .as_ref()
        .context("footprint has no definition")?;
    let id = definition
        .id
        .as_ref()
        .context("footprint has no library identity")?;
    ensure!(
        format!("{}:{}", id.library_nickname, id.entry_name) == component.footprint_id,
        "assigned footprint differs; identity relink cannot replace a footprint"
    );
    let mut numbers = BTreeSet::new();
    let mut pad_ids = BTreeSet::new();
    let mut physical_pads = 0;
    for child in &definition.items {
        if !konnect_ipc::builders::any_is(child, "kiapi.board.types.Pad") {
            continue;
        }
        let pad = kiapi::board::types::Pad::decode(child.value.as_slice())?;
        ensure!(
            pad.encode_to_vec() == child.value,
            "pad contains unmodeled protobuf data"
        );
        let id = pad.id.as_ref().context("physical pad has no UUID")?;
        ensure!(
            !id.value.is_empty() && pad_ids.insert(id.value.clone()),
            "ambiguous physical pad UUID"
        );
        numbers.insert(pad.number.clone());
        let got = match pad.net.as_ref() {
            None => None,
            Some(net) if net.name.is_empty() => {
                ensure!(
                    net.code.as_ref().is_none_or(|code| code.value == 0),
                    "unnamed pad has a positive net code"
                );
                None
            }
            Some(net) => {
                let code = before
                    .nets
                    .get(&net.name)
                    .context("pad net is absent from live net table")?;
                ensure!(
                    net.code.as_ref().is_none_or(|actual| actual.value == *code),
                    "pad net name/code disagree"
                );
                Some(net.name.as_str())
            }
        };
        ensure!(
            got == component.pad_nets.get(&pad.number).map(String::as_str),
            "physical pad {} net differs from the saved schematic",
            pad.number
        );
        physical_pads += 1;
    }
    ensure!(
        physical_pads > 0
            && component
                .pad_nets
                .keys()
                .all(|number| numbers.contains(number)),
        "schematic pin has no physical pad, or footprint has zero pads"
    );
    let path = fp
        .symbol_path
        .as_mut()
        .context("footprint has no symbol path")?;
    path.path = m.new_symbol_path[1..]
        .split('/')
        .map(|value| kiapi::common::types::Kiid {
            value: value.into(),
        })
        .collect();
    let item = prost_types::Any {
        type_url: before.items[index].type_url.clone(),
        value: fp.encode_to_vec(),
    };
    let mut next = before.clone();
    next.items[index] = item.clone();
    next.items = sorted_items(next.items);
    Ok(RelinkPlan {
        next,
        item,
        physical_pads,
    })
}

fn revision(m: &Mapping, sources: &Sources, before: &RawBoard) -> String {
    let mut hash = Sha256::new();
    // Length-delimited JSON preserves byte/path boundaries and exact raw items.
    hash.update(
        serde_json::to_vec(&(
            m,
            &sources.0,
            before.document.encode_to_vec(),
            before
                .items
                .iter()
                .map(|item| (&item.type_url, &item.value))
                .collect::<Vec<_>>(),
            &before.nets,
        ))
        .expect("serializable revision"),
    );
    format!("{:x}", hash.finalize())
}

fn result(mut body: Value, target: &str, status: OutcomeStatus) -> CallToolResult {
    let complete = status == OutcomeStatus::Complete;
    body["source"] = json!({"board":"live_kicad_ipc","schematic":"saved_schematic_hierarchy"});
    let mut response = CallToolResult::json(&body);
    response.is_error = !complete;
    outcome::attach(
        response,
        outcome::summary(
            status,
            target,
            "live_kicad_ipc+saved_schematic_hierarchy",
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

fn refused(target: &str, reason: impl std::fmt::Display) -> CallToolResult {
    result(
        json!({"status":"conflict","applied":false,"identities_relinked":{"planned":0,"applied":0},"reason":reason.to_string()}),
        target,
        OutcomeStatus::Failed,
    )
}

pub(super) async fn handle_identity_relink(
    args: &Value,
    ctx: &ToolContext,
) -> Result<CallToolResult> {
    let target = args["board"].as_str().unwrap_or("unresolved board");
    let m = match mapping(args) {
        Ok(m) => m,
        Err(error) => return Ok(refused(target, error)),
    };
    let schematic = crate::tools::get_path(args, "schematic")?;
    let board = crate::tools::get_path(args, "board")?;
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let expected = args["expected_plan_revision"].as_str().map(String::from);
    if !dry_run && expected.is_none() {
        return Ok(refused(
            target,
            "apply requires expected_plan_revision from dry_run",
        ));
    }
    let sources = match Sources::read(&schematic) {
        Ok(s) => s,
        Err(e) => return Ok(refused(target, e)),
    };
    let file = tempfile::Builder::new().suffix(".net").tempfile()?;
    if let Err(e) = super::super::cli::export_netlist(
        &ctx.config.kicad_cli,
        &schematic,
        file.path(),
        "kicadsexpr",
    )
    .await
    {
        return Ok(refused(target, e));
    }
    let exported = std::fs::read_to_string(file.path())?;
    let mut design = match parse_exported_netlist(&exported) {
        Ok(d) => d,
        Err(e) => return Ok(refused(target, e)),
    };
    let paths = sources.0.keys().cloned().collect::<Vec<_>>();
    // Saved hierarchy order is significant: the first file identifies the root.
    let mut paths = paths;
    let root = match std::fs::canonicalize(&schematic) {
        Ok(root) => root,
        Err(error) => return Ok(refused(target, error)),
    };
    paths.sort_by_key(|path| path != &root);
    if let Err(e) =
        apply_saved_symbol_flags(&paths, &mut design).and_then(|()| sources.check(&schematic))
    {
        return Ok(refused(target, e));
    }
    let ipc_board = board.clone();
    let name = target.to_string();
    let response = attempt_ipc_write(ctx,&board,"identity relink",move |client| {
        let before = match raw_board(client,&ipc_board) { Ok(b)=>b,Err(e)=>return Ok(refused(&name,e)) };
        let planned = match plan(&m,&design,&before) { Ok(p)=>p,Err(e)=>return Ok(refused(&name,e)) };
        let rev = revision(&m,&sources,&before);
        if let Err(e) = sources.check(&schematic) { return Ok(refused(&name,e)); }
        if !dry_run && expected.as_deref() != Some(rev.as_str()) { return Ok(refused(&name,"stale_plan_revision: board, schematic or exact mapping changed; rerun dry_run")); }
        if dry_run {
            return Ok(result(json!({"status":"ready","dry_run":true,"applied":false,"mapping":m,"plan_revision":rev,"physical_pads_verified":planned.physical_pads,"live_items_covered":before.items.len(),"identities_relinked":{"planned":1,"applied":0},"saved_hierarchy_files":sources.0.len()}),&name,OutcomeStatus::Complete));
        }
        let mut attempted = false;
        let committed = client.run_commit("Relink equivalent schematic identity",|client| {
            sources.check(&schematic)?;
            ensure!(raw_board(client,&ipc_board)? == before,"live PCB changed immediately before identity update");
            attempted = true;
            client.update_items_in(before.document.clone(),vec![planned.item.clone()])?;
            Ok(())
        });
        let checked = committed.and_then(|()| {
            ensure!(raw_board(client,&ipc_board)? == planned.next,"independent readback differs from the exact identity-only expected board");
            sources.check(&schematic)
        });
        match checked {
            Ok(()) => Ok(result(json!({"status":"applied","applied":true,"mapping":m,"plan_revision":rev,"physical_pads_verified":planned.physical_pads,"live_items_covered":before.items.len(),"identities_relinked":{"planned":1,"applied":1},"readback_verified":true,"unrelated_items_and_routing_verified":true,"saved_hierarchy_unchanged":true,"board_saved":false}),&name,OutcomeStatus::Complete)),
            Err(e) if !attempted => Ok(refused(&name,e)),
            Err(e) => Ok(result(json!({"status":"uncertain","applied":false,"potentially_applied":true,"mapping":m,"plan_revision":rev,"identities_relinked":{"planned":1,"applied":0},"reason":format!("{e:#}"),"recovery":"Inspect the live footprint identity and unrelated PCB state before retrying; do not assume rollback or save automatically."}),&name,OutcomeStatus::Uncertain)),
        }
    }).await?;
    Ok(match response {
        BoardWrite::Ipc(result) => result,
        BoardWrite::File(reason) => refused(
            target,
            format!("{} identity relink is live-IPC-only", reason.premise()),
        ),
        BoardWrite::Refused(result) => refused(
            target,
            result
                .content
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
mod tests {
    use super::*;
    use konnect_ipc::builders::{pack_any, vec2};
    const OLD: &str = "/957827ba-fa90-4f43-a1ca-8515c9493fca/11111111-1111-4111-8111-111111111111";
    const NEW: &str = "/957827ba-fa90-4f43-a1ca-8515c9493fca/532603d2-c307-4e01-b7cf-8f2221eb0821";
    const ID: &str = "22222222-2222-4222-8222-222222222222";
    pub(super) fn fixture() -> (Mapping, ExportedDesign, RawBoard) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pcb_sync_identity/pcb_sync_identity.kicad_sch");
        let mut design = parse_exported_netlist(include_str!(
            "../../../tests/fixtures/pcb_sync_identity/pcb_sync_identity.net"
        ))
        .unwrap();
        apply_saved_symbol_flags(&saved_hierarchy_files(&root).unwrap(), &mut design).unwrap();
        let component = design
            .components
            .iter()
            .find(|c| c.reference == "R1")
            .unwrap();
        let (nickname, entry) = component.footprint_id.split_once(':').unwrap();
        let mut codes = BTreeMap::new();
        let mut pads = Vec::new();
        for (number, net) in &component.pad_nets {
            let code = codes.len() as i32 + 1;
            codes.insert(net.clone(), code);
            pads.push(pack_any(
                &kiapi::board::types::Pad {
                    id: Some(kiapi::common::types::Kiid {
                        value: uuid::Uuid::new_v4().to_string(),
                    }),
                    number: number.clone(),
                    net: Some(kiapi::board::types::Net {
                        name: net.clone(),
                        code: Some(kiapi::board::types::NetCode { value: code }),
                    }),
                    position: Some(vec2(1.0, 2.0)),
                    ..Default::default()
                },
                "kiapi.board.types.Pad",
            ));
        }
        let fp = kiapi::board::types::FootprintInstance {
            id: Some(kiapi::common::types::Kiid { value: ID.into() }),
            position: Some(vec2(10.0, 20.0)),
            orientation: Some(kiapi::common::types::Angle {
                value_degrees: 37.0,
            }),
            layer: kiapi::board::types::BoardLayer::BlBCu as i32,
            locked: kiapi::common::types::LockedState::LsLocked as i32,
            symbol_path: Some(kiapi::common::types::SheetPath {
                path: OLD[1..]
                    .split('/')
                    .map(|p| kiapi::common::types::Kiid { value: p.into() })
                    .collect(),
                path_human_readable: "/Root/".into(),
            }),
            reference_field: Some(kiapi::board::types::Field {
                name: "Reference".into(),
                text: Some(kiapi::board::types::BoardText {
                    text: Some(kiapi::common::types::Text {
                        text: "R1".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            definition: Some(kiapi::board::types::Footprint {
                id: Some(kiapi::common::types::LibraryIdentifier {
                    library_nickname: nickname.into(),
                    entry_name: entry.into(),
                }),
                items: pads,
                ..Default::default()
            }),
            ..Default::default()
        };
        let items = sorted_items(vec![
            pack_any(&fp, "kiapi.board.types.FootprintInstance"),
            prost_types::Any {
                type_url: "type.googleapis.com/kiapi.board.types.Track".into(),
                value: vec![8, 1],
            },
        ]);
        (
            Mapping {
                reference: "R1".into(),
                footprint_uuid: ID.into(),
                old_symbol_path: OLD.into(),
                new_symbol_path: NEW.into(),
            },
            design,
            RawBoard {
                document: Default::default(),
                items,
                nets: codes,
            },
        )
    }
    fn mutate_target(
        board: &mut RawBoard,
        f: impl FnOnce(&mut kiapi::board::types::FootprintInstance),
    ) {
        let item = board
            .items
            .iter_mut()
            .find(|i| konnect_ipc::builders::any_is(i, "kiapi.board.types.FootprintInstance"))
            .unwrap();
        let mut fp = kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
        f(&mut fp);
        item.value = fp.encode_to_vec();
    }
    #[test]
    fn plan_changes_only_explicit_symbol_path_and_checks_every_physical_pad() {
        let (m, d, mut b) = fixture();
        mutate_target(&mut b, |fp| {
            let p = fp.definition.as_mut().unwrap().items[0].clone();
            let mut pad = kiapi::board::types::Pad::decode(p.value.as_slice()).unwrap();
            pad.id.as_mut().unwrap().value = uuid::Uuid::new_v4().to_string();
            fp.definition
                .as_mut()
                .unwrap()
                .items
                .push(pack_any(&pad, "kiapi.board.types.Pad"));
        });
        let p = plan(&m, &d, &b).unwrap();
        assert_eq!(p.physical_pads, 3);
        let mut after =
            kiapi::board::types::FootprintInstance::decode(p.item.value.as_slice()).unwrap();
        assert_eq!(board_symbol_path(after.symbol_path.as_ref()).unwrap(), NEW);
        after.symbol_path.as_mut().unwrap().path = OLD[1..]
            .split('/')
            .map(|p| kiapi::common::types::Kiid { value: p.into() })
            .collect();
        let before = b
            .items
            .iter()
            .find(|i| konnect_ipc::builders::any_is(i, "kiapi.board.types.FootprintInstance"))
            .unwrap();
        assert_eq!(after.encode_to_vec(), before.value);
        assert_eq!(
            p.next.items.iter().find(|i| i.type_url.ends_with("Track")),
            b.items.iter().find(|i| i.type_url.ends_with("Track"))
        );
        mutate_target(&mut b, |fp| {
            let mut pad = kiapi::board::types::Pad::decode(
                fp.definition
                    .as_ref()
                    .unwrap()
                    .items
                    .last()
                    .unwrap()
                    .value
                    .as_slice(),
            )
            .unwrap();
            pad.net = fp
                .definition
                .as_ref()
                .unwrap()
                .items
                .iter()
                .filter_map(|item| kiapi::board::types::Pad::decode(item.value.as_slice()).ok())
                .find(|other| other.number != pad.number)
                .unwrap()
                .net;
            *fp.definition.as_mut().unwrap().items.last_mut().unwrap() =
                pack_any(&pad, "kiapi.board.types.Pad");
        });
        assert!(plan(&m, &d, &b).is_err());
    }
    #[test]
    fn mismatched_footprint_identity_net_and_missing_pad_all_refuse() {
        for fault in 0..7 {
            let (mut m, d, mut b) = fixture();
            match fault {
                0 => m.old_symbol_path = NEW.into(),
                1 => m.footprint_uuid = uuid::Uuid::new_v4().to_string(),
                2 => m.reference = "R2".into(),
                3 => mutate_target(&mut b, |fp| {
                    fp.definition
                        .as_mut()
                        .unwrap()
                        .id
                        .as_mut()
                        .unwrap()
                        .entry_name = "different".into()
                }),
                4 => mutate_target(&mut b, |fp| {
                    fp.definition.as_mut().unwrap().items.pop();
                }),
                5 => mutate_target(&mut b, |fp| {
                    let item = &mut fp.definition.as_mut().unwrap().items[0];
                    let mut pad = kiapi::board::types::Pad::decode(item.value.as_slice()).unwrap();
                    pad.net.as_mut().unwrap().code.as_mut().unwrap().value = 999;
                    *item = pack_any(&pad, "kiapi.board.types.Pad");
                }),
                _ => {
                    let item = b
                        .items
                        .iter_mut()
                        .find(|i| i.type_url.ends_with("FootprintInstance"))
                        .unwrap();
                    item.value.extend([0xa0, 0x06, 0x01]);
                }
            }
            assert!(plan(&m, &d, &b).is_err(), "fault {fault}");
        }
    }
    #[test]
    fn old_schematic_identity_and_ambiguous_new_or_board_identity_refuse() {
        for fault in 0..5 {
            let (m, mut d, mut b) = fixture();
            match fault {
                0 => {
                    let mut c = d.components[0].clone();
                    c.reference = "OLD1".into();
                    c.symbol_path = OLD.into();
                    d.components.push(c);
                }
                1 => d.components.push(d.components[0].clone()),
                2 => {
                    let i = b
                        .items
                        .iter()
                        .find(|i| i.type_url.ends_with("FootprintInstance"))
                        .unwrap()
                        .clone();
                    b.items.push(i);
                }
                3 => {
                    let mut other = b.clone();
                    mutate_target(&mut other, |fp| {
                        fp.id.as_mut().unwrap().value = uuid::Uuid::new_v4().to_string();
                        fp.symbol_path.as_mut().unwrap().path = NEW[1..]
                            .split('/')
                            .map(|p| kiapi::common::types::Kiid { value: p.into() })
                            .collect();
                    });
                    b.items.extend(
                        other
                            .items
                            .into_iter()
                            .filter(|i| i.type_url.ends_with("FootprintInstance")),
                    );
                }
                _ => {
                    d.components
                        .iter_mut()
                        .find(|c| c.reference == "R1")
                        .unwrap()
                        .symbol_path = "/33333333-3333-4333-8333-333333333333".into()
                }
            }
            assert!(plan(&m, &d, &b).is_err(), "ambiguity fault {fault}");
        }
    }
    #[test]
    fn revision_covers_exact_mapping_all_raw_items_nets_and_hierarchy_bytes() {
        let (mut m, _, mut b) = fixture();
        let mut sources = Sources(BTreeMap::from([(
            PathBuf::from("root.kicad_sch"),
            b"source".to_vec(),
        )]));
        let first = revision(&m, &sources, &b);
        b.items.last_mut().unwrap().value.push(0);
        assert_ne!(first, revision(&m, &sources, &b));
        b.items.last_mut().unwrap().value.pop();
        sources.0.values_mut().next().unwrap().push(0);
        assert_ne!(first, revision(&m, &sources, &b));
        sources.0.values_mut().next().unwrap().pop();
        b.nets.insert("new net".into(), 999);
        assert_ne!(first, revision(&m, &sources, &b));
        b.nets.remove("new net");
        m.new_symbol_path = OLD.into();
        assert_ne!(first, revision(&m, &sources, &b));
    }
    #[test]
    fn mapping_refuses_incomplete_aliases_and_uncertain_has_zero_confirmed() {
        let (m, _, _) = fixture();
        let args = json!({"reference":m.reference,"footprint_uuid":m.footprint_uuid,"old_symbol_path":m.old_symbol_path,"new_symbol_path":m.new_symbol_path});
        assert!(mapping(&args).is_ok());
        for field in [
            "reference",
            "footprint_uuid",
            "old_symbol_path",
            "new_symbol_path",
        ] {
            let mut invalid = args.clone();
            invalid[field] = json!("");
            assert!(mapping(&invalid).is_err());
        }
        let mut alias = args.clone();
        alias["new_symbol_path"] = alias["old_symbol_path"].clone();
        assert!(mapping(&alias).is_err());
        let res = result(
            json!({"status":"uncertain","identities_relinked":{"planned":1,"applied":0}}),
            "board",
            OutcomeStatus::Uncertain,
        );
        assert!(res.is_error);
        let body: Value = serde_json::from_str(match &res.content[0] {
            ToolContent::Text { text } => text,
            _ => panic!(),
        })
        .unwrap();
        assert_eq!(body["identities_relinked"]["applied"], 0);
        assert_eq!(body["outcome"]["completed"], 0);
        assert_eq!(body["outcome"]["retry"]["safe"], false);
    }

    /// Opens no editor: requires a separately launched disposable FC project copy.
    #[tokio::test]
    #[ignore = "requires isolated FC project, real KiCad CLI and frame-specific IPC"]
    async fn live_identity_relink_is_exactly_one_path_change() {
        let board = PathBuf::from(std::env::var("KONNECT_LIVE_LIBRARY_COPY").unwrap());
        let socket = std::env::var("KICAD_API_SOCKET").unwrap();
        assert!(socket.contains("api-") && socket.ends_with(".sock"));
        let cli = std::env::var("KICAD_CLI").unwrap();
        let config = crate::tools::ServerConfig {
            kicad_cli: cli,
            kicad_binary: String::new(),
            ipc_address: socket.clone(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        };
        let ctx = ToolContext::new(
            config.clone(),
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        );
        let client = konnect_ipc::KiCadIpcClient::new(&socket);
        let before = raw_board(&client, &board).unwrap();
        let target = before
            .items
            .iter()
            .filter(|i| konnect_ipc::builders::any_is(i, "kiapi.board.types.FootprintInstance"))
            .map(|i| kiapi::board::types::FootprintInstance::decode(i.value.as_slice()).unwrap())
            .find(|fp| field_text(&fp.reference_field) == "U2")
            .unwrap();
        let old = board_symbol_path(target.symbol_path.as_ref()).unwrap();
        let new = old.replace(
            "b168ec8d-8992-4c6e-92a7-35bb242dfd57",
            "90af6e9e-ef0e-4612-a503-88ea8611cc85",
        );
        assert_ne!(old, new);
        let args = json!({"board":board,"schematic":board.with_extension("kicad_sch"),"reference":"U2","footprint_uuid":target.id.as_ref().unwrap().value,"old_symbol_path":old,"new_symbol_path":new,"dry_run":true});
        let decode = |r: CallToolResult| {
            assert!(!r.is_error, "{r:?}");
            serde_json::from_str::<Value>(match &r.content[0] {
                ToolContent::Text { text } => text,
                _ => panic!(),
            })
            .unwrap()
        };
        let dry = decode(handle_identity_relink(&args, &ctx).await.unwrap());
        assert_eq!(dry["status"], "ready");
        assert_eq!(dry["physical_pads_verified"], 14);
        let mut stale = args.clone();
        stale["dry_run"] = json!(false);
        stale["expected_plan_revision"] = json!("0".repeat(64));
        let refused = handle_identity_relink(&stale, &ctx).await.unwrap();
        assert!(refused.is_error);
        assert_eq!(raw_board(&client, &board).unwrap(), before);
        let mut apply = args.clone();
        apply["dry_run"] = json!(false);
        apply["expected_plan_revision"] = dry["plan_revision"].clone();
        let applied = decode(handle_identity_relink(&apply, &ctx).await.unwrap());
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["identities_relinked"]["applied"], 1);
        let after = raw_board(&client, &board).unwrap();
        let mut expected = before.clone();
        for item in &mut expected.items {
            if !konnect_ipc::builders::any_is(item, "kiapi.board.types.FootprintInstance") {
                continue;
            }
            let mut fp =
                kiapi::board::types::FootprintInstance::decode(item.value.as_slice()).unwrap();
            if fp.id != target.id {
                continue;
            }
            fp.symbol_path.as_mut().unwrap().path = new[1..]
                .split('/')
                .map(|p| kiapi::common::types::Kiid { value: p.into() })
                .collect();
            item.value = fp.encode_to_vec();
        }
        expected.items = sorted_items(expected.items);
        assert_eq!(after, expected);
        let repeated = handle_identity_relink(&apply, &ctx).await.unwrap();
        assert!(repeated.is_error);
        assert_eq!(raw_board(&client, &board).unwrap(), after);
        if let Ok(output) = std::env::var("KONNECT_LIVE_IDENTITY_EVIDENCE") {
            let output = PathBuf::from(output);
            std::fs::create_dir_all(&output).unwrap();
            for (label, state) in [("before", before), ("after", after)] {
                std::fs::write(output.join(format!("{label}.json")),serde_json::to_vec_pretty(&json!({"items":state.items.iter().map(|i|(&i.type_url,&i.value)).collect::<Vec<_>>(),"nets":state.nets})).unwrap()).unwrap();
            }
            std::fs::write(output.join("responses.json"),serde_json::to_vec_pretty(&json!({"request":args,"dry":dry,"apply":applied,"stale_refused":true,"repeat_refused":true,"exactly_one_path_changed":true})).unwrap()).unwrap();
        }
    }
}

#[cfg(test)]
mod served_tests;
