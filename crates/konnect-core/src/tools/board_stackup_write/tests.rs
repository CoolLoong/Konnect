use super::*;
use std::{path::PathBuf, sync::Arc};

const BOARD: &str = r#"(kicad_pcb (version 20260121)
    (general (thickness 1.6) (legacy "keep"))
    (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
    (setup (pad_to_mask_clearance 0) (future "preserve"))
    (footprint "LIB:PART" (property "Reference" "U1") (pad "1" smd rect (net "GND")))
    (segment (start 1 2) (end 3 4) (width 0.25) (layer "F.Cu") (net "GND"))
)"#;
fn args(board: &Path) -> Value {
    json!({"board":board,"confirm_closed_saved":true,"board_thickness_mm":1.59,"layers":[
    {"name":"F.Cu","kind":"copper","thickness_mm":0.035},
    {"name":"dielectric 1","kind":"core","thickness_mm":1.52,"material":"FR4","epsilon_r":4.6,"loss_tangent":0.02},
    {"name":"B.Cu","kind":"copper","thickness_mm":0.035}]})
}
fn body(r: &CallToolResult) -> Value {
    serde_json::from_str(match &r.content[0] {
        ToolContent::Text { text } => text,
        _ => panic!("text"),
    })
    .unwrap()
}
fn xml() -> String {
    r#"<IPC-2581><Spec name="F.Cu_1"><General type="MATERIAL"><Property text="COPPER"/></General></Spec>
<Spec name="DIELECTRIC_1_1"><General type="MATERIAL"><Property text="FR4"/><Property text="Type : core"/></General><Dielectric type="DIELECTRIC_CONSTANT"><Property value="4.60"/></Dielectric><Dielectric type="LOSS_TANGENT"><Property value="0.020"/></Dielectric></Spec>
<Spec name="B.Cu_1"><General type="MATERIAL"><Property text="COPPER"/></General></Spec>
<Stackup overallThickness="1.59"><StackupGroup><StackupLayer layerOrGroupRef="F.Cu" thickness="0.035"><SpecRef id="F.Cu_1"/></StackupLayer>
<StackupLayer layerOrGroupRef="DIELECTRIC_1" thickness="1.52"><SpecRef id="DIELECTRIC_1_1"/></StackupLayer>
<StackupLayer layerOrGroupRef="B.Cu" thickness="0.035"><SpecRef id="B.Cu_1"/></StackupLayer></StackupGroup></Stackup></IPC-2581>"#.into()
}
fn ctx(ipc: String, cli: String) -> ToolContext {
    ToolContext::new(
        crate::tools::ServerConfig {
            kicad_cli: cli,
            kicad_binary: String::new(),
            ipc_address: ipc,
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        },
        Arc::new(crate::router::ToolRouter::new()),
    )
}

#[test]
fn stackup_plan_preserves_all_objects_and_unrequested_properties() {
    let r = request(&args(Path::new("board"))).unwrap();
    let p = plan(BOARD, &r).unwrap();
    assert!(p.candidate.contains("(thickness 1.59)"));
    assert!(p.candidate.contains("(future \"preserve\")"));
    assert!(p.candidate.contains("(segment (start 1 2) (end 3 4)"));
    assert_eq!(
        without_stackup(strict(BOARD).unwrap(), true),
        without_stackup(strict(&p.candidate).unwrap(), true)
    );
    let source = p.candidate.replace(
        "(type \"core\")",
        "(type \"core\") (color \"unknown\") (future_layer_setting yes)",
    );
    let mut changed = r;
    changed.layers[1].thickness_mm = 1.50;
    changed.layers[1].epsilon_r = None;
    changed.board_thickness_mm = None;
    let edited = plan(&source, &changed).unwrap();
    assert!(edited.candidate.contains("(epsilon_r 4.6)"));
    assert!(edited.candidate.contains("(color \"unknown\")"));
    assert!(edited.candidate.contains("(future_layer_setting yes)"));
    assert_eq!(
        without_stackup(strict(&source).unwrap(), false),
        without_stackup(strict(&edited.candidate).unwrap(), false)
    );
    assert_ne!(
        edited.revision,
        plan(&(source.clone() + "\n"), &changed).unwrap().revision
    );
    assert!(plan(&(BOARD.to_string() + " (damage)"), &changed).is_err());
    let duplicate = p.candidate.replace(
        "(material \"FR4\")",
        "(material \"FR4\") (material \"other\")",
    );
    assert!(plan(&duplicate, &changed).is_err());
    let mut wrong = args(Path::new("board"));
    wrong["layers"][0]["name"] = json!("In1.Cu");
    assert!(plan(BOARD, &request(&wrong).unwrap()).is_err());
}

#[test]
fn independent_cli_xml_rejects_missing_wrong_reordered_and_malformed_evidence() {
    let r = request(&args(Path::new("board"))).unwrap();
    assert_eq!(verify_xml(&xml(), &r).unwrap()["layers_verified"], 3);
    for bad in [
        xml().replace("1.52", "1.40"),
        xml().replace("1.59", "1.40"),
        xml().replace("4.60", "3.80"),
        xml().replace("FR4", "wrong"),
        xml().replace("Type : core", "Type : prepreg"),
        xml().replace("B.Cu\" thickness", "In1.Cu\" thickness"),
        "<bad>".into(),
    ] {
        assert!(verify_xml(&bad, &r).is_err());
    }
    let mut nominal = args(Path::new("board"));
    nominal["board_thickness_mm"] = json!(1.6);
    assert!(
        verify_xml(&xml(), &request(&nominal).unwrap()).is_ok(),
        "nominal thickness differs legitimately from physical sum"
    );
    let mut wrong = args(Path::new("board"));
    wrong["confirm_closed_saved"] = json!(false);
    assert!(request(&wrong).is_err());
    wrong["confirm_closed_saved"] = json!(true);
    wrong["layers"][0]["thickness_mm"] = json!(-0.1);
    assert!(request(&wrong).is_err());
}

#[tokio::test]
async fn served_stackup_refuses_open_locked_stale_and_verifies_candidate_and_written_readback() {
    for fault in 0..3 {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let board = dir.join("board.kicad_pcb");
        std::fs::write(&board, BOARD).unwrap();
        let other = dir.join("other.kicad_pcb");
        std::fs::write(&other, "other").unwrap();
        let server =
            crate::tools::pcb_board::board_mock::spawn_kicad_holding_board(&other, |_| None);
        let output = dir.join("verified.xml");
        std::fs::write(&output, xml()).unwrap();
        let script = if fault == 1 {
            format!(
                "#!/bin/sh\nif [ \"$8\" = '{}' ]; then exit 8; fi\ncp '{}' \"$5\"\n",
                board.display(),
                output.display()
            )
        } else if fault == 2 {
            format!(
                "#!/bin/sh\ncp '{}' \"$5\"\nif [ \"$8\" = '{}' ]; then echo damaged >> '{}'; fi\n",
                output.display(),
                board.display(),
                board.display()
            )
        } else {
            format!("#!/bin/sh\ncp '{}' \"$5\"\n", output.display())
        };
        let cli = crate::tools::cli::test_support::write_script(
            dir,
            "mock-stackup-cli",
            &script,
            &format!("@copy /y \"{}\" \"%~5\" >nul\r\n", output.display()),
        );
        let context = ctx(server.address().into(), cli.to_string_lossy().into());
        let a = args(&board);
        let lock = konnect_sexp::writer::kicad_editor_lock_path(&board).unwrap();
        std::fs::write(&lock, "open editor").unwrap();
        let refused = handle_set_stackup(&a, &context).await.unwrap();
        assert!(refused.is_error);
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
        std::fs::remove_file(lock).unwrap();
        let dry = handle_set_stackup(&a, &context).await.unwrap();
        assert!(!dry.is_error, "{}", body(&dry));
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
        let mut apply = a.clone();
        apply["dry_run"] = json!(false);
        apply["expected_plan_revision"] = json!("0".repeat(64));
        assert!(handle_set_stackup(&apply, &context).await.unwrap().is_error);
        assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
        apply["expected_plan_revision"] = body(&dry)["plan_revision"].clone();
        let result = handle_set_stackup(&apply, &context).await.unwrap();
        let actual = body(&result);
        if cfg!(windows) || fault == 0 {
            assert!(!result.is_error, "{actual}");
            assert_eq!(actual["readback_verified"], true);
            assert_eq!(actual["stackups_edited"]["applied"], 1);
        } else {
            assert!(result.is_error);
            assert_eq!(actual["status"], "uncertain");
            assert_eq!(actual["stackups_edited"]["applied"], 0);
            assert_eq!(actual["outcome"]["retry"]["safe"], false);
        }
        let preimage = actual["preimage"].as_str().unwrap();
        assert_eq!(std::fs::read_to_string(preimage).unwrap(), BOARD);
    }
    let temp = tempfile::tempdir().unwrap();
    let board = temp.path().join("open.kicad_pcb");
    std::fs::write(&board, BOARD).unwrap();
    let server = crate::tools::pcb_board::board_mock::spawn_kicad_holding_board(&board, |_| None);
    let result = handle_set_stackup(
        &args(&board),
        &ctx(server.address().into(), "must not run".into()),
    )
    .await
    .unwrap();
    assert!(result.is_error);
    assert!(body(&result)["reason"]
        .as_str()
        .unwrap()
        .contains("open in KiCad"));
    assert_eq!(std::fs::read_to_string(&board).unwrap(), BOARD);
}

#[tokio::test]
#[ignore = "requires explicit saved isolated FC copy and real KiCad CLI"]
async fn real_cli_closed_stackup_reopen_preserves_all_unrelated_board_source() {
    let board = PathBuf::from(std::env::var("KONNECT_STACKUP_BOARD").unwrap());
    assert!(board
        .to_string_lossy()
        .contains("/work/controlled-stackup-acceptance/project/"));
    let source = std::fs::read_to_string(&board).unwrap();
    let ctx = ToolContext::new(
        crate::tools::ServerConfig {
            kicad_cli: "/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli".into(),
            kicad_binary: String::new(),
            ipc_address: "ipc:///tmp/kicad/api-35808.sock".into(),
            project_dir: None,
            jlcpcb_db_path: None,
            auto_load_toolsets: false,
            eager_toolsets: false,
        },
        Arc::new(crate::router::ToolRouter::new()),
    );
    let layers = json!([
        {"name":"F.SilkS","kind":"silkscreen","thickness_mm":0},
        {"name":"F.Paste","kind":"solderpaste","thickness_mm":0},
        {"name":"F.Mask","kind":"soldermask","thickness_mm":0.03048,"epsilon_r":3.8},
        {"name":"F.Cu","kind":"copper","thickness_mm":0.035},
        {"name":"dielectric 1","kind":"prepreg","thickness_mm":0.2104,"material":"7628 RC49%","epsilon_r":4.4,"loss_tangent":0.02},
        {"name":"In1.Cu","kind":"copper","thickness_mm":0.0152},
        {"name":"dielectric 2","kind":"core","thickness_mm":1.065,"material":"FR4","epsilon_r":4.6,"loss_tangent":0.02},
        {"name":"In2.Cu","kind":"copper","thickness_mm":0.0152},
        {"name":"dielectric 3","kind":"prepreg","thickness_mm":0.2104,"material":"7628 RC49%","epsilon_r":4.4,"loss_tangent":0.02},
        {"name":"B.Cu","kind":"copper","thickness_mm":0.035},
        {"name":"B.Mask","kind":"soldermask","thickness_mm":0.03048,"epsilon_r":3.8},
        {"name":"B.Paste","kind":"solderpaste","thickness_mm":0},
        {"name":"B.SilkS","kind":"silkscreen","thickness_mm":0}
    ]);
    let args =
        json!({"board":board,"confirm_closed_saved":true,"layers":layers,"board_thickness_mm":1.6});
    let dry = handle_set_stackup(&args, &ctx).await.unwrap();
    let preview = body(&dry);
    assert!(!dry.is_error, "{preview}");
    assert_eq!(std::fs::read_to_string(&board).unwrap(), source);
    let mut apply = args.clone();
    apply["dry_run"] = json!(false);
    apply["expected_plan_revision"] = json!("0".repeat(64));
    assert!(handle_set_stackup(&apply, &ctx).await.unwrap().is_error);
    assert_eq!(std::fs::read_to_string(&board).unwrap(), source);
    apply["expected_plan_revision"] = preview["plan_revision"].clone();
    let applied = handle_set_stackup(&apply, &ctx).await.unwrap();
    let actual = body(&applied);
    assert!(!applied.is_error, "{actual}");
    let candidate = plan(&source, &request(&args).unwrap()).unwrap().candidate;
    let after = std::fs::read_to_string(&board).unwrap();
    assert_eq!(after, candidate);
    assert_eq!(
        without_stackup(strict(&source).unwrap(), true),
        without_stackup(strict(&after).unwrap(), true)
    );
    assert_eq!(
        std::fs::read_to_string(actual["preimage"].as_str().unwrap()).unwrap(),
        source
    );
    let readback = cli_readback(&ctx, &board, &request(&args).unwrap())
        .await
        .unwrap();
    let out = PathBuf::from(std::env::var("KONNECT_STACKUP_EVIDENCE").unwrap());
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("responses.json"),serde_json::to_vec_pretty(&json!({"request":args,"preview":preview,"apply":actual,"independent_cli":readback,"all_unrelated_source_preserved":true,"formal_apply_called":false})).unwrap()).unwrap();
}
