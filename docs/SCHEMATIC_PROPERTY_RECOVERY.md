# Compact symbol property insertion and recovery

`batch_edit_schematic_components(create_missing=true)` formerly copied the
non-whitespace symbol header as indentation when the first existing property
was on the symbol's opening line. Every generated property line then opened
another placed symbol. Subsequent assignments could fail while the batch still
wrote the malformed candidate.

The shared property inserter now uses only whitespace indentation. Batch field
editing validates the entire source and candidate, preserves every non-property
node and existing property geometry, verifies declared field values and unit
coverage, then independently reads the committed file. `status: partial` and
`errors` remain material even when the transport succeeds. A failed readback
reports `uncertain` with zero confirmed update counts.

## Recovering the exact historical signature

`repair_schematic_property_prefixes` is a narrow MCP operation for this bug.
For each affected placed unit, supply `reference`, `symbol_uuid`, `field_name`
and the exact retained `value` inside `repairs`. Use the complete schema from
`tools/list`; the tool defaults to `dry_run: true`.

It recognizes the exact nine-line legacy generated body and a header occurring
once on the original symbol plus nine times in that body. It deletes only the
nine duplicated prefixes. All remaining bytes, including already changed Value,
Datasheet, MPN and the new custom field, remain intact. It validates the complete
recovered document and uniquely binds the resulting symbols and property values.
It refuses an inexact body, mismatched proof, incomplete coverage, ambiguity,
additional damage or trailing data.

Dry run returns `source_sha256`, `candidate_sha256`, deletion spans and
`plan_revision`. Applying requires that exact revision for the same file and
proofs. Apply creates a sibling immutable `.preimage` backup of the damaged
source, uses the shared atomic source comparison and editor-lock guard, then
independently reads the complete file. Unexpected state after a possible write
is `uncertain`; inspect before retrying. It does not restart/reload editors or
restore an arbitrary file copy.

The operation proves structural recovery for this exact signature. It does not
claim a clean ERC, electrical equivalence or a complete visual review. Export the
whole saved hierarchy and compare net membership, pin functions/types, instance
identities and layout against independent pre-damage evidence before continuing.
An already open schematic must be closed safely before apply; unsaved editor
changes are not automatically discarded.
