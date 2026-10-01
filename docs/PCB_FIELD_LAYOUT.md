# Placed Reference/Value layout

`edit_footprint_field_layout` in `pcb_components` previews or applies a batch
of instance field layout edits over native IPC. Every entry binds an exact
footprint UUID and reference and selects `Reference` or `Value`. Optional
`position: {x,y}` uses absolute board millimeters; `angle_deg` uses absolute
degrees normalized into [0,360); `size: {x,y}` specifies text width/height in
millimeters; `visible` controls the placed Field's visibility. Field contents,
UUIDs, layer, stroke, mirror/upright flags, footprint pose, pads and library
graphics are preserved. No PCB file is saved automatically.

`dry_run` defaults to true. Its `plan_revision` covers every raw PCB object,
the live document/net table and the exact edit batch. Apply requires that
revision as `expected_plan_revision`; stale plans and ambiguous targets are
refused before mutation. Duplicate edits to the same field are refused.

The batch uses one KiCad undo commit. Native transactions can still leave
uncertain persistence on a backend failure. A fresh complete raw-board readback
must equal the exact field-only expected result. Dropped field updates,
partial batch application and collateral routing changes yield `uncertain`,
zero confirmed applied fields and unsafe-to-retry guidance. Inspect the live
board and undo if appropriate before generating a fresh preview.

The schema is exposed through `tools/list`. A minimal preview is:

```json
{
  "board": "/absolute/board.kicad_pcb",
  "changes": [{
    "reference": "J2",
    "footprint_uuid": "11111111-1111-4111-8111-111111111111",
    "field": "Reference",
    "position": {"x": 15.9, "y": 8.2},
    "angle_deg": 0,
    "size": {"x": 0.7, "y": 0.7},
    "visible": true
  }]
}
```

Reference/Value text positions and glyph sizes use checked nanometer conversion.
Nonfinite, overflowing and sub-nanometer zero sizes are refused. Tests cover
exact field-only preservation, target ambiguity, stale requests, dropped and
partial native writes, collateral copper changes and a real isolated KiCad
batch with independent complete-board comparison.
