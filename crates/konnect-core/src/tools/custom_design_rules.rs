//! Validated single-rule updates, preserving unrelated bytes and rule priority.

use super::{get_path, ToolContext, ToolDef};
use crate::{
    mcp::{error::ToolErrorKind, protocol::CallToolResult},
    outcome::{self, OutcomeStatus},
    tool,
};
use anyhow::{ensure, Context, Result};
use konnect_sexp::{parse_sexp, writer, SexpNode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, ops::Range, path::PathBuf, time::Duration};

pub(super) fn tool() -> ToolDef {
    tool!(
        "set_custom_design_rule",
        "Validate and upsert ONE named KiCad custom DRC rule in the sibling .kicad_dru. \
         Supply a complete (rule ...) definition, including any condition, constraints, \
         layer and severity. Preserves all other rule bytes and priority; a new rule is \
         appended and therefore has highest priority. Defaults to dry_run. Apply requires \
         expected_rules_revision returned by the dry run. Requires the saved PCB, sibling \
         project and KiCad 10 CLI. Compiles the entire candidate using a temporary assertion \
         probe, then runs actual DRC on an isolated saved copy; findings are not a clean-board \
         verdict. Writes only .kicad_dru with conflict checking and readback. Does not save \
         or reload the live editor; reload custom rules in KiCad before live routing.",
        json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "board": {"type": "string", "description": "Saved .kicad_pcb path; sibling .kicad_pro must exist"},
                "rule": {"type": "string", "minLength": 1, "maxLength": 65536, "description": "Exactly one complete KiCad (rule ...) definition, without a version header"},
                "dry_run": {"type": "boolean", "default": true},
                "expected_rules_revision": {"type": "string", "pattern": "^[0-9a-f]{64}$", "description": "Required for apply: exact revision observed by dry_run (also represents an absent file)"}
            }, "required": ["board", "rule"]
        }),
        |args, ctx| async move { handle(args, ctx).await }
    )
}

struct Form {
    span: Range<usize>,
    node: SexpNode,
}

/// Scan the entire input, including comments and escaped strings. The ordinary
/// board reader accepts trailing unparsable input, so it cannot own this guard.
fn forms(source: &str) -> Result<Vec<Form>> {
    let mut result = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    let (mut quoted, mut escaped, mut comment) = (false, false, false);
    let mut clean = String::new();
    for (at, ch) in source.char_indices() {
        if comment {
            if ch == '\n' {
                comment = false;
                if depth > 0 {
                    clean.push('\n');
                }
            }
            continue;
        }
        if quoted {
            clean.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
            continue;
        }
        if ch == '#' {
            comment = true;
            if depth > 0 {
                clean.push(' ');
            }
            continue;
        }
        if depth == 0 {
            if ch.is_whitespace() {
                continue;
            }
            ensure!(ch == '(', "unexpected top-level text at byte {at}");
            start = at;
            clean.clear();
        }
        clean.push(ch);
        match ch {
            '"' => quoted = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    result.push(Form {
                        span: start..at + 1,
                        node: parse_sexp(&clean)?,
                    });
                }
            }
            _ => {}
        }
    }
    ensure!(depth == 0 && !quoted, "incomplete rule or quoted string");
    Ok(result)
}

fn rule_name(node: &SexpNode) -> Result<&str> {
    ensure!(node.head() == Some("rule"), "expected a rule definition");
    let name = node
        .get(1)
        .and_then(SexpNode::as_str)
        .context("missing rule name")?;
    ensure!(
        !name.is_empty() && !name.chars().any(char::is_control),
        "invalid rule name"
    );
    Ok(name)
}

fn candidate(source: Option<&str>, definition: &str) -> Result<(String, String, bool)> {
    ensure!(definition.len() <= 65536, "rule exceeds 65536 bytes");
    let replacement = forms(definition)?;
    ensure!(
        replacement.len() == 1,
        "rule must contain exactly one definition"
    );
    let form = &replacement[0];
    let name = rule_name(&form.node)?.to_string();
    ensure!(
        !form.node.find_all("constraint").is_empty(),
        "rule needs at least one constraint"
    );
    for singleton in ["condition", "layer", "severity"] {
        ensure!(
            form.node.find_all(singleton).len() <= 1,
            "duplicate {singleton} clauses"
        );
    }
    let mut constraint_types = HashSet::new();
    for constraint in form.node.find_all("constraint") {
        let kind = constraint
            .get(1)
            .and_then(SexpNode::as_str)
            .context("missing constraint type")?;
        ensure!(
            constraint_types.insert(kind),
            "duplicate constraint type '{kind}'"
        );
    }
    let source = source.unwrap_or("(version 1)\n");
    let existing = forms(source)?;
    ensure!(
        existing.first().is_some_and(|f| f.node
            == SexpNode::List(vec![
                SexpNode::Atom("version".into()),
                SexpNode::Atom("1".into())
            ])),
        "custom rules must start with exactly (version 1)"
    );
    let mut names = HashSet::new();
    let mut matched = None;
    for f in &existing[1..] {
        let old_name = rule_name(&f.node)?;
        ensure!(
            names.insert(old_name),
            "duplicate existing rule name '{old_name}'"
        );
        if old_name == name {
            matched = Some(f.span.clone());
        }
    }
    let mut next = source.to_string();
    if let Some(span) = &matched {
        next.replace_range(span.clone(), &definition[form.span.clone()]);
    } else {
        if !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&definition[form.span.clone()]);
        next.push('\n');
    }
    Ok((next, name, matched.is_some()))
}

fn revision(source: Option<&str>) -> String {
    // Existence is part of the revision: absent and an empty file differ.
    let mut digest = Sha256::new();
    digest.update([u8::from(source.is_some())]);
    if let Some(source) = source {
        digest.update(source.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

struct Snapshot {
    board: PathBuf,
    project: PathBuf,
    rules: PathBuf,
    board_bytes: Vec<u8>,
    project_bytes: Vec<u8>,
    original: Option<String>,
    next: String,
    name: String,
    replaced: bool,
    dry_run: bool,
}

fn read_rules(path: &std::path::Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "rules path must be a regular file, not a symlink"
            );
            Ok(Some(writer::read_consistent(path)?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl Snapshot {
    fn prepare(args: &Value) -> Result<Self> {
        let board = get_path(args, "board")?.canonicalize()?;
        ensure!(
            board.extension().is_some_and(|s| s == "kicad_pcb"),
            "board must be a .kicad_pcb file"
        );
        let project = board.with_extension("kicad_pro");
        let rules = board.with_extension("kicad_dru");
        let board_bytes = std::fs::read(&board)?;
        let project_bytes =
            std::fs::read(&project).context("sibling project is required for rule validation")?;
        // Do not let KiCad silently use default settings for invalid JSON.
        let project_json: Value = serde_json::from_slice(&project_bytes)?;
        ensure!(project_json.is_object(), "project must be a JSON object");
        let original = read_rules(&rules)?;
        let definition = args["rule"].as_str().context("rule must be a string")?;
        let (next, name, replaced) = candidate(original.as_deref(), definition)?;
        let dry_run = match args.get("dry_run") {
            None => true,
            Some(value) => value.as_bool().context("dry_run must be a boolean")?,
        };
        if let Some(expected) = args.get("expected_rules_revision") {
            ensure!(
                expected.as_str() == Some(revision(original.as_deref()).as_str()),
                "expected_rules_revision is stale or malformed"
            );
        }
        ensure!(
            dry_run || args.get("expected_rules_revision").is_some(),
            "apply requires expected_rules_revision from dry_run"
        );
        Ok(Self {
            board,
            project,
            rules,
            board_bytes,
            project_bytes,
            original,
            next,
            name,
            replaced,
            dry_run,
        })
    }

    fn check_sources(&self) -> Result<()> {
        ensure!(
            std::fs::read(&self.board)? == self.board_bytes,
            "saved board changed during validation"
        );
        ensure!(
            std::fs::read(&self.project)? == self.project_bytes,
            "project changed during validation"
        );
        Ok(())
    }
}

async fn drc(cli: &str, board: &std::path::Path) -> Result<Value> {
    let output = board.with_extension("drc.json");
    super::cli::run_cli(
        cli,
        &[
            "pcb",
            "drc",
            "--format",
            "json",
            "--severity-all",
            "--output",
            output.to_str().context("non-UTF8 report path")?,
            board.to_str().context("non-UTF8 board path")?,
        ],
        Duration::from_secs(600),
    )
    .await?;
    let raw: Value = serde_json::from_slice(&tokio::fs::read(&output).await?)?;
    ensure!(
        raw["kicad_version"]
            .as_str()
            .is_some_and(|v| v.starts_with("10.")),
        "rule compiler probe is supported for KiCad 10 only"
    );
    super::cli::parse_drc_report(&raw)?;
    Ok(raw)
}

async fn validate(snapshot: &Snapshot, cli: &str) -> Result<Value> {
    let dir = tempfile::tempdir()?;
    let board = dir.path().join(
        snapshot
            .board
            .file_name()
            .context("board filename is missing")?,
    );
    tokio::fs::write(&board, &snapshot.board_bytes).await?;
    tokio::fs::write(board.with_extension("kicad_pro"), &snapshot.project_bytes).await?;
    let rules = board.with_extension("kicad_dru");
    let probe = format!("KonnectValidation_{}", uuid::Uuid::new_v4().simple());
    // KiCad 10 can discard malformed rules and still exit 0. A real assertion
    // finding proves the entire candidate was compiled, not merely that DRC ran.
    let probed = format!(
        "{}\n(rule \"{probe}\" (severity error) (constraint assertion \"0\"))\n",
        snapshot.next
    );
    writer::write_new_atomic(&rules, &probed)?;
    let report = drc(cli, &board).await?;
    let probe_count = report["violations"]
        .as_array()
        .context("missing violations")?
        .iter()
        .filter(|v| {
            v["type"] == "assertion_failure"
                && v["description"]
                    .as_str()
                    .is_some_and(|d| d.contains(&probe))
        })
        .count();
    ensure!(probe_count > 0, "KiCad did not confirm custom-rule compilation: rules may be invalid, assertion checks disabled, or saved board empty. No target file was written");
    writer::write_atomic_if_unchanged(&rules, &probed, &snapshot.next)?;
    let report = drc(cli, &board).await?;
    ensure!(
        std::fs::read(&board)? == snapshot.board_bytes,
        "CLI changed the isolated board"
    );
    ensure!(
        std::fs::read_to_string(&rules)? == snapshot.next,
        "CLI changed the isolated rules"
    );
    snapshot.check_sources()?;
    ensure!(
        read_rules(&snapshot.rules)? == snapshot.original,
        "rules changed during validation"
    );
    Ok(json!({
        "compiler_confirmed": true, "compiler_probe_findings": probe_count,
        "kicad_version": report["kicad_version"],
        "source": "isolated_saved_copy", "schematic_parity_checked": false,
        "drc_report": report
    }))
}

fn result(
    body: Value,
    failed: bool,
    uncertain: bool,
    target: &str,
    completed: usize,
) -> CallToolResult {
    let mut result = CallToolResult::json(&body);
    result.is_error = failed;
    outcome::attach(
        result,
        outcome::summary(
            if uncertain {
                OutcomeStatus::Uncertain
            } else if failed {
                OutcomeStatus::Failed
            } else {
                OutcomeStatus::Complete
            },
            target,
            "saved_file",
            1,
            completed,
            1 - completed,
            if uncertain {
                Some(outcome::inspect_before_retry())
            } else if failed {
                Some(outcome::retry_whole_request())
            } else {
                None
            },
        ),
    )
}

pub(super) async fn handle(args: &Value, ctx: &ToolContext) -> Result<CallToolResult> {
    handle_with_observer(args, ctx, |_| {}).await
}

// The callback gives tests a deterministic external edit between publication
// and independent readback. Production does not change the just-written file.
async fn handle_with_observer(
    args: &Value,
    ctx: &ToolContext,
    after_publish: impl FnOnce(&std::path::Path) + Send,
) -> Result<CallToolResult> {
    let target = args["board"].as_str().unwrap_or("unresolved board");
    let refusal = |error: anyhow::Error| {
        result(
            json!({
                "status": "refused", "applied": 0,
                "error": ToolErrorKind::PlanBlocked { operation: "set_custom_design_rule".into(), reasons: vec![format!("{error:#}")] },
                "message": format!("{error:#}"),
            }),
            true,
            false,
            target,
            0,
        )
    };
    let snapshot = match Snapshot::prepare(args) {
        Ok(snapshot) => snapshot,
        Err(error) => return Ok(refusal(error)),
    };
    let validation = match validate(&snapshot, &ctx.config.kicad_cli).await {
        Ok(validation) => validation,
        Err(error) => return Ok(refusal(error)),
    };
    let changed = snapshot.original.as_deref() != Some(&snapshot.next);
    let mut applied = 0;
    let mut verified = false;
    if !snapshot.dry_run && changed {
        let write = match &snapshot.original {
            Some(original) => {
                writer::write_atomic_if_unchanged(&snapshot.rules, original, &snapshot.next)
            }
            None => writer::write_new_atomic(&snapshot.rules, &snapshot.next),
        };
        if let Err(error) = write {
            if read_rules(&snapshot.rules).ok().as_ref() == Some(&snapshot.original) {
                return Ok(refusal(error.into()));
            }
            return Ok(result(
                json!({
                    "status": "uncertain", "applied": null, "rules_file": snapshot.rules,
                    "message": format!("Rules publication was not verified: {error}. Inspect before retrying."),
                    "validation": validation,
                }),
                true,
                true,
                target,
                0,
            ));
        }
        applied = 1;
        after_publish(&snapshot.rules);
    }
    if !snapshot.dry_run {
        let readback = read_rules(&snapshot.rules);
        if !matches!(readback, Ok(Some(ref value)) if value == &snapshot.next)
            || snapshot.check_sources().is_err()
        {
            return Ok(result(
                json!({
                    "status": "uncertain", "applied": applied, "rules_file": snapshot.rules,
                    "message": "Rules or saved source readback changed. Inspect the rules and sources before retrying.",
                    "validation": validation,
                }),
                true,
                true,
                target,
                0,
            ));
        }
        verified = true;
    }
    Ok(result(
        json!({
            "status": if !changed { "noop" } else if snapshot.dry_run { "ready" } else { "applied" },
            "applied": applied, "dry_run": snapshot.dry_run, "source": "saved_file",
            "rules_file": snapshot.rules, "rule_name": snapshot.name, "replaced": snapshot.replaced,
            "rules_revision": revision(snapshot.original.as_deref()),
            "candidate_rules_revision": revision(Some(&snapshot.next)),
            "candidate_rules": snapshot.next, "readback_verified": verified,
            "live_editor_reloaded": false, "validation": validation,
        }),
        false,
        false,
        target,
        1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{mcp::handler::McpHandler, router::ToolRouter, tools::ServerConfig};
    use std::sync::Arc;

    const RULE: &str = "(rule \"one pad\" (condition \"A.memberOfFootprint('J1') && A.Pad_Number == '1'\") (constraint edge_clearance (min 0.05mm)))";
    const ORIGINAL: &str = "(version 1)\n# (rule \"one pad\" fake text)\n(rule \"other\" (constraint track_width (min 0.1mm)))\n# keep this comment\n";

    fn body(result: CallToolResult) -> Value {
        serde_json::from_value::<Value>(serde_json::to_value(result).unwrap()).unwrap()["content"]
            [0]["text"]
            .as_str()
            .map(|s| serde_json::from_str(s).unwrap())
            .unwrap()
    }

    fn fixture(dir: &std::path::Path) -> PathBuf {
        let board = dir.join("acceptance.kicad_pcb");
        std::fs::write(
            &board,
            include_bytes!("../../tests/fixtures/drc_ownership_j1.kicad_pcb"),
        )
        .unwrap();
        std::fs::write(
            board.with_extension("kicad_pro"),
            include_bytes!("../../tests/fixtures/multichannel_mixer.kicad_pro"),
        )
        .unwrap();
        board
    }

    fn context(cli: &str) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: cli.into(),
                ..Default::default()
            },
            Arc::new(ToolRouter::new()),
        )
    }

    #[test]
    fn replacement_preserves_comments_unrelated_bytes_and_priority() {
        let source =
            format!("{ORIGINAL}{RULE}\n(rule \"later\" (constraint edge_clearance (min 1mm)))\n");
        let definition = RULE.replace("0.05mm", "0.1mm");
        let (next, name, replaced) = candidate(Some(&source), &definition).unwrap();
        assert!(replaced);
        assert_eq!(name, "one pad");
        assert_eq!(
            next,
            format!(
                "{ORIGINAL}{definition}\n(rule \"later\" (constraint edge_clearance (min 1mm)))\n"
            )
        );
        assert_eq!(
            candidate(Some(ORIGINAL), RULE).unwrap().0,
            format!("{ORIGINAL}{RULE}\n")
        );
    }

    #[test]
    fn rejects_duplicate_names_trailing_garbage_headers_and_incomplete_strings() {
        for source in [
            format!("{ORIGINAL}{RULE}\n{RULE}\n"),
            format!("{ORIGINAL}garbage"),
            "(version 2)".into(),
            "(version 1) (version 1)".into(),
        ] {
            assert!(candidate(Some(&source), RULE).is_err(), "{source}");
        }
        for rule in [
            format!("{RULE} trailing"),
            format!("{RULE} {RULE}"),
            "(rule \"bad\" (condition \"unfinished))".into(),
            "(rule \"bad\")".into(),
            "(version 1)".into(),
        ] {
            assert!(candidate(Some(ORIGINAL), &rule).is_err(), "{rule}");
        }
        assert_ne!(revision(None), revision(Some("")));
        let quoted = "(rule \"x # ( ) \\\" y\" # comment\n (constraint assertion \"0\"))";
        assert!(candidate(None, quoted).is_ok());
        for duplicate in [
            "(rule x (condition \"0\") (condition \"1\") (constraint assertion \"0\"))",
            "(rule x (severity error) (severity ignore) (constraint assertion \"0\"))",
            "(rule x (layer F.Cu) (layer B.Cu) (constraint assertion \"0\"))",
            "(rule x (constraint edge_clearance (min 0.1mm)) (constraint edge_clearance (min 0.2mm)))",
        ] {
            assert!(candidate(None, duplicate).is_err(), "{duplicate}");
        }
    }

    #[cfg(unix)]
    fn mock_cli(dir: &std::path::Path, edit_rules: bool) -> PathBuf {
        let side_effect = if edit_rules {
            format!(
                "printf '%s\\n' '# external change' >> '{}'\n",
                dir.join("acceptance.kicad_dru").display()
            )
        } else {
            String::new()
        };
        super::super::cli::test_support::write_script(
            dir,
            "rules-cli",
            &format!(
                r##"#!/bin/sh
for last; do :; done
rules="${{last%.*}}.kicad_dru"
out="${{last%.*}}.drc.json"
probe=$(sed -n 's/.*(rule "\(KonnectValidation_[^"]*\)".*/\1/p' "$rules")
{side_effect}if [ -n "$probe" ] && ! grep -q INVALID_RULE "$rules"; then
  printf '{{"kicad_version":"10.0.6","violations":[{{"type":"assertion_failure","severity":"error","description":"%s","items":[]}}],"unconnected_items":[],"schematic_parity":[]}}' "$probe" > "$out"
else
  printf '%s' '{{"kicad_version":"10.0.6","violations":[],"unconnected_items":[],"schematic_parity":[]}}' > "$out"
fi
"##
            ),
            "",
        )
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn served_dry_run_apply_and_bad_rule_leave_board_project_unmodified() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture(dir.path());
        let rules = board.with_extension("kicad_dru");
        std::fs::write(&rules, ORIGINAL).unwrap();
        let cli = mock_cli(dir.path(), false);
        let handler = McpHandler::new(ServerConfig {
            kicad_cli: cli.to_str().unwrap().into(),
            eager_toolsets: true,
            ..Default::default()
        })
        .await
        .unwrap();
        async fn call(handler: &McpHandler, args: Value) -> Value {
            let response = handler.handle_message(json!({"jsonrpc":"2.0", "id": 1, "method":"tools/call", "params":{"name":"set_custom_design_rule", "arguments":args}})).await.unwrap();
            serde_json::from_str(
                response.result.unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap()
        }
        let plan = call(&handler, json!({"board":board,"rule":RULE})).await;
        assert_eq!(plan["status"], "ready");
        assert_eq!(plan["applied"], 0);
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), ORIGINAL);
        let apply_args = json!({"board":board,"rule":RULE,"dry_run":false,"expected_rules_revision":plan["rules_revision"]});
        let applied = call(&handler, apply_args.clone()).await;
        assert_eq!(applied["status"], "applied");
        assert_eq!(applied["readback_verified"], true);
        assert_eq!(applied["applied"], 1);
        assert_eq!(applied["outcome"]["status"], "complete");
        let saved = std::fs::read_to_string(&rules).unwrap();
        assert_eq!(saved, format!("{ORIGINAL}{RULE}\n"));
        let stale = call(&handler, apply_args).await;
        assert_eq!(stale["status"], "refused");
        let bad = call(&handler, json!({"board":board,"rule":RULE.replace("0.05mm", "INVALID_RULE"),"dry_run":false,"expected_rules_revision":revision(Some(&saved))})).await;
        assert_eq!(bad["status"], "refused");
        assert_eq!(bad["applied"], 0);
        assert_eq!(std::fs::read_to_string(&rules).unwrap(), saved);
        assert_eq!(
            std::fs::read(&board).unwrap(),
            include_bytes!("../../tests/fixtures/drc_ownership_j1.kicad_pcb")
        );
        assert_eq!(
            std::fs::read(board.with_extension("kicad_pro")).unwrap(),
            include_bytes!("../../tests/fixtures/multichannel_mixer.kicad_pro")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_validation_edit_is_refused_and_readback_loss_is_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let board = fixture(dir.path());
        let rules = board.with_extension("kicad_dru");
        std::fs::write(&rules, ORIGINAL).unwrap();
        let cli = mock_cli(dir.path(), true);
        let args = json!({"board":board,"rule":RULE,"dry_run":false,"expected_rules_revision":revision(Some(ORIGINAL))});
        let refused = body(
            handle(&args, &context(cli.to_str().unwrap()))
                .await
                .unwrap(),
        );
        assert_eq!(refused["status"], "refused");
        assert_eq!(refused["applied"], 0);
        assert!(std::fs::read_to_string(&rules)
            .unwrap()
            .starts_with(ORIGINAL));
        assert!(!std::fs::read_to_string(&rules).unwrap().contains(RULE));
        std::fs::write(&rules, ORIGINAL).unwrap();
        let cli = mock_cli(dir.path(), false);
        let uncertain = body(
            handle_with_observer(&args, &context(cli.to_str().unwrap()), |path| {
                std::fs::write(path, "# external replacement\n").unwrap();
            })
            .await
            .unwrap(),
        );
        assert_eq!(uncertain["status"], "uncertain");
        assert_eq!(uncertain["applied"], 1);
        assert_eq!(uncertain["outcome"]["status"], "uncertain");
        assert_eq!(uncertain["outcome"]["retry"]["safe"], false);
        assert_eq!(
            std::fs::read_to_string(&rules).unwrap(),
            "# external replacement\n"
        );
    }

    #[tokio::test]
    #[ignore = "requires KiCad 10 CLI; set KONNECT_TEST_KICAD_CLI"]
    async fn real_kicad_compiles_rejects_bad_rules_and_changes_only_selected_pad() {
        let cli = std::env::var("KONNECT_TEST_KICAD_CLI").expect("set KONNECT_TEST_KICAD_CLI");
        let dir = tempfile::tempdir().unwrap();
        let board = fixture(dir.path());
        let rules = board.with_extension("kicad_dru");
        std::fs::write(&rules, ORIGINAL).unwrap();
        let baseline = drc(&cli, &board).await.unwrap();
        let ctx = context(&cli);
        let args = json!({"board":board,"rule":RULE,"dry_run":false,"expected_rules_revision":revision(Some(ORIGINAL))});
        let applied = body(handle(&args, &ctx).await.unwrap());
        assert_eq!(applied["status"], "applied", "{applied}");
        let report = drc(&cli, &board).await.unwrap();
        let edge = |r: &Value| -> Vec<Vec<String>> {
            r["violations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v["type"] == "copper_edge_clearance")
                .map(|v| {
                    v["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|item| item["uuid"].as_str().unwrap().to_string())
                        .collect()
                })
                .collect()
        };
        let mut expected = edge(&baseline);
        assert_eq!(expected.len(), 4);
        // KiCad-authored pad 1 UUID; independent of our rule planner.
        let pad1 = "5bc25fc3-1886-4e08-a602-7e08cc66255e";
        assert_eq!(
            expected
                .iter()
                .filter(|ids| ids.iter().any(|id| id == pad1))
                .count(),
            1,
            "{expected:?}"
        );
        expected.retain(|ids| !ids.iter().any(|id| id == pad1));
        assert_eq!(edge(&report), expected);
        let saved = std::fs::read_to_string(&rules).unwrap();
        for bad in [
            RULE.replace("A.Pad_Number", "A.Bogus_Property"),
            RULE.replace("edge_clearance", "copper_edge_clearance"),
            RULE.replace("== '1'", "=="),
            RULE.replace("0.05mm", "nonsense"),
        ] {
            let response = body(handle(&json!({"board":board,"rule":bad,"dry_run":false,"expected_rules_revision":revision(Some(&saved))}), &ctx).await.unwrap());
            assert_eq!(response["status"], "refused", "{response}");
            assert_eq!(response["applied"], 0);
            assert_eq!(std::fs::read_to_string(&rules).unwrap(), saved);
        }
        assert_eq!(
            std::fs::read(&board).unwrap(),
            include_bytes!("../../tests/fixtures/drc_ownership_j1.kicad_pcb")
        );
        assert_eq!(
            std::fs::read(board.with_extension("kicad_pro")).unwrap(),
            include_bytes!("../../tests/fixtures/multichannel_mixer.kicad_pro")
        );
    }
}
