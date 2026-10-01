# Zoned-board synchronization coverage

The sync snapshot previously treated every board net as routed when any Zone
existed. This incorrectly blocked an unconnected pin's net reassignment on a
board with ground pours.

Zone net evidence is available through the existing KiCad protobuf:
`Zone.settings.copper_settings.net`. Native KiCad 10 serializes the net name
without a code, as the checked-in live GND-zone capture demonstrates. The adapter
now requires that name to identify a positive-code net in the requested live
board's net table. If a zone supplies a code, it must agree with that table.
Only that net is marked routed. Tracks, arcs and vias keep their existing guards.

The adapter recognizes copper zones and teardrops, validated rule areas, and
explicitly netless graphical zones. Missing net evidence, unknown/inconsistent
types or settings, malformed payloads, unknown names, contradictory codes, and
missing/duplicate identities refuse the snapshot before any commit. An explicitly
present empty Net message means native net-zero; an absent Net does not.
Coverage is published only after all zones are understood.

Regression evidence uses real KiCad IPC captures documented in
`crates/konnect-core/tests/fixtures/issue_474_ipc.README.md`. It protects GND and
supply-zone pins while allowing a third unrouted NC pin to change, and preserves
the existing board-only-object integration case. No saved-file fallback or editor
save/reload is introduced.

Protocol source:
[KiCad 10.0.6 board_types.proto](https://github.com/KiCad/kicad/blob/10.0.6/api/proto/board/board_types.proto).
