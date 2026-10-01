# Saved schematic annotations in PCB sync

`update_pcb_from_schematic` reads the saved schematic hierarchy and a fresh
KiCad CLI netlist, and writes only the exact requested open board through IPC.
Save schematic changes before planning. `in_bom=no` sets the footprint's BOM
exclusion; only `on_board=no` suppresses board inclusion. BOM-excluded components
with no footprint are still reported as unassigned.

Exported Datasheet, Description and custom field values, including empty values,
are synchronized. Reference, Value and Footprint retain their existing dedicated
paths. Field-only differences cause an update and invalidate the plan revision.
Existing custom field presentation and unrelated board-only fields are preserved;
new custom fields are hidden, with independent native text UUIDs. Duplicate
exported/custom names refuse before mutation.

Arguments and dry-run defaults are unchanged. Plan changes add `schematic_fields`
(`exclude_from_bom`, `fields`). Coverage adds `fields_synchronized`, counting
requested field names on changed footprints. Its applied value is counted from
independent IPC readback, including preserved board-only annotations. BOM-only
updates have zero field names but still update the footprint.

Apply still requires the revision of a current dry run, uses one KiCad undo
commit and never saves automatically. If the write or subsequent readback cannot
be confirmed, the response has `isError=true`, `status="uncertain"`,
`error.kind="mutation_outcome_uncertain"`, and no confirmed applied counts.
A zero confirmed count does not establish rollback. Inspect the live board before
retrying; Ctrl-Z can reverse a completed update after inspecting its state.
