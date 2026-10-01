# Reviewed saved-board stackup changes

`set_stackup` (`pcb_board`) changes only a physical stackup and, if requested,
the nominal `general/thickness`. The live native setter is not implemented in
KiCad 10. This tool requires `confirm_closed_saved: true` after saving and closing
the exact PCB, no editor lock, and safe IPC editor-absence evidence. It refuses
live boards and inconclusive/lost-session evidence. After clean closure a fresh
MCP connection may be needed if the existing connection observed that live board.
Do not use the acknowledgment to bypass an open editor or unsaved changes.

Give the complete physical order using canonical names: enabled top silk, paste,
mask; F.Cu; dielectric 1; inner copper/dielectric pairs; B.Cu; enabled bottom mask,
paste, silk. Each layer has `name`, `kind`, `thickness_mm`, and optional `material`,
`epsilon_r`, `loss_tangent`. Kinds are copper, core, prepreg, soldermask,
silkscreen, solderpaste. Silk/paste may have zero thickness. The copper layer
count/table/order cannot change. Existing unrequested stackup properties, layer
colors, and all other source bytes/structures are preserved. Unsupported duplicate
properties or dielectric sublayer arrangements are refused rather than flattened.
A previously implicit default becomes an explicit stackup; the preview accurately
reports the absence of a saved stackup before that edit.

`dry_run` defaults true. The exact saved source and request produce a
`plan_revision`; apply requires that revision as `expected_plan_revision`.
Before any publish the candidate must reopen in real KiCad CLI and its exported
IPC-2581 physical layers, material, thickness and requested dielectric properties
must agree. The saved file is atomically replaced only with the same original
source and no editor lock, keeping a content-addressed immutable preimage. An
independent whole-file read and a second CLI reopen/readback verify the result.
The file contains the exact requested values; XML comparison tolerances disclose
KiCad export precision (six significant digits for layer thickness, two decimal
places for epsilon, three significant digits for loss tangent). The XML overallThickness is checked against the sum of all physical layers,
including masks. Nominal general/thickness is verified separately in the exact
saved source. Export precision is disclosed.

There is no GUI undo for saved-file edits. A post-write/readback failure reports
`uncertain`, zero confirmed applied stackups, the preimage, and inspection required
before retry; it never restores over subsequent edits or reopens a stale editor.
After success reopen the saved PCB in KiCad. `get_board_stackup` remains the
independent live getter for final inspection in the reopened editor.

The tool does not select a manufacturer stack, compute impedance, alter routed
widths/gaps, or certify manufacturing readiness. Those require their own evidence.
