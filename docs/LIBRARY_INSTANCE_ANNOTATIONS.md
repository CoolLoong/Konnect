# Instance annotations during library refresh

`update_footprints_from_library` preserves the current Reference, Value,
Datasheet and Description values, including explicit empty strings. The
library's mandatory field clauses remain validated, but library defaults do
not authorize replacing a placed annotation. Existing custom fields are
retained verbatim even when a library field has the same name; only missing
custom fields are added. This protects schematic annotations such as MPN.
Library geometry, attributes, 3D models and library description metadata still
refresh. The existing explicit `unlocked` presentation support remains.

This changes the former behavior that replaced Datasheet/Description and
same-name custom fields with library defaults. Changing an annotation belongs
to an explicit field edit or schematic synchronization.

`preserved.instance_overrides` refers to design-rule overrides.
`preserved.instance_field_values` compares the four mandatory annotation
values. An independent post-write readback checks the prepared mandatory
fields and custom fields; a mismatch returns `mutation_outcome_uncertain`
with zero confirmed applied count. Inspect the live board before retrying.

This branch depends on `codex/footprint-library-properties` (c714d3b), which
already includes upstream/main af1c185. Regression tests cover both sides,
non-cardinal rotation, blank defaults and overrides, same-name custom fields,
and a successful update followed by an independently cleared Datasheet.
