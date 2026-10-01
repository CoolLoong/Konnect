# Typed footprint refresh property profile

`update_footprints_from_library` supports KiCad 10's `(unlocked yes|no)` on
mandatory/custom properties and `fp_text user`. `yes` means
`TextAttributes.keep_upright=false`, not an editing lock. Unlocked custom text
keeps its authored angle, transformed for board rotation and side; locked-to-
upright text keeps the existing readable-angle behavior. Placed mandatory field
presentation is retained unless the library explicitly supplies `unlocked`.

Per-pad `(solder_mask_margin <mm>)` maps to both front and back padstack solder
mask overrides. Values must be finite and fit signed 32-bit nanometres. Explicit
zero is an override distinct from an omitted/inherited value. Physical pad order
keeps different margins on pads with duplicate logical numbers. Pad nets remain
matched by logical number. Repeated or malformed clauses refuse before writing.
Other unmodeled clauses still refuse; this does not broaden the tool to arbitrary
footprint attributes or custom pad geometry.

For schematic-linked footprints, placed BOM exclusion is retained so a library
refresh cannot undo schematic synchronization. Board-only footprints still take
the library BOM flag. This changes the previous library-overwrite behavior.

Arguments, filters and default dry run remain unchanged. Apply requires a current
`expected_plan_revision`, uses one IPC undo commit, and never saves automatically.
A fresh GetItems readback checks library domains, mandatory field presentation,
position, rotation, side, edit lock, identity, sheet path, pad nets and overrides.
`coverage.changed.applied` counts independently verified footprints. Native child
UUIDs and parent IDs are normalized only where KiCad assigns them.

Any failure after a possible write returns `isError=true`, `status="uncertain"`
and `error.kind="mutation_outcome_uncertain"`. Zero confirmed applied counts do
not prove rollback. Inspect the board before retrying; use Ctrl-Z only after
checking whether the commit completed. Unsupported inputs and stale revisions
still refuse before mutation.
