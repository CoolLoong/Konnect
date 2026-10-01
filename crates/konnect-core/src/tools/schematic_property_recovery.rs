//! Recover only the exact duplicated-header signature of legacy property insertion.
use super::{get_path, schematic_property_integrity, ToolContext, ToolDef};
use crate::{
    mcp::protocol::CallToolResult,
    outcome::{self, OutcomeStatus},
    tool,
};
use anyhow::{ensure, Context, Result};
use konnect_sexp::{
    writer::{apply_edits, read_consistent, write_atomic_if_unchanged, write_new_atomic, SexpEdit},
    SexpNode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(super) fn tool() -> ToolDef {
    tool!(
        "repair_schematic_property_prefixes",
        "Recover ONLY the legacy compact-symbol custom-property insertion bug: the exact \
         same placed-symbol header repeated on each of the nine generated property lines. \
         Explicitly identify each reference, symbol UUID, field name and retained value. \
         Removes only those proven duplicate prefixes; retains existing fields, all other \
         bytes, UUIDs, placement and wiring. Rejects any mismatched, missing, ambiguous or \
         additional damage. Defaults to dry_run with source/candidate hashes and deletion \
         spans; apply requires the exact returned plan_revision. Creates an immutable bad \
         preimage backup, then uses the shared atomic compare-and-swap writer and reads the \
         full file back. Refuses a locked/open schematic. Does not restore a file copy, save \
         or reload editors. Unexpected post-write state is uncertain; inspect before retry.",
        json!({"type":"object","additionalProperties":false,"properties":{
            "schematic":{"type":"string"},
            "repairs":{"type":"array","minItems":1,"maxItems":64,"items":{"type":"object","additionalProperties":false,"properties":{
                "reference":{"type":"string","minLength":1},
                "symbol_uuid":{"type":"string","format":"uuid"},
                "field_name":{"type":"string","minLength":1},
                "value":{"type":"string"}
            },"required":["reference","symbol_uuid","field_name","value"]}},
            "dry_run":{"type":"boolean","default":true},
            "expected_plan_revision":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"Required for apply: exact plan_revision from this file and these proofs' latest dry_run"}
        },"required":["schematic","repairs"]}),
        |args, ctx| async move { handle(args, ctx).await }
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Proof {
    reference: String,
    symbol_uuid: String,
    field_name: String,
    value: String,
}

#[derive(Debug, Clone, Serialize)]
struct Removal {
    start: usize,
    end: usize,
}

fn quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn legacy_insertion(prefix: &str, proof: &Proof, x: f64, y: f64, newline: &str) -> String {
    format!("{newline}{prefix}(property \"{}\" \"{}\"{newline}{prefix}\t(at {x} {y} 0){newline}{prefix}\t(hide yes){newline}{prefix}\t(effects{newline}{prefix}\t\t(font{newline}{prefix}\t\t\t(size 1.27 1.27){newline}{prefix}\t\t){newline}{prefix}\t){newline}{prefix})",quote(&proof.field_name),quote(&proof.value))
}

fn candidate(source: &str, proofs: &[Proof]) -> Result<(String, Vec<Removal>)> {
    ensure!(
        !proofs.is_empty() && proofs.len() <= 64,
        "requires 1..64 exact repair proofs"
    );
    let mut seen = BTreeSet::new();
    let mut removals = Vec::new();
    for proof in proofs {
        ensure!(
            uuid::Uuid::parse_str(&proof.symbol_uuid)?.to_string() == proof.symbol_uuid,
            "symbol UUID is not canonical"
        );
        ensure!(
            !proof.reference.is_empty() && !proof.field_name.is_empty(),
            "empty repair identity"
        );
        ensure!(
            !["Reference", "Value", "Footprint", "Datasheet"].contains(&proof.field_name.as_str()),
            "only newly created custom fields can have this legacy signature"
        );
        ensure!(
            seen.insert((&proof.symbol_uuid, &proof.field_name)),
            "duplicate repair proof"
        );
        let needle = format!(
            "(property \"{}\" \"{}\"",
            quote(&proof.field_name),
            quote(&proof.value)
        );
        let matches = source.match_indices(&needle).collect::<Vec<_>>();
        let mut matched = None;
        for (at, _) in matches {
            let line = source[..at].rfind('\n').map_or(0, |pos| pos + 1);
            let prefix = &source[line..at];
            if !prefix.starts_with("(symbol (lib_id ") || !prefix.ends_with(' ') {
                continue;
            }
            let header_source = format!("{prefix})");
            let Ok(header) = konnect_sexp::parse_sexp(&header_source) else {
                continue;
            };
            if header.head() != Some("symbol")
                || header.find_str("uuid") != Some(proof.symbol_uuid.as_str())
                || !header.find_all("property").is_empty()
            {
                continue;
            }
            ensure!(matched.is_none(), "ambiguous generated property signature");
            let placement = header.find("at").context("header has no placement")?;
            let x = placement.get_f64(1).context("invalid placement x")?;
            let y = placement.get_f64(2).context("invalid placement y")?;
            ensure!(x.is_finite() && y.is_finite(), "nonfinite placement");
            let newline = if source[..line].ends_with("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
            let bad = legacy_insertion(prefix, proof, x, y, newline);
            let start = line
                .checked_sub(newline.len())
                .context("generated insertion has no initial newline")?;
            ensure!(
                source[start..].starts_with(&bad),
                "generated property body differs from the exact legacy signature"
            );
            ensure!(source.match_indices(prefix).count()==10,"header must occur exactly once originally and nine times in this generated property");
            for (offset, _) in bad.match_indices(prefix) {
                removals.push(Removal {
                    start: start + offset,
                    end: start + offset + prefix.len(),
                });
            }
            matched = Some(());
        }
        ensure!(
            matched.is_some(),
            "no exact legacy signature for {}/{}",
            proof.reference,
            proof.field_name
        );
    }
    removals.sort_by_key(|span| span.start);
    ensure!(
        removals.len() == 9 * proofs.len()
            && removals.windows(2).all(|pair| pair[0].end <= pair[1].start),
        "overlapping or incomplete deletion proof"
    );
    let next = apply_edits(
        source.into(),
        removals
            .iter()
            .map(|span| SexpEdit::delete(span.start, span.end))
            .collect(),
    );
    let tree = schematic_property_integrity::parse(&next)?;
    let mut ids = BTreeSet::new();
    for symbol in tree.find_all("symbol") {
        ensure!(
            ids.insert(
                symbol
                    .find_str("uuid")
                    .context("placed symbol has no UUID")?
            ),
            "duplicate placed symbol UUID after repair"
        );
    }
    for proof in proofs {
        let symbols = tree
            .find_all("symbol")
            .into_iter()
            .filter(|symbol| symbol.find_str("uuid") == Some(proof.symbol_uuid.as_str()))
            .collect::<Vec<_>>();
        ensure!(symbols.len() == 1, "repaired symbol UUID is not unique");
        let properties = symbols[0].find_all("property");
        ensure!(
            properties
                .iter()
                .any(|p| p.get(1).and_then(SexpNode::as_str) == Some("Reference")
                    && p.get(2).and_then(SexpNode::as_str) == Some(proof.reference.as_str())),
            "repaired UUID belongs to another reference"
        );
        ensure!(
            properties
                .iter()
                .any(
                    |p| p.get(1).and_then(SexpNode::as_str) == Some(proof.field_name.as_str())
                        && p.get(2).and_then(SexpNode::as_str) == Some(proof.value.as_str())
                ),
            "retained property differs"
        );
    }
    Ok((next, removals))
}

fn sha(source: &str) -> String {
    format!("{:x}", Sha256::digest(source.as_bytes()))
}
fn revision(path: &std::path::Path, source: &str, proofs: &[Proof]) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(path, source, proofs)).expect("serializable plan"))
    )
}

fn result(body: Value, target: &str, status: OutcomeStatus) -> CallToolResult {
    let complete = status == OutcomeStatus::Complete;
    let mut result = CallToolResult::json(&body);
    result.is_error = !complete;
    outcome::attach(
        result,
        outcome::summary(
            status,
            target,
            "saved_file",
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

pub(super) async fn handle(args: &Value, _ctx: &ToolContext) -> Result<CallToolResult> {
    handle_observed(args, |_| {}).await
}

async fn handle_observed(
    args: &Value,
    after_write: impl FnOnce(&std::path::Path) + Send,
) -> Result<CallToolResult> {
    let target = args["schematic"].as_str().unwrap_or("unresolved schematic");
    let refused = |reason: String| {
        result(
            json!({"status":"conflict","applied":false,"repaired_symbols":0,"reason":reason}),
            target,
            OutcomeStatus::Failed,
        )
    };
    let path = get_path(args, "schematic")?;
    let proofs: Vec<Proof> = match serde_json::from_value(args["repairs"].clone()) {
        Ok(p) => p,
        Err(e) => return Ok(refused(e.to_string())),
    };
    let source = match read_consistent(&path) {
        Ok(source) => source,
        Err(e) => return Ok(refused(e.to_string())),
    };
    let (next, spans) = match candidate(&source, &proofs) {
        Ok(c) => c,
        Err(e) => return Ok(refused(format!("{e:#}"))),
    };
    let rev = revision(&path, &source, &proofs);
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let mut body = json!({"status":"ready","dry_run":dry_run,"applied":false,"plan_revision":rev,"source_sha256":sha(&source),"candidate_sha256":sha(&next),"repairs":proofs,"duplicate_prefix_deletions":spans,"all_other_bytes_preserved":true,"structure_verified":true,"repaired_symbols":0});
    if !dry_run {
        if args["expected_plan_revision"].as_str() != Some(rev.as_str()) {
            return Ok(refused(
                "stale_or_missing_plan_revision: rerun dry_run for this exact source and proofs"
                    .into(),
            ));
        }
        let backup = path.with_file_name(format!(
            ".{}.konnect-property-recovery-{}.preimage",
            path.file_name()
                .context("missing filename")?
                .to_string_lossy(),
            sha(&source)
        ));
        let backup_result = if backup.exists() {
            ensure!(
                !std::fs::symlink_metadata(&backup)?.file_type().is_symlink(),
                "preimage backup is a symlink"
            );
            read_consistent(&backup).and_then(|saved| {
                if saved == source {
                    Ok(())
                } else {
                    Err(konnect_sexp::SexpError::InvalidValue(
                        "existing preimage backup differs".into(),
                    ))
                }
            })
        } else {
            write_new_atomic(&backup, &source)
        };
        if let Err(e) = backup_result {
            return Ok(refused(format!("preimage backup failed: {e}")));
        }
        if let Err(e) = write_atomic_if_unchanged(&path, &source, &next) {
            if read_consistent(&path).is_ok_and(|actual| actual == source) {
                return Ok(refused(format!(
                    "atomic recovery refused before replacement: {e}"
                )));
            }
            body["status"] = json!("uncertain");
            body["potentially_applied"] = json!(true);
            body["all_other_bytes_preserved"] = Value::Null;
            body["structure_verified"] = json!(false);
            body["readback_verified"] = json!(false);
            body["preimage_backup"] = json!(backup);
            body["reason"] = json!(format!("atomic writer failed and original source is no longer independently confirmed: {e}"));
            return Ok(result(body, target, OutcomeStatus::Uncertain));
        }
        after_write(&path);
        let verified = read_consistent(&path).and_then(|actual| {
            if actual != next {
                return Err(konnect_sexp::SexpError::InvalidValue(
                    "independent recovery readback differs".into(),
                ));
            }
            schematic_property_integrity::parse(&actual)
                .map(|_| ())
                .map_err(|e| konnect_sexp::SexpError::InvalidValue(e.to_string()))
        });
        body["preimage_backup"] = json!(backup);
        if let Err(e) = verified {
            body["status"] = json!("uncertain");
            body["potentially_applied"] = json!(true);
            body["all_other_bytes_preserved"] = Value::Null;
            body["structure_verified"] = json!(false);
            body["readback_verified"] = json!(false);
            body["reason"] = json!(e.to_string());
            return Ok(result(body, target, OutcomeStatus::Uncertain));
        }
        body["status"] = json!("applied");
        body["applied"] = json!(true);
        body["repaired_symbols"] = json!(proofs.len());
        body["readback_verified"] = json!(true);
    }
    Ok(result(body, target, OutcomeStatus::Complete))
}

#[cfg(test)]
mod tests {
    use super::*;
    const AUTHORED: &str = include_str!("../../tests/fixtures/ecc83_multiunit.kicad_sch");

    fn fixture() -> (String, String, Vec<Proof>) {
        let clean = AUTHORED
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(" ");
        let blocks = super::super::find_all_symbol_instance_blocks(&clean, "U1");
        let mut edits = Vec::new();
        let mut proofs = Vec::new();
        for (start, end) in blocks {
            let block = &clean[start..end];
            let prefix = &block[..block.find("(property ").unwrap()];
            let tree = konnect_sexp::parse_sexp(block).unwrap();
            let at = tree.find("at").unwrap();
            let proof = Proof {
                reference: "U1".into(),
                symbol_uuid: tree.find_str("uuid").unwrap().into(),
                field_name: "Manufacturer".into(),
                value: "Maker \"quotes\" \\path".into(),
            };
            // Independent transcription of the released writer's nine lines,
            // applied to a formatting-only KiCad-authored fixture variant.
            let lines = vec![
                format!("(property \"Manufacturer\" \"{}\"", quote(&proof.value)),
                format!(
                    "\t(at {} {} 0)",
                    at.get_f64(1).unwrap(),
                    at.get_f64(2).unwrap()
                ),
                "\t(hide yes)".into(),
                "\t(effects".into(),
                "\t\t(font".into(),
                "\t\t\t(size 1.27 1.27)".into(),
                "\t\t)".into(),
                "\t)".into(),
                ")".into(),
            ];
            let bad = format!(
                "\n{}",
                lines
                    .iter()
                    .map(|line| format!("{prefix}{line}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            edits.push(SexpEdit::insert(end - 1, bad));
            proofs.push(proof);
        }
        let damaged = apply_edits(clean.clone(), edits);
        assert!(schematic_property_integrity::parse(&damaged).is_err());
        (clean, damaged, proofs)
    }

    #[test]
    fn exact_signature_recovers_all_units_and_retains_every_unrelated_object() {
        let (clean, damaged, proofs) = fixture();
        for source in [&damaged, &damaged.replace('\n', "\r\n")] {
            let (next, spans) = candidate(source, &proofs).unwrap();
            assert_eq!(spans.len(), 27);
            let changed = json!({"reference":"U1","fields":[{"name":"Manufacturer","value":proofs[0].value,"updated_units":0,"created_units":3}]});
            schematic_property_integrity::verify(&clean, &next, &[changed]).unwrap();
            assert!(next.contains("(wire "));
            assert!(candidate(&next, &proofs).is_err());
        }
    }

    #[test]
    fn refuses_wrong_proofs_inexact_signature_extra_damage_and_missing_coverage() {
        let (_, damaged, proofs) = fixture();
        for field in ["reference", "symbol_uuid", "value", "field_name"] {
            let mut bad = proofs.clone();
            match field {
                "reference" => bad[0].reference = "OTHER".into(),
                "symbol_uuid" => bad[0].symbol_uuid = uuid::Uuid::new_v4().to_string(),
                "value" => bad[0].value = "OTHER".into(),
                _ => bad[0].field_name = "OTHER".into(),
            }
            assert!(candidate(&damaged, &bad).is_err(), "{field}");
        }
        // Change the generated block's signature, not a retained library font.
        let start = damaged.find("(property \"Manufacturer\"").unwrap();
        let inexact = format!(
            "{}{}",
            &damaged[..start],
            damaged[start..].replacen("(size 1.27 1.27)", "(size 2.27 1.27)", 1)
        );
        assert!(candidate(&inexact, &proofs).is_err());
        assert!(candidate(&format!("{damaged} trailing"), &proofs).is_err());
        assert!(candidate(&damaged, &proofs[..1]).is_err());
        let mut duplicate = proofs.clone();
        duplicate.push(proofs[0].clone());
        assert!(candidate(&damaged, &duplicate).is_err());
    }

    fn context() -> ToolContext {
        ToolContext::new(
            super::super::ServerConfig::default(),
            std::sync::Arc::new(crate::router::ToolRouter::new()),
        )
    }
    fn body(result: CallToolResult) -> Value {
        let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else {
            panic!("text")
        };
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn served_dry_run_apply_stale_revision_and_preimage_are_verified() {
        let (_, damaged, proofs) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recover.kicad_sch");
        std::fs::write(&path, &damaged).unwrap();
        let handler = crate::mcp::handler::McpHandler::new(super::super::ServerConfig {
            eager_toolsets: true,
            ..Default::default()
        })
        .await
        .unwrap();
        async fn call(handler: &crate::mcp::handler::McpHandler, args: Value) -> Value {
            let response=handler.handle_message(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"repair_schematic_property_prefixes","arguments":args}})).await.unwrap();
            serde_json::from_str(
                response.result.unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap()
        }
        let mut args = json!({"schematic":path,"repairs":proofs});
        let dry = call(&handler, args.clone()).await;
        assert_eq!(dry["status"], "ready");
        assert_eq!(dry["outcome"]["status"], "complete");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), damaged);
        args["dry_run"] = json!(false);
        args["expected_plan_revision"] = json!("0".repeat(64));
        assert_eq!(call(&handler, args.clone()).await["status"], "conflict");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), damaged);
        args["expected_plan_revision"] = dry["plan_revision"].clone();
        let applied = call(&handler, args.clone()).await;
        assert_eq!(applied["status"], "applied", "{applied}");
        assert_eq!(applied["readback_verified"], true);
        assert_eq!(
            std::fs::read_to_string(applied["preimage_backup"].as_str().unwrap()).unwrap(),
            damaged
        );
        assert_eq!(
            sha(&std::fs::read_to_string(&path).unwrap()),
            dry["candidate_sha256"].as_str().unwrap()
        );
        assert_eq!(call(&handler, args).await["status"], "conflict");
    }

    #[tokio::test]
    async fn locked_schematic_and_post_write_loss_do_not_report_success() {
        let (_, damaged, proofs) = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recover.kicad_sch");
        std::fs::write(&path, &damaged).unwrap();
        let mut args = json!({"schematic":path,"repairs":proofs});
        let dry = body(handle(&args, &context()).await.unwrap());
        args["dry_run"] = json!(false);
        args["expected_plan_revision"] = dry["plan_revision"].clone();
        let lock = konnect_sexp::writer::kicad_editor_lock_path(&path).unwrap();
        std::fs::write(&lock, "{}").unwrap();
        let blocked = body(handle(&args, &context()).await.unwrap());
        assert_eq!(blocked["status"], "conflict");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), damaged);
        std::fs::remove_file(lock).unwrap();
        let uncertain = body(
            handle_observed(&args, |path| {
                let text = std::fs::read_to_string(path).unwrap();
                std::fs::write(path, text + " ").unwrap();
            })
            .await
            .unwrap(),
        );
        assert_eq!(uncertain["status"], "uncertain");
        assert_eq!(uncertain["repaired_symbols"], 0);
        assert_eq!(uncertain["outcome"]["retry"]["safe"], false);
    }
}
