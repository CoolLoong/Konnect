use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

const ID1: &str = "11111111-1111-4111-8111-111111111111";
const ID2: &str = "22222222-2222-4222-8222-222222222222";
fn fixture() -> (Vec<Change>, RawBoard) {
    let make_field = |name: &str, value: &str| kiapi::board::types::Field {
        name: name.into(),
        visible: true,
        text: Some(builders::board_text_with_stroke_width(
            "F.SilkS", value, 15.9, 8.2, 1.0, 0.15, 0.0, false,
        )),
        ..Default::default()
    };
    let make_fp = |id: &str, reference: &str| kiapi::board::types::FootprintInstance {
        id: Some(kiapi::common::types::Kiid { value: id.into() }),
        position: Some(builders::vec2(10.0, 10.0)),
        reference_field: Some(make_field("Reference", reference)),
        value_field: Some(make_field("Value", "unchanged")),
        definition: Some(kiapi::board::types::Footprint {
            items: vec![prost_types::Any {
                type_url: "opaque-pad".into(),
                value: vec![7, 8, 9],
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    let changes=serde_json::from_value(json!([
        {"reference":"J1","footprint_uuid":ID1,"field":"Reference","position":{"x":8.2,"y":-15.9},"angle_deg":-90,"size":{"x":0.7,"y":0.8},"visible":false},
        {"reference":"U1","footprint_uuid":ID2,"field":"Value","visible":false}
    ])).unwrap();
    let before = RawBoard {
        document: Default::default(),
        nets: BTreeMap::new(),
        items: sorted_items(vec![
            builders::pack_any(&make_fp(ID1, "J1"), "kiapi.board.types.FootprintInstance"),
            builders::pack_any(&make_fp(ID2, "U1"), "kiapi.board.types.FootprintInstance"),
            builders::pack_any(
                &builders::build_track("NET", 1, "F.Cu", 0.25, 0.0, 0.0, 1.0, 1.0),
                "kiapi.board.types.Track",
            ),
        ]),
    };
    (changes, before)
}
fn body(result: &CallToolResult) -> Value {
    serde_json::from_str(match &result.content[0] {
        ToolContent::Text { text } => text,
        _ => panic!("text"),
    })
    .unwrap()
}

#[test]
fn field_plan_preserves_identity_pose_pads_graphics_strings_and_unrequested_layout() {
    let (changes, before) = fixture();
    let plan = plan(&changes, &before).unwrap();
    assert_eq!(plan.changed, 2);
    assert_eq!(plan.updates.len(), 2);
    for update in &plan.updates {
        let mut fp =
            kiapi::board::types::FootprintInstance::decode(update.value.as_slice()).unwrap();
        let original = before
            .items
            .iter()
            .find(|item| {
                builders::any_is(item, "kiapi.board.types.FootprintInstance")
                    && kiapi::board::types::FootprintInstance::decode(item.value.as_slice())
                        .unwrap()
                        .id
                        == fp.id
            })
            .unwrap();
        let old =
            kiapi::board::types::FootprintInstance::decode(original.value.as_slice()).unwrap();
        if fp.id.as_ref().unwrap().value == ID1 {
            let f = fp.reference_field.as_ref().unwrap();
            let text = f.text.as_ref().unwrap().text.as_ref().unwrap();
            assert_eq!(text.position.as_ref().unwrap().x_nm, 8_200_000);
            assert_eq!(text.position.as_ref().unwrap().y_nm, -15_900_000);
            assert_eq!(
                text.attributes
                    .as_ref()
                    .unwrap()
                    .angle
                    .as_ref()
                    .unwrap()
                    .value_degrees,
                270.0
            );
            assert_eq!(text.text, "J1");
            assert!(!f.visible);
            assert_eq!(fp.value_field, old.value_field);
            fp.reference_field = old.reference_field;
        } else {
            assert_eq!(fp.reference_field, old.reference_field);
            assert_eq!(text(&fp.value_field), Some("unchanged"));
            fp.value_field = old.value_field;
        }
        assert_eq!(
            fp.encode_to_vec(),
            original.value,
            "only the selected layout may change"
        );
    }
    assert_eq!(before.nets, plan.next.nets);
    assert_eq!(before.document, plan.next.document);
    let tracks = |b: &RawBoard| {
        b.items
            .iter()
            .filter(|i| i.type_url.ends_with("Track"))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(tracks(&before), tracks(&plan.next));
    assert_ne!(revision(&changes, &before), revision(&changes, &plan.next));
}

#[test]
fn field_plan_refuses_ambiguous_targets_lossy_protobuf_and_invalid_layout() {
    let (changes, before) = fixture();
    let mut wrong = changes.clone();
    wrong[0].reference = "WRONG".into();
    assert!(plan(&wrong, &before).is_err());
    let mut missing = changes.clone();
    missing[0].footprint_uuid = "33333333-3333-4333-8333-333333333333".into();
    assert!(plan(&missing, &before).is_err());
    let mut unknown = before.clone();
    unknown
        .items
        .iter_mut()
        .find(|i| builders::any_is(i, "kiapi.board.types.FootprintInstance"))
        .unwrap()
        .value
        .extend([0xa0, 0x06, 0x01]);
    assert!(plan(&changes, &unknown).is_err());
    let mut duplicate = before.clone();
    duplicate.items.push(
        duplicate
            .items
            .iter()
            .find(|i| builders::any_is(i, "kiapi.board.types.FootprintInstance"))
            .unwrap()
            .clone(),
    );
    assert!(plan(&changes, &duplicate).is_err());
    for mutate in 0..4 {
        let mut edits = changes.clone();
        match mutate {
            0 => edits.push(edits[0].clone()),
            1 => edits[0].position.as_mut().unwrap().x = f64::INFINITY,
            2 => edits[0].size.as_mut().unwrap().y = 0.0000001,
            _ => edits[0].angle_deg = Some(f64::NAN),
        }
        assert!(validate(&Request {
            board: "board".into(),
            changes: edits,
            dry_run: true,
            expected_plan_revision: None
        })
        .is_err());
    }
    let mut track_change = before.clone();
    track_change
        .items
        .iter_mut()
        .find(|i| i.type_url.ends_with("Track"))
        .unwrap()
        .value
        .push(0);
    assert_ne!(
        revision(&changes, &before),
        revision(&changes, &track_change)
    );
}

#[tokio::test]
async fn served_batch_rejects_stale_and_reports_dropped_collateral_or_partial_writes_uncertain() {
    for fault in 0..4 {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        std::fs::write(&board, "existence marker").unwrap();
        let (changes, initial) = fixture();
        let state = Arc::new(Mutex::new(initial.items));
        let observed = state.clone();
        let writes = Arc::new(AtomicUsize::new(0));
        let updates = writes.clone();
        let commits = Arc::new(AtomicUsize::new(0));
        let count_commits = commits.clone();
        let server = crate::tools::pcb_board::board_mock::spawn_kicad_holding_board(
            &board,
            move |command| {
                if command.type_url.ends_with("GetItems") {
                    return Some(builders::pack_any(
                        &kiapi::common::commands::GetItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            items: observed.lock().unwrap().clone(),
                        },
                        "kiapi.common.commands.GetItemsResponse",
                    ));
                }
                if command.type_url.ends_with("GetNets") {
                    return Some(builders::pack_any(
                        &kiapi::board::commands::NetsResponse { nets: vec![] },
                        "kiapi.board.commands.NetsResponse",
                    ));
                }
                if command.type_url.ends_with("BeginCommit") {
                    count_commits.fetch_add(1, Ordering::SeqCst);
                    return Some(builders::pack_any(
                        &kiapi::common::commands::BeginCommitResponse {
                            id: Some(kiapi::common::types::Kiid {
                                value: "fields-commit".into(),
                            }),
                        },
                        "kiapi.common.commands.BeginCommitResponse",
                    ));
                }
                if command.type_url.ends_with("EndCommit") {
                    return Some(builders::pack_any(
                        &kiapi::common::commands::EndCommitResponse {},
                        "kiapi.common.commands.EndCommitResponse",
                    ));
                }
                if command.type_url.ends_with("UpdateItems") {
                    updates.fetch_add(1, Ordering::SeqCst);
                    let request =
                        kiapi::common::commands::UpdateItems::decode(command.value.as_slice())
                            .unwrap();
                    assert_eq!(request.items.len(), 2);
                    let mut items = observed.lock().unwrap();
                    for (number, wanted) in request.items.iter().enumerate() {
                        if fault == 3 && number == 1 {
                            continue;
                        }
                        let mut fp =
                            kiapi::board::types::FootprintInstance::decode(wanted.value.as_slice())
                                .unwrap();
                        let index = items
                            .iter()
                            .position(|i| {
                                builders::any_is(i, "kiapi.board.types.FootprintInstance")
                                    && kiapi::board::types::FootprintInstance::decode(
                                        i.value.as_slice(),
                                    )
                                    .unwrap()
                                    .id == fp.id
                            })
                            .unwrap();
                        if fault == 1 {
                            fp.reference_field.as_mut().unwrap().visible = true;
                            fp.value_field.as_mut().unwrap().visible = true;
                        }
                        items[index] =
                            builders::pack_any(&fp, "kiapi.board.types.FootprintInstance");
                    }
                    if fault == 2 {
                        items
                            .iter_mut()
                            .find(|i| i.type_url.ends_with("Track"))
                            .unwrap()
                            .value
                            .push(0);
                    }
                    return Some(builders::pack_any(
                        &kiapi::common::commands::UpdateItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            updated_items: request
                                .items
                                .into_iter()
                                .map(|item| kiapi::common::commands::ItemUpdateResult {
                                    status: Some(kiapi::common::commands::ItemStatus {
                                        code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                                        error_message: String::new(),
                                    }),
                                    item: Some(item),
                                })
                                .collect(),
                        },
                        "kiapi.common.commands.UpdateItemsResponse",
                    ));
                }
                None
            },
        );
        let ctx = ToolContext::new(
            crate::tools::ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address: server.address().to_string(),
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(crate::router::ToolRouter::new()),
        );
        let args = json!({"board":board,"changes":changes});
        let dry = handle_field_layout(&args, &ctx).await.unwrap();
        assert!(!dry.is_error, "{}", body(&dry));
        let preview = body(&dry);
        assert_eq!(preview["fields_edited"]["planned"], 2);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        let mut apply = args.clone();
        apply["dry_run"] = json!(false);
        apply["expected_plan_revision"] = json!("0".repeat(64));
        assert!(handle_field_layout(&apply, &ctx).await.unwrap().is_error);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        apply["expected_plan_revision"] = preview["plan_revision"].clone();
        let result = handle_field_layout(&apply, &ctx).await.unwrap();
        let actual = body(&result);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        if fault == 0 {
            assert!(!result.is_error, "{actual}");
            assert_eq!(actual["fields_edited"]["applied"], 2);
            assert_eq!(actual["readback_verified"], true);
        } else {
            assert!(result.is_error);
            assert_eq!(actual["status"], "uncertain");
            assert_eq!(actual["fields_edited"]["applied"], 0);
            assert_eq!(actual["outcome"]["retry"]["safe"], false);
        }
        assert_eq!(std::fs::read_to_string(&board).unwrap(), "existence marker");
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated real KiCad editor"]
async fn live_field_layout_preserves_all_raw_items_and_physical_pads() {
    let board = std::path::PathBuf::from(
        std::env::var("KONNECT_LIVE_FIELD_BOARD").expect("isolated board"),
    );
    let socket = std::env::var("KONNECT_LIVE_FIELD_IPC").expect("isolated IPC");
    assert!(board
        .to_string_lossy()
        .contains("/work/identity-relink-acceptance/project/"));
    assert_eq!(socket, "ipc:///tmp/kicad/api-35808.sock");
    let client = konnect_ipc::KiCadIpcClient::new(&socket);
    let before = raw_board(&client, &board).unwrap();
    let mut changes = Vec::new();
    for (reference, field_name) in [("J2", "Reference"), ("U2", "Value")] {
        let fp = before
            .items
            .iter()
            .filter(|i| builders::any_is(i, "kiapi.board.types.FootprintInstance"))
            .map(|i| kiapi::board::types::FootprintInstance::decode(i.value.as_slice()).unwrap())
            .find(|fp| text(&fp.reference_field) == Some(reference))
            .unwrap();
        let field = if field_name == "Reference" {
            fp.reference_field.as_ref().unwrap()
        } else {
            fp.value_field.as_ref().unwrap()
        };
        let pos = field
            .text
            .as_ref()
            .unwrap()
            .text
            .as_ref()
            .unwrap()
            .position
            .as_ref()
            .unwrap();
        changes.push(json!({"reference":reference,"footprint_uuid":fp.id.as_ref().unwrap().value,"field":field_name,"position":{"x":builders::nm_to_mm(pos.x_nm)+1.0,"y":8.2},"angle_deg":0.0,"size":{"x":0.65,"y":0.70},"visible":!field.visible}));
    }
    let ctx = ToolContext::new(
        crate::tools::ServerConfig {
            kicad_cli: String::new(),
            kicad_binary: String::new(),
            ipc_address: socket,
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        },
        Arc::new(crate::router::ToolRouter::new()),
    );
    let args = json!({"board":board,"changes":changes});
    let dry = handle_field_layout(&args, &ctx).await.unwrap();
    assert!(!dry.is_error, "{}", body(&dry));
    let preview = body(&dry);
    assert_eq!(preview["fields_edited"]["planned"], 2);
    assert_eq!(raw_board(&client, &board).unwrap(), before);
    let mut apply = args.clone();
    apply["dry_run"] = json!(false);
    apply["expected_plan_revision"] = json!("0".repeat(64));
    assert!(handle_field_layout(&apply, &ctx).await.unwrap().is_error);
    assert_eq!(raw_board(&client, &board).unwrap(), before);
    apply["expected_plan_revision"] = preview["plan_revision"].clone();
    let result = handle_field_layout(&apply, &ctx).await.unwrap();
    let actual = body(&result);
    assert!(!result.is_error, "{actual}");
    let after = raw_board(&client, &board).unwrap();
    let request = parse_request(&args).unwrap();
    assert_eq!(after, plan(&request.changes, &before).unwrap().next);
    if let Ok(output) = std::env::var("KONNECT_LIVE_FIELD_EVIDENCE") {
        let out = std::path::PathBuf::from(output);
        std::fs::create_dir_all(&out).unwrap();
        for (name, state) in [("before", &before), ("after", &after)] {
            std::fs::write(out.join(format!("{name}.json")),serde_json::to_vec_pretty(&json!({"items":state.items.iter().map(|a|(&a.type_url,&a.value)).collect::<Vec<_>>(),"nets":state.nets})).unwrap()).unwrap();
        }
        std::fs::write(out.join("responses.json"),serde_json::to_vec_pretty(&json!({"request":args,"dry":preview,"apply":actual,"exact_complete_readback":true})).unwrap()).unwrap();
    }
}
