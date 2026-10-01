# Reviewed native footprint sides

`batch_flip_components` (`pcb_board`) binds each placed footprint by exact
reference and canonical UUID and explicitly selects `F.Cu` or `B.Cu`.
An already-correct footprint is a no-op; mixed batches do not reset other sides.
The requested board must be open over IPC. There is no layer-only or file
fallback. Unsupported native `FlipItems` cannot be reported as applied.

Preview (`dry_run` defaults true) returns the sides and a `plan_revision` covering
the complete raw board, live document/net table and exact requested changes.
Apply requires that revision as `expected_plan_revision`. Stale requests are
refused before beginning a mutation. KiCad's own `FlipItems` flips each item
around its own anchor with top/bottom reflection, including child geometry,
fields and the native 3D transform, inside one undo commit. No automatic save.

The native response must cover every requested UUID with `ISC_OK` and a complete
footprint. UUID, anchor, schematic path, library identity, attributes, field
contents/visibility, all child identities and physical pad UUIDs/nets must be
preserved. Geometry is produced by KiCad, not synthesized by Konnect. Independent
complete-board reads before and after publishing the commit must exactly match
the native returned footprint payloads and all unchanged board objects/net data.
A readback or partial failure is `uncertain`, with zero confirmed applied
footprints and inspection required before retry. Do not assume rollback.

```json
{
  "board": "/absolute/board.kicad_pcb",
  "changes": [{
    "reference": "U1",
    "footprint_uuid": "11111111-1111-4111-8111-111111111111",
    "layer": "B.Cu"
  }]
}
```

Preview establishes identities and revision, not native capability by a guessed
version. Apply establishes capability through the actual command. Tests cover
stale plans, wrong targets, omitted/partial/incorrect native results, collateral
copper changes and zero-result envelope success. Isolated real KiCad acceptance
compares the entire board through a native two-footprint F/B/F roundtrip.
Existing `flip_component` remains the compatible single-target convenience tool;
use the batch tool for reviewed revision-controlled changes, including one item.

Native semantics: [KiCad API handler](https://docs.kicad.org/doxygen/api__handler__board_8cpp_source.html).
