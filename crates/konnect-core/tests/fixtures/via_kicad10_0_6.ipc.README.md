# Via IPC fixture provenance

Captured read-only on 2026-09-30 from KiCad 10.0.6 macOS aarch64 using
`GetOpenDocuments` and `GetItems` with `KOT_PCB_VIA`. This is one serialized
`kiapi.board.types.Via` from COOLLOONG-FC-G4, UUID
9ee81141-93ba-4326-b7c6-0af0d7dae141 at (7.5, 17.6) mm. No mutation,
transaction, save or editor navigation was performed to obtain it.

The tests decode this KiCad-authored wire image and vary individual fields for
negative controls. It contains the real pad-stack, layer-pair, drill, net and
lock representation emitted by this supported KiCad version. The sample is
not evidence that a live move/delete was run.
