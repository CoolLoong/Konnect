use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
const ID1: &str = "11111111-1111-4111-8111-111111111111";
const ID2: &str = "22222222-2222-4222-8222-222222222222";
fn fixture() -> (Vec<Change>, RawBoard) {
    let make = |id: &str, reference: &str| kiapi::board::types::FootprintInstance {
        id: Some(kiapi::common::types::Kiid { value: id.into() }),
        position: Some(builders::vec2(10.0, 8.2)),
        orientation: Some(kiapi::common::types::Angle {
            value_degrees: 37.0,
        }),
        layer: builders::layer_from_name("F.Cu") as i32,
        reference_field: Some(kiapi::board::types::Field {
            name: "Reference".into(),
            visible: true,
            text: Some(builders::board_text_with_stroke_width(
                "F.SilkS", reference, 10.0, 9.7, 1.0, 0.15, 37.0, false,
            )),
            ..Default::default()
        }),
        definition: Some(kiapi::board::types::Footprint {
            items: vec![
                builders::pack_any(
                    &kiapi::board::types::Pad {
                        id: Some(kiapi::common::types::Kiid {
                            value: format!("{}3", &id[..35]),
                        }),
                        number: "1".into(),
                        net: Some(kiapi::board::types::Net {
                            name: "NET".into(),
                            code: Some(kiapi::board::types::NetCode { value: 1 }),
                        }),
                        position: Some(builders::vec2(1.2, 0.7)),
                        ..Default::default()
                    },
                    "kiapi.board.types.Pad",
                ),
                builders::pack_any(
                    &kiapi::board::types::Footprint3DModel {
                        filename: "${KICAD10_3DMODEL_DIR}/asymmetric.step".into(),
                        scale: Some(kiapi::common::types::Vector3D {
                            x_nm: 1000000.0,
                            y_nm: 1000000.0,
                            z_nm: 1000000.0,
                        }),
                        rotation: Some(kiapi::common::types::Vector3D {
                            x_nm: 23000000.0,
                            y_nm: 14000000.0,
                            z_nm: 37000000.0,
                        }),
                        offset: Some(kiapi::common::types::Vector3D {
                            x_nm: 1100000.0,
                            y_nm: 2300000.0,
                            z_nm: 500000.0,
                        }),
                        visible: true,
                        opacity: 1.0,
                    },
                    "kiapi.board.types.Footprint3DModel",
                ),
            ],
            ..Default::default()
        }),
        ..Default::default()
    };
    let changes=serde_json::from_value(json!([{"reference":"J1","footprint_uuid":ID1,"layer":"B.Cu"},{"reference":"U1","footprint_uuid":ID2,"layer":"B.Cu"}])).unwrap();
    let (changes, mut board) = (
        changes,
        RawBoard {
            document: Default::default(),
            nets: BTreeMap::new(),
            items: sorted_items(vec![
                builders::pack_any(&make(ID1, "J1"), "kiapi.board.types.FootprintInstance"),
                builders::pack_any(&make(ID2, "U1"), "kiapi.board.types.FootprintInstance"),
                builders::pack_any(
                    &builders::build_track("NET", 1, "F.Cu", 0.25, 0.0, 0.0, 1.0, 1.0),
                    "kiapi.board.types.Track",
                ),
            ]),
        },
    );
    let children = board
        .items
        .iter()
        .filter(|a| builders::any_is(a, "kiapi.board.types.FootprintInstance"))
        .flat_map(|a| {
            kiapi::board::types::FootprintInstance::decode(a.value.as_slice())
                .unwrap()
                .definition
                .unwrap()
                .items
        })
        .filter(|a| builders::any_is(a, "kiapi.board.types.Pad"))
        .collect::<Vec<_>>();
    board.items.extend(children);
    board.items = sorted_items(board.items);
    (changes, board)
}
// Model the server payload, never used to compute production mirror geometry.
fn flipped(a: &prost_types::Any) -> prost_types::Any {
    let mut fp = kiapi::board::types::FootprintInstance::decode(a.value.as_slice()).unwrap();
    fp.layer = builders::layer_from_name("B.Cu") as i32;
    fp.orientation.as_mut().unwrap().value_degrees = 323.0;
    let field = fp.reference_field.as_mut().unwrap().text.as_mut().unwrap();
    field.layer = builders::layer_from_name("B.SilkS") as i32;
    field.text.as_mut().unwrap().position.as_mut().unwrap().y_nm = 6_700_000;
    let a = &mut fp.definition.as_mut().unwrap().items[0];
    let mut pad = kiapi::board::types::Pad::decode(a.value.as_slice()).unwrap();
    pad.position.as_mut().unwrap().y_nm = -700_000;
    *a = builders::pack_any(&pad, "kiapi.board.types.Pad");
    builders::pack_any(&fp, "kiapi.board.types.FootprintInstance")
}
fn body(r: &CallToolResult) -> Value {
    serde_json::from_str(match &r.content[0] {
        ToolContent::Text { text } => text,
        _ => panic!("text"),
    })
    .unwrap()
}
#[test]
fn preview_binds_exact_uuid_reference_sides_and_all_copper_and_preserves_noops() {
    let (changes, b) = fixture();
    let p = plan(&changes, &b).unwrap();
    assert_eq!(p.ids.len(), 2);
    let native = p
        .ids
        .iter()
        .map(|id| flipped(&b.items[p.targets[id].0]))
        .collect();
    let after = expected(&b, &p, native).unwrap();
    assert_eq!(plan(&changes, &after).unwrap().ids.len(), 0);
    assert_ne!(revision(&changes, &b), revision(&changes, &after));
    let mut copper = b.clone();
    copper
        .items
        .iter_mut()
        .find(|a| a.type_url.ends_with("Track"))
        .unwrap()
        .value
        .push(0);
    assert_ne!(revision(&changes, &b), revision(&changes, &copper));
    let mut bad = changes.clone();
    bad[0].reference = "wrong".into();
    assert!(plan(&bad, &b).is_err());
    bad = changes.clone();
    bad[0].layer = "F.Cu".into();
    assert_eq!(plan(&bad, &b).unwrap().ids.len(), 1);
    assert_ne!(revision(&changes, &b), revision(&bad, &b));
    let mut duplicate = b.clone();
    duplicate.items.push(b.items[0].clone());
    assert!(plan(&changes, &duplicate).is_err());
    assert!(request(&json!({"board":"b","changes":changes,"dry_run":false})).is_err());
    let mut args = json!({"board":"b","changes":changes});
    args["changes"][1] = args["changes"][0].clone();
    assert!(request(&args).is_err());
    args["changes"][0]["layer"] = json!("In1.Cu");
    assert!(request(&args).is_err());
}
#[test]
fn native_results_must_preserve_full_target_identity_and_pad_nets() {
    let (changes, b) = fixture();
    let p = plan(&changes, &b).unwrap();
    let native = p
        .ids
        .iter()
        .map(|id| flipped(&b.items[p.targets[id].0]))
        .collect::<Vec<_>>();
    assert!(expected(&b, &p, native.clone()).is_ok());
    for fault in 0..5 {
        let mut items = native.clone();
        let mut fp =
            kiapi::board::types::FootprintInstance::decode(items[0].value.as_slice()).unwrap();
        match fault {
            0 => fp.position.as_mut().unwrap().x_nm += 1,
            1 => fp.reference_field.as_mut().unwrap().visible = false,
            2 => {
                let a = &mut fp.definition.as_mut().unwrap().items[0];
                let mut pad = kiapi::board::types::Pad::decode(a.value.as_slice()).unwrap();
                pad.net.as_mut().unwrap().code.as_mut().unwrap().value = 2;
                *a = builders::pack_any(&pad, "kiapi.board.types.Pad");
            }
            3 => fp
                .definition
                .as_mut()
                .unwrap()
                .items
                .pop()
                .map(|_| ())
                .unwrap(),
            _ => fp.layer = builders::layer_from_name("F.Cu") as i32,
        }
        items[0] = builders::pack_any(&fp, "kiapi.board.types.FootprintInstance");
        assert!(expected(&b, &p, items).is_err());
    }
    assert!(expected(&b, &p, vec![]).is_err());
    assert!(expected(&b, &p, vec![native[0].clone(), native[0].clone()]).is_err());
}
#[tokio::test]
async fn served_native_batch_is_one_commit_and_refuses_stale_or_uncertain_readbacks() {
    for fault in 0..7 {
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        std::fs::write(&board, "unchanged saved board").unwrap();
        let (changes, b) = fixture();
        let state = Arc::new(Mutex::new(b.items));
        let observed = state.clone();
        let flips = Arc::new(AtomicUsize::new(0));
        let writes = flips.clone();
        let commits = Arc::new(AtomicUsize::new(0));
        let begins = commits.clone();
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
                    begins.fetch_add(1, Ordering::SeqCst);
                    return Some(builders::pack_any(
                        &kiapi::common::commands::BeginCommitResponse {
                            id: Some(kiapi::common::types::Kiid {
                                value: "flip-commit".into(),
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
                if command.type_url.ends_with("FlipItems") {
                    writes.fetch_add(1, Ordering::SeqCst);
                    let cmd = kiapi::board::commands::FlipItems::decode(command.value.as_slice())
                        .unwrap();
                    assert_eq!(cmd.items.len(), 2);
                    assert_eq!(
                        cmd.direction,
                        kiapi::board::commands::BoardFlipDirection::BfdTopBottom as i32
                    );
                    let mut all = observed.lock().unwrap();
                    let mut results = Vec::new();
                    for (n, id) in cmd.items.iter().enumerate() {
                        let i = all
                            .iter()
                            .position(|a| {
                                builders::any_is(a, "kiapi.board.types.FootprintInstance")
                                    && kiapi::board::types::FootprintInstance::decode(
                                        a.value.as_slice(),
                                    )
                                    .unwrap()
                                    .id
                                    .as_ref()
                                        == Some(id)
                            })
                            .unwrap();
                        let returned = flipped(&all[i]);
                        if !(fault == 1 || (fault == 2 && n == 1)) {
                            all[i] = returned.clone();
                            if fault != 6 {
                                let child = kiapi::board::types::FootprintInstance::decode(
                                    returned.value.as_slice(),
                                )
                                .unwrap()
                                .definition
                                .unwrap()
                                .items
                                .remove(0);
                                let pad_id =
                                    kiapi::board::types::Pad::decode(child.value.as_slice())
                                        .unwrap()
                                        .id;
                                let j = all
                                    .iter()
                                    .position(|a| {
                                        builders::any_is(a, "kiapi.board.types.Pad")
                                            && kiapi::board::types::Pad::decode(a.value.as_slice())
                                                .unwrap()
                                                .id
                                                == pad_id
                                    })
                                    .unwrap();
                                all[j] = child;
                            }
                        }
                        let mut result = kiapi::board::commands::ItemFlipResult {
                            status: Some(kiapi::common::commands::ItemStatus {
                                code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                                error_message: String::new(),
                            }),
                            item: Some(returned),
                        };
                        if fault == 4 {
                            result.status.as_mut().unwrap().code =
                                kiapi::common::commands::ItemStatusCode::IscNonexistent as i32;
                        }
                        results.push(result);
                    }
                    if fault == 3 {
                        all.iter_mut()
                            .find(|a| a.type_url.ends_with("Track"))
                            .unwrap()
                            .value
                            .push(0);
                    }
                    if fault == 5 {
                        results.clear();
                    }
                    return Some(builders::pack_any(
                        &kiapi::board::commands::FlipItemsResponse {
                            header: cmd.header,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            flipped_items: results,
                        },
                        "kiapi.board.commands.FlipItemsResponse",
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
        let dry = handle_native_flip(&args, &ctx).await.unwrap();
        assert!(!dry.is_error, "{}", body(&dry));
        let preview = body(&dry);
        assert_eq!(preview["footprints_flipped"]["planned"], 2);
        assert_eq!(flips.load(Ordering::SeqCst), 0);
        let mut apply = args.clone();
        apply["dry_run"] = json!(false);
        apply["expected_plan_revision"] = json!("0".repeat(64));
        assert!(handle_native_flip(&apply, &ctx).await.unwrap().is_error);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        apply["expected_plan_revision"] = preview["plan_revision"].clone();
        let actual = handle_native_flip(&apply, &ctx).await.unwrap();
        let v = body(&actual);
        assert_eq!(flips.load(Ordering::SeqCst), 1);
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        if fault == 0 {
            assert!(!actual.is_error, "{v}");
            assert_eq!(v["footprints_flipped"]["applied"], 2);
            let dry = body(&handle_native_flip(&args, &ctx).await.unwrap());
            apply["expected_plan_revision"] = dry["plan_revision"].clone();
            let noop = body(&handle_native_flip(&apply, &ctx).await.unwrap());
            assert_eq!(noop["status"], "noop");
            assert_eq!(commits.load(Ordering::SeqCst), 1);
        } else {
            assert!(actual.is_error);
            assert_eq!(v["status"], "uncertain");
            assert_eq!(v["footprints_flipped"]["applied"], 0);
            assert_eq!(v["outcome"]["retry"]["safe"], false);
        }
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            "unchanged saved board"
        );
    }
}
#[tokio::test]
#[ignore = "requires owned isolated KiCad editor"]
async fn live_native_flip_roundtrip_preserves_every_raw_item_and_pad_identity() {
    let board = std::path::PathBuf::from(std::env::var("KONNECT_FLIP_BOARD").unwrap());
    let socket = std::env::var("KONNECT_FLIP_IPC").unwrap();
    assert!(board
        .to_string_lossy()
        .contains("/work/identity-relink-acceptance/project/"));
    assert_eq!(socket, "ipc:///tmp/kicad/api-35808.sock");
    let client = konnect_ipc::KiCadIpcClient::new(&socket);
    let before = raw_board(&client, &board).unwrap();
    let saved = std::fs::read(&board).unwrap();
    let all = footprints(&before).unwrap();
    let changes = ["J2", "U2"]
        .iter()
        .map(|reference| {
            let (id, (_, fp)) = all
                .iter()
                .find(|(_, (_, fp))| text(&fp.reference_field) == Some(reference))
                .unwrap();
            assert!([builders::layer_from_name("F.Cu") as i32,builders::layer_from_name("B.Cu") as i32].contains(&fp.layer));
            json!({"reference":reference,"footprint_uuid":id,"layer":if fp.layer==builders::layer_from_name("F.Cu") as i32 {"B.Cu"}else{"F.Cu"}})
        })
        .collect::<Vec<_>>();
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
    let dry = body(&handle_native_flip(&args, &ctx).await.unwrap());
    assert_eq!(dry["footprints_flipped"]["planned"], 2, "{dry}");
    assert_eq!(before, raw_board(&client, &board).unwrap());
    let mut apply = args.clone();
    apply["dry_run"] = json!(false);
    apply["expected_plan_revision"] = dry["plan_revision"].clone();
    let result = handle_native_flip(&apply, &ctx).await.unwrap();
    let after = raw_board(&client, &board).unwrap();
    let actual = body(&result);
    if result.is_error {
        if let Ok(out) = std::env::var("KONNECT_FLIP_EVIDENCE") {
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(
                std::path::Path::new(&out).join("failed.json"),
                serde_json::to_vec_pretty(&actual).unwrap(),
            )
            .unwrap();
        }
        panic!("{actual}");
    }
    let mut back = args.clone();
    for c in back["changes"].as_array_mut().unwrap() {
        c["layer"] = json!(if c["layer"] == "F.Cu" { "B.Cu" } else { "F.Cu" });
    }
    let back_result = handle_native_flip(&back, &ctx).await.unwrap();
    let back_dry = body(&back_result);
    assert!(!back_result.is_error, "{back_dry}");
    back["dry_run"] = json!(false);
    back["expected_plan_revision"] = back_dry["plan_revision"].clone();
    let returned = handle_native_flip(&back, &ctx).await.unwrap();
    assert!(!returned.is_error, "{}", body(&returned));
    let roundtrip = raw_board(&client, &board).unwrap();
    // Compare native roundtrip, including 3D models, all child geometry and routing.
    assert_eq!(
        roundtrip, before,
        "native flip roundtrip altered complete raw board"
    );
    assert_eq!(std::fs::read(&board).unwrap(), saved);
    if let Ok(out) = std::env::var("KONNECT_FLIP_EVIDENCE") {
        let out = std::path::PathBuf::from(out);
        std::fs::create_dir_all(&out).unwrap();
        for (name, b) in [
            ("before", before),
            ("back_side", after),
            ("roundtrip", roundtrip),
        ] {
            std::fs::write(out.join(format!("{name}.json")),serde_json::to_vec_pretty(&json!({"items":b.items.iter().map(|a|(&a.type_url,&a.value)).collect::<Vec<_>>(),"nets":b.nets})).unwrap()).unwrap();
        }
        std::fs::write(out.join("responses.json"),serde_json::to_vec_pretty(&json!({"request":args,"preview":dry,"apply":actual,"return":body(&returned),"roundtrip_full_raw_equal":true,"no_auto_save":true})).unwrap()).unwrap();
    }
}
