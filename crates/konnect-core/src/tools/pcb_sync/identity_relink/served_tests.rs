use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

fn body(result: &CallToolResult) -> Value {
    serde_json::from_str(match &result.content[0] {
        ToolContent::Text { text } => text,
        _ => panic!("text JSON"),
    })
    .unwrap()
}

#[tokio::test]
async fn served_apply_checks_stale_source_mapping_and_full_post_write_readback() {
    for fault in 0..4 {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pcb_sync_identity");
        for entry in std::fs::read_dir(source).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|x| x == "kicad_sch") {
                std::fs::copy(&p, dir.join(p.file_name().unwrap())).unwrap();
            }
        }
        #[cfg(unix)]
        let logical = {
            let alias = dir.join("alias");
            std::os::unix::fs::symlink(dir, &alias).unwrap();
            alias
        };
        #[cfg(not(unix))]
        let logical = dir.to_path_buf();
        let sch = logical.join("pcb_sync_identity.kicad_sch");
        let board = dir.join("board.kicad_pcb");
        std::fs::write(&board, "board existence marker").unwrap();
        let net = dir.join("export.net");
        std::fs::write(
            &net,
            include_str!("../../../../tests/fixtures/pcb_sync_identity/pcb_sync_identity.net"),
        )
        .unwrap();
        let cli = crate::tools::cli::test_support::write_script(
            dir,
            "netlist-cli",
            &format!(
                "#!/bin/sh\ncp '{}' \"$5\"\n",
                net.to_string_lossy().replace('\'', "'\\''")
            ),
            &format!(
                "@copy /y \"{}\" \"%~5\" >nul\r\n@exit /b 0\r\n",
                net.display()
            ),
        );
        let (mapping, _, initial) = super::tests::fixture();
        let state = Arc::new(Mutex::new(initial.items));
        let writes = Arc::new(AtomicUsize::new(0));
        let observed = state.clone();
        let updates = writes.clone();
        let sch_changed = sch.clone();
        let nets = initial
            .nets
            .into_iter()
            .map(|(name, code)| kiapi::board::types::Net {
                name,
                code: Some(kiapi::board::types::NetCode { value: code }),
            })
            .collect::<Vec<_>>();
        let server = crate::tools::pcb_board::board_mock::spawn_kicad_holding_board(
            &board,
            move |command| {
                if command.type_url.ends_with("GetItems") {
                    return Some(konnect_ipc::builders::pack_any(
                        &kiapi::common::commands::GetItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            items: observed.lock().unwrap().clone(),
                        },
                        "kiapi.common.commands.GetItemsResponse",
                    ));
                }
                if command.type_url.ends_with("GetNets") {
                    return Some(konnect_ipc::builders::pack_any(
                        &kiapi::board::commands::NetsResponse { nets: nets.clone() },
                        "kiapi.board.commands.NetsResponse",
                    ));
                }
                if command.type_url.ends_with("BeginCommit") {
                    return Some(konnect_ipc::builders::pack_any(
                        &kiapi::common::commands::BeginCommitResponse {
                            id: Some(kiapi::common::types::Kiid {
                                value: "relink-commit".into(),
                            }),
                        },
                        "kiapi.common.commands.BeginCommitResponse",
                    ));
                }
                if command.type_url.ends_with("EndCommit") {
                    return Some(konnect_ipc::builders::pack_any(
                        &kiapi::common::commands::EndCommitResponse {},
                        "kiapi.common.commands.EndCommitResponse",
                    ));
                }
                if command.type_url.ends_with("UpdateItems") {
                    updates.fetch_add(1, Ordering::SeqCst);
                    let request =
                        kiapi::common::commands::UpdateItems::decode(command.value.as_slice())
                            .unwrap();
                    assert_eq!(request.items.len(), 1);
                    let wanted = request.items[0].clone();
                    let mut actual = wanted.clone();
                    if fault == 1 {
                        let mut fp =
                            kiapi::board::types::FootprintInstance::decode(actual.value.as_slice())
                                .unwrap();
                        fp.symbol_path = None;
                        actual.value = fp.encode_to_vec();
                    }
                    let mut items = observed.lock().unwrap();
                    let index = items
                        .iter()
                        .position(|item| {
                            konnect_ipc::builders::any_is(
                                item,
                                "kiapi.board.types.FootprintInstance",
                            )
                        })
                        .unwrap();
                    items[index] = actual;
                    if fault == 2 {
                        items
                            .iter_mut()
                            .find(|i| i.type_url.ends_with("Track"))
                            .unwrap()
                            .value
                            .push(0);
                    }
                    if fault == 3 {
                        let mut text = std::fs::read_to_string(&sch_changed).unwrap();
                        text.push('\n');
                        std::fs::write(&sch_changed, text).unwrap();
                    }
                    return Some(konnect_ipc::builders::pack_any(
                        &kiapi::common::commands::UpdateItemsResponse {
                            header: None,
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            updated_items: vec![kiapi::common::commands::ItemUpdateResult {
                                status: Some(kiapi::common::commands::ItemStatus {
                                    code: kiapi::common::commands::ItemStatusCode::IscOk as i32,
                                    error_message: String::new(),
                                }),
                                item: Some(wanted),
                            }],
                        },
                        "kiapi.common.commands.UpdateItemsResponse",
                    ));
                }
                None
            },
        );
        let config = crate::tools::ServerConfig {
            kicad_cli: cli.to_string_lossy().into_owned(),
            kicad_binary: String::new(),
            ipc_address: server.address().into(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        };
        let ctx = ToolContext::new(config, Arc::new(crate::router::ToolRouter::new()));
        let args = json!({"board":board,"schematic":sch,"reference":mapping.reference,"footprint_uuid":mapping.footprint_uuid,"old_symbol_path":mapping.old_symbol_path,"new_symbol_path":mapping.new_symbol_path,"dry_run":true});
        let before = state.lock().unwrap().clone();
        let dry = handle(&args, &ctx).await.unwrap();
        assert!(!dry.is_error, "{}", body(&dry));
        let revision = body(&dry)["plan_revision"].clone();
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        let mut apply = args.clone();
        apply["dry_run"] = json!(false);
        apply["expected_plan_revision"] = json!("0".repeat(64));
        let stale = handle(&apply, &ctx).await.unwrap();
        assert!(stale.is_error);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert_eq!(before, *state.lock().unwrap());
        let source_before = std::fs::read_to_string(&sch).unwrap();
        std::fs::write(&sch, format!("{source_before}\n")).unwrap();
        apply["expected_plan_revision"] = revision.clone();
        let stale_source = handle(&apply, &ctx).await.unwrap();
        assert!(stale_source.is_error);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        std::fs::write(&sch, &source_before).unwrap();
        let original_path = apply["new_symbol_path"].clone();
        apply["new_symbol_path"] = json!("/33333333-3333-4333-8333-333333333333");
        let wrong = handle(&apply, &ctx).await.unwrap();
        assert!(wrong.is_error);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        apply["new_symbol_path"] = original_path;
        let result = handle(&apply, &ctx).await.unwrap();
        let response = body(&result);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        if fault == 0 {
            assert!(!result.is_error, "{response}");
            assert_eq!(response["status"], "applied");
            assert_eq!(response["identities_relinked"]["applied"], 1);
        } else {
            assert!(result.is_error);
            assert_eq!(response["status"], "uncertain", "{response}");
            assert_eq!(response["identities_relinked"]["applied"], 0);
            assert_eq!(response["outcome"]["completed"], 0);
            assert_eq!(response["outcome"]["retry"]["safe"], false);
        }
        assert_ne!(before, *state.lock().unwrap());
        assert_eq!(
            std::fs::read_to_string(&board).unwrap(),
            "board existence marker"
        );
    }
}
