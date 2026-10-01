# Explicit equivalent schematic identity relink

Ordinary `update_pcb_from_schematic` remains strict: a reference whose saved
symbol UUID differs from its PCB association is a conflict, even when its
reference and footprint happen to match. It never automatically adopts that
new identity.

`relink_pcb_footprint_to_schematic` provides a separate explicit operation for
an intentional UUID replacement. Supply the saved root schematic, exact open
board, reference, existing footprint UUID, exact old full symbol path, and new
exported full symbol instance path. Paths use canonical UUID segments; complete
hierarchical paths are required. Rooted and root-relative paths are validated
against the saved export; only the exact specified new path is written.

The tool strictly validates the complete saved hierarchy, exports it with
KiCad CLI, rejects the old identity if it remains in the schematic, rejects
ambiguous references/paths/UUIDs or an already-associated new path, and requires
the same assigned footprint. Every physical pad, including repeated logical
pad numbers, must retain its saved schematic net; all exported connected pins
must have physical pads. Named live nets must have unique positive codes.
Unmodeled target/pad protobuf data is refused rather than discarded.

Default dry run returns the exact mapping, number of checked physical pads and
raw live items, hierarchy coverage and `plan_revision`. Revision covers mapping,
all exact hierarchy bytes/paths, native document identity, all raw PCB items and
nets. Apply requires that revision and rechecks source and live state inside one
KiCad undo commit immediately before the write. It changes only `symbol_path`.
Full independent raw board and net readback must equal the identity-only expected
snapshot, and hierarchy bytes must remain unchanged. It never refreshes a
footprint, changes fields/pads/placement/routing, saves a board or restarts an
editor. Any possible write followed by failed verification is `uncertain` with
zero confirmed relinks and an inspect-before-retry instruction. Repeating the
old mapping after success is refused.

Pure planner tests cover every physical pad, equivalence, duplicate identities,
unknown protobuf data and complete revision inputs. Stateful native-protocol
mock tests exercise dry run, stale source/revision, wrong mapping, successful
apply, dropped path, collateral routing changes and post-write source changes.
The ignored `live_identity_relink_is_exactly_one_path_change` test requires a
separately opened disposable FC project copy, `KONNECT_LIVE_LIBRARY_COPY`, its
frame-specific `KICAD_API_SOCKET` (never shared `api.sock`) and real `KICAD_CLI`.
It applies the U2 regression through the handler and independently compares the
entire board against exactly one symbol path change without saving it.
