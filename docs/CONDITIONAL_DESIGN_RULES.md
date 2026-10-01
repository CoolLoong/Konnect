# Conditional DRC rules

`verification.set_custom_design_rule` upserts one complete KiCad rule definition.
For example, keep board-level copper edge clearance at 0.5 mm and select only
the shell pads of one connector:

```json
{
  "board": "/absolute/path/design.kicad_pcb",
  "rule": "(rule \"J7 shell edge clearance\" (condition \"A.memberOfFootprint('J7') && A.Pad_Number == 'SH'\") (constraint edge_clearance (min 0.25mm)))",
  "dry_run": true
}
```

Use the returned `rules_revision` as `expected_rules_revision` with
`dry_run: false` to apply. A revision distinguishes missing and empty files.
The handler refuses stale revisions, duplicate existing names, malformed or
extra top-level input, missing/invalid project JSON and unverifiable CLI output.
It replaces a matching rule in place, or appends a new rule. All other bytes,
including comments and rule order, remain intact; later rules have priority.
The PCB and project settings are never written.

KiCad 10.0.6 may silently discard invalid custom rules and run DRC with implicit
rules, exiting successfully. Therefore validation uses two runs on an isolated
copy of the saved board and project:

1. Append a unique failing assertion to the entire candidate rules file. An
   actual `assertion_failure` with that unique name proves compilation and loading.
   Absent proof is a refusal, including empty boards or disabled assertion checks.
2. Remove the temporary assertion and run ordinary DRC on the actual candidate.
   Return its complete report separately from compilation success. Existing DRC
   findings do not prevent a syntactically valid rule from being published.

Validation reads saved state, omits schematic parity, and does not refill zones.
Rule writes use the shared locked atomic writer with compare-and-swap (or
create-only publication), followed by independent rules and source readback.
Preflight/validation failures apply nothing. Unverified publication is reported
as `uncertain`; inspect the target before retrying. No automatic rollback hides
an external edit. The live editor is not reloaded: a caller must explicitly
reload custom rules before relying on them for interactive routing.

The condition and `edge_clearance` spelling follow the official
[KiCad 10 PCB Editor manual](https://docs.kicad.org/10.0/en/pcbnew/pcbnew.html#custom-design-rules).
The fallback behavior and full-file compilation guard are grounded in
[KiCad 10.0.6 DRC_ENGINE::InitEngine](https://github.com/KiCad/kicad/blob/10.0.6/pcbnew/drc/drc_engine.cpp)
and verified with that installed CLI. This guard is intentionally limited to
reports identifying KiCad 10. Other versions require separate evidence.

This tool is distinct from `add_design_rule`, which stores natural-language
preferences, and `set_layer_constraints`, which exposes two simple layer limits.
