//! Tests use a KiCad-authored Via wire image and isolated IPC doubles.
use super::*;
use crate::tools::pcb_board::board_mock::{
    board_document, ctx_talking_to, spawn_kicad_holding_board,
};
use konnect_ipc::gen::kiapi;
use std::sync::{Arc, Mutex};

const UUID: &str = "9ee81141-93ba-4326-b7c6-0af0d7dae141";
fn captured_via() -> kiapi::board::types::Via {
    kiapi::board::types::Via::decode(
        include_bytes!("../../tests/fixtures/via_kicad10_0_6.ipc.bin").as_slice(),
    )
    .unwrap()
}
fn body(result: &CallToolResult) -> serde_json::Value {
    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
        panic!("JSON text expected")
    };
    serde_json::from_str(text).unwrap()
}
#[derive(Clone, Copy, PartialEq, Debug)]
enum Fault {
    None,
    Missing,
    WrongType,
    Malformed,
    Duplicate,
    UnknownField,
    EmptyUuid,
    Incomplete,
    NoPayloadBefore,
    NoPayloadAfter,
    BadStatusAfter,
    WrongEnvelopeAfter,
    StillPresent,
    Position,
    Net,
    Drill,
    Layers,
    PadSize,
    Lock,
    Parent,
    WriteReplyMissing,
}
struct State {
    via: Option<kiapi::board::types::Via>,
    reads: usize,
    writes: usize,
}
fn mock(board: &Path, fault: Fault) -> (crate::test_support::MockIpcServer, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State {
        via: Some(captured_via()),
        reads: 0,
        writes: 0,
    }));
    let shared = state.clone();
    let document = board_document(&board.to_string_lossy());
    let server = spawn_kicad_holding_board(board, move |command| {
        let mut state = shared.lock().unwrap();
        if command.type_url.ends_with("GetItems") {
            let request =
                kiapi::common::commands::GetItems::decode(command.value.as_slice()).unwrap();
            assert_eq!(request.header.unwrap().document.unwrap(), document);
            assert_eq!(
                request.types,
                vec![kiapi::common::types::KiCadObjectType::KotPcbVia as i32]
            );
            state.reads += 1;
            if fault == Fault::NoPayloadBefore
                || (state.reads > 1 && fault == Fault::NoPayloadAfter)
            {
                return None;
            }
            if state.reads > 1 && fault == Fault::WrongEnvelopeAfter {
                return Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetItemsResponse {
                        header: None,
                        status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                        items: state
                            .via
                            .as_ref()
                            .map(|via| {
                                vec![konnect_ipc::builders::pack_any(
                                    via,
                                    "kiapi.board.types.Via",
                                )]
                            })
                            .unwrap_or_default(),
                    },
                    "kiapi.common.commands.DeleteItemsResponse",
                ));
            }
            let mut items = if fault == Fault::Missing {
                vec![]
            } else {
                state
                    .via
                    .as_ref()
                    .map(|via| {
                        vec![konnect_ipc::builders::pack_any(
                            via,
                            "kiapi.board.types.Via",
                        )]
                    })
                    .unwrap_or_default()
            };
            if let Some(item) = items.first_mut() {
                if fault == Fault::EmptyUuid || fault == Fault::Incomplete {
                    let mut via = captured_via();
                    if fault == Fault::EmptyUuid {
                        via.id.as_mut().unwrap().value.clear();
                    }
                    if fault == Fault::Incomplete {
                        via.pad_stack = None;
                    }
                    *item = konnect_ipc::builders::pack_any(&via, "kiapi.board.types.Via");
                }
                if fault == Fault::WrongType {
                    item.type_url = "type.googleapis.com/kiapi.board.types.Track".to_owned();
                }
                if fault == Fault::Malformed {
                    item.value = vec![0xff];
                }
                if fault == Fault::UnknownField {
                    item.value.extend_from_slice(&[0xa0, 0x06, 0x01]);
                }
            }
            if fault == Fault::Duplicate {
                items.extend(items.clone());
            }
            return Some(konnect_ipc::builders::pack_any(
                &kiapi::common::commands::GetItemsResponse {
                    header: None,
                    status: if state.reads > 1 && fault == Fault::BadStatusAfter {
                        kiapi::common::types::ItemRequestStatus::IrsUnknown as i32
                    } else {
                        kiapi::common::types::ItemRequestStatus::IrsOk as i32
                    },
                    items,
                },
                "kiapi.common.commands.GetItemsResponse",
            ));
        }
        if command.type_url.ends_with("DeleteItems") {
            let request =
                kiapi::common::commands::DeleteItems::decode(command.value.as_slice()).unwrap();
            assert_eq!(request.header.unwrap().document.unwrap(), document);
            assert_eq!(request.item_ids[0].value, UUID);
            state.writes += 1;
            if fault != Fault::StillPresent {
                state.via = None;
            }
            if fault == Fault::WriteReplyMissing {
                return None;
            }
            return Some(konnect_ipc::builders::pack_any(
                &kiapi::common::commands::DeleteItemsResponse {
                    header: None,
                    status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                    deleted_items: vec![],
                },
                "kiapi.common.commands.DeleteItemsResponse",
            ));
        }
        if command.type_url.ends_with("UpdateItems") {
            let request =
                kiapi::common::commands::UpdateItems::decode(command.value.as_slice()).unwrap();
            assert_eq!(request.header.unwrap().document.unwrap(), document);
            assert_eq!(request.items.len(), 1);
            assert!(konnect_ipc::builders::any_is(
                &request.items[0],
                "kiapi.board.types.Via"
            ));
            let mut updated =
                kiapi::board::types::Via::decode(request.items[0].value.as_slice()).unwrap();
            // Independently assert the writer changed exactly position.
            let mut without_position = updated.clone();
            without_position.position = state.via.as_ref().unwrap().position;
            assert_eq!(without_position, *state.via.as_ref().unwrap());
            state.writes += 1;
            match fault {
                Fault::Position => updated.position.as_mut().unwrap().x_nm += 1,
                Fault::Net => updated.net.as_mut().unwrap().name = "OTHER".to_owned(),
                Fault::Drill => {
                    updated
                        .pad_stack
                        .as_mut()
                        .unwrap()
                        .drill
                        .as_mut()
                        .unwrap()
                        .diameter
                        .as_mut()
                        .unwrap()
                        .x_nm += 1
                }
                Fault::Layers => {
                    updated
                        .pad_stack
                        .as_mut()
                        .unwrap()
                        .drill
                        .as_mut()
                        .unwrap()
                        .end_layer = kiapi::board::types::BoardLayer::BlIn1Cu as i32
                }
                Fault::PadSize => {
                    updated.pad_stack.as_mut().unwrap().copper_layers[0]
                        .size
                        .as_mut()
                        .unwrap()
                        .y_nm += 1
                }
                Fault::Lock => updated.locked = kiapi::common::types::LockedState::LsLocked as i32,
                Fault::Parent => {
                    updated.parent = Some(kiapi::common::types::Kiid {
                        value: "changed-parent".to_owned(),
                    })
                }
                _ => {}
            }
            state.via = Some(updated);
            if fault == Fault::WriteReplyMissing {
                return None;
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
                        item: None,
                    }],
                },
                "kiapi.common.commands.UpdateItemsResponse",
            ));
        }
        None
    });
    (server, state)
}
fn board_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let board = dir.path().join("test.kicad_pcb");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/specctra_two_resistors.kicad_pcb"),
        &board,
    )
    .unwrap();
    board
}
fn args(board: &Path) -> serde_json::Value {
    json!({ "board": board, "uuid": UUID, "x": 9.25, "y": 15.0 })
}
#[tokio::test]
async fn via_refusals_precede_every_write() {
    for fault in [
        Fault::Missing,
        Fault::WrongType,
        Fault::Malformed,
        Fault::Duplicate,
        Fault::UnknownField,
        Fault::EmptyUuid,
        Fault::Incomplete,
        Fault::NoPayloadBefore,
    ] {
        for is_move in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let board = board_file(&dir);
            let original = std::fs::read(&board).unwrap();
            let (server, state) = mock(&board, fault);
            let mut input = args(&board);
            if !is_move {
                input.as_object_mut().unwrap().remove("x");
                input.as_object_mut().unwrap().remove("y");
            }
            let result = handle_via_mutation(
                &input,
                &ctx_talking_to(server.address().to_owned()),
                is_move,
            )
            .await
            .unwrap();
            assert!(result.is_error, "{fault:?}");
            assert_eq!(
                body(&result)["error"]["kind"],
                if fault == Fault::Missing {
                    "stale_target"
                } else {
                    "handler_error"
                }
            );
            assert_eq!(body(&result)["outcome"]["status"], "failed");
            assert_eq!(state.lock().unwrap().writes, 0, "{fault:?}");
            assert_eq!(std::fs::read(&board).unwrap(), original);
        }
    }
}
#[tokio::test]
async fn via_wrong_board_and_invalid_coordinate_refuse_before_write() {
    let dir = tempfile::tempdir().unwrap();
    let board = board_file(&dir);
    let other = dir.path().join("other.kicad_pcb");
    std::fs::copy(&board, &other).unwrap();
    let (server, state) = mock(&other, Fault::None);
    for is_move in [false, true] {
        let result = handle_via_mutation(
            &args(&board),
            &ctx_talking_to(server.address().to_owned()),
            is_move,
        )
        .await
        .unwrap();
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("wrong_document")
        );
    }
    let ctx = ctx_talking_to(server.address().to_owned());
    for value in [
        json!(2147.483648),
        json!(-2147.483649),
        json!("9"),
        serde_json::Value::Null,
    ] {
        let mut input = args(&other);
        input["x"] = value;
        let result = handle_move_via(&input, &ctx).await.unwrap();
        assert_eq!(
            crate::mcp::error::extract_error_kind(&result).as_deref(),
            Some("invalid_argument")
        );
    }
    assert_eq!(state.lock().unwrap().writes, 0);
    assert_eq!(state.lock().unwrap().reads, 0);
}
#[tokio::test]
async fn via_post_write_failures_are_uncertain_with_preimage_and_recovery() {
    for is_move in [false, true] {
        let faults = if is_move {
            vec![
                Fault::NoPayloadAfter,
                Fault::BadStatusAfter,
                Fault::WrongEnvelopeAfter,
                Fault::Position,
                Fault::Net,
                Fault::Drill,
                Fault::Layers,
                Fault::PadSize,
                Fault::Lock,
                Fault::Parent,
                Fault::WriteReplyMissing,
            ]
        } else {
            vec![
                Fault::NoPayloadAfter,
                Fault::BadStatusAfter,
                Fault::WrongEnvelopeAfter,
                Fault::StillPresent,
                Fault::WriteReplyMissing,
            ]
        };
        for fault in faults {
            let dir = tempfile::tempdir().unwrap();
            let board = board_file(&dir);
            let (server, state) = mock(&board, fault);
            let result = handle_via_mutation(
                &args(&board),
                &ctx_talking_to(server.address().to_owned()),
                is_move,
            )
            .await
            .unwrap();
            assert!(result.is_error, "{fault:?}");
            let response = body(&result);
            assert_eq!(
                response["error"]["kind"], "mutation_outcome_uncertain",
                "{fault:?}"
            );
            assert_eq!(response["outcome"]["status"], "uncertain");
            assert_eq!(response["outcome"]["retry"]["safe"], false);
            assert_eq!(response["preimage"]["uuid"], UUID);
            assert_eq!(state.lock().unwrap().writes, 1);
        }
    }
}
#[tokio::test]
async fn via_success_and_noop_have_independent_readback() {
    for is_move in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        let (server, state) = mock(&board, Fault::None);
        let result = handle_via_mutation(
            &args(&board),
            &ctx_talking_to(server.address().to_owned()),
            is_move,
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{result:?}");
        let response = body(&result);
        assert_eq!(response["outcome"]["status"], "complete");
        assert_eq!(response["preimage"]["uuid"], UUID);
        assert_eq!(state.lock().unwrap().reads, 2);
        assert_eq!(state.lock().unwrap().writes, 1);
        if is_move {
            assert_eq!(response["readback"]["position"], json!({"x":9.25,"y":15.0}));
            assert_eq!(
                response["readback"]["drill_nm"],
                response["preimage"]["drill_nm"]
            );
        } else {
            assert!(state.lock().unwrap().via.is_none());
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let board = board_file(&dir);
    let (server, state) = mock(&board, Fault::None);
    let result = handle_move_via(
        &json!({"board": board, "uuid": UUID, "x":7.5,"y":17.6}),
        &ctx_talking_to(server.address().to_owned()),
    )
    .await
    .unwrap();
    assert!(!result.is_error);
    assert_eq!(body(&result)["changed"], false);
    assert_eq!(state.lock().unwrap().reads, 2);
    assert_eq!(state.lock().unwrap().writes, 0);
}
#[tokio::test]
async fn via_served_dispatch_preserves_success_uncertainty_and_schema_refusal() {
    for fault in [Fault::None, Fault::Position] {
        let dir = tempfile::tempdir().unwrap();
        let board = board_file(&dir);
        let (server, state) = mock(&board, fault);
        let mut config = ctx_talking_to(server.address().to_owned()).config.clone();
        config.eager_toolsets = true;
        let handler = crate::mcp::handler::McpHandler::new(config).await.unwrap();
        let mut input = args(&board);
        input["xx"] = json!(9);
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"move_via","arguments":input}});
        let result = handler
            .handle_message(request)
            .await
            .unwrap()
            .result
            .unwrap();
        assert_eq!(result["isError"], true);
        assert_eq!(state.lock().unwrap().writes, 0);
        let request = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"move_via","arguments":args(&board)}});
        let result = handler
            .handle_message(request.clone())
            .await
            .unwrap()
            .result
            .unwrap();
        let response: serde_json::Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            response["outcome"]["status"],
            if fault == Fault::None {
                "complete"
            } else {
                "uncertain"
            }
        );
        println!(
            "MOCK_MCP_EVIDENCE {}",
            json!({"source":"mock_kicad_ipc","request":request,"response":result})
        );
    }
}
