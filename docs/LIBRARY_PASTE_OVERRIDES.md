# Pad mask and paste overrides during library refresh

Library refresh supports per-physical-pad `solder_paste_margin` and
`solder_paste_margin_ratio` through the existing IPC PadStackOuterLayer
SolderPasteOverrides model. Duplicate logical pad numbers keep independent
settings. Both technical outer-layer settings are retained on either board
side and at non-cardinal rotation; actual paste layers are still those in the
library layer set. Distance is converted to checked signed 32-bit nanometres,
ratio remains a finite unitless double, and duplicate or malformed clauses
refuse before mutation.

For footprint format versions <= 20240201, zero mask/paste settings mean
inheritance, matching KiCad's legacy reader. Newer formats preserve explicit
zero separately from absent settings. A missing version follows legacy
semantics; malformed or repeated versions refuse. Existing tests using a
KiCad 10-authored fixture keep their explicit-zero behavior.

Independent readback compares typed pad overrides. If an accepted write loses
paste settings, the result is uncertain and has no confirmed applied count;
do not assume rollback. The fault test isolates paste loss from mask settings.

`property pad_prop_mechanical` remains unsupported: KiCad 10.0.6's Pad IPC
message and PAD::Serialize/Deserialize do not carry PAD_PROP. Library refresh
must refuse it; accepting the property while stripping it would be lossy.

Source evidence: KiCad's official source mirror, tag 10.0.6:
- https://github.com/KiCad/kicad-source-mirror/blob/10.0.6/pcbnew/pad.cpp
- https://github.com/KiCad/kicad-source-mirror/blob/10.0.6/pcbnew/padstack.cpp
- https://github.com/KiCad/kicad-source-mirror/blob/10.0.6/pcbnew/pcb_io/kicad_sexpr/pcb_io_kicad_sexpr_parser.cpp

Depends on codex/footprint-library-properties (c714d3b). Geometry, pad nets,
placed identity and original library source remain unchanged by the parser.
