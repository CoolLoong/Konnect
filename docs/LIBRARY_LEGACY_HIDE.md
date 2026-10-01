# Legacy footprint field visibility

KiCad accepts a bare `hide` token in footprint properties as equivalent to
`(hide yes)`. Library refresh now accepts both forms. Custom fields retain
that visibility through their typed `Field.visible` value; mandatory clauses
are validated while existing placed presentation remains authoritative.

Duplicate or conflicting bare/list hide tokens, unknown atoms and malformed
explicit booleans still refuse during preflight. No source clause is removed
or rewritten to make a library pass validation.

Depends on codex/footprint-library-properties (c714d3b). Tests transform an
existing KiCad-authored fixture in memory and compare the complete typed
custom fields against its explicit-bool version. The unmodified parser fails
this compatibility test; ambiguous and unknown variants must still refuse.
