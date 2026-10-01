# Preserve library circle radius points through IPC

Library circles persist a center and circumference control point. Replacing the latter with center + (radius, 0) preserves the rendered circle but changes its authored geometry. KiCad compares both circle points for library parity, so a library refresh could leave all 13 stock TestPoint footprints reporting a mismatch.

The footprint graphic converter now transforms both authored points into board coordinates, including the existing side-mirroring and rotation transforms. It retains the shared circle builder for stroke/fill/UUID defaults, then sets the exact transformed radius point. Other graphic types and pad conversion are unchanged; no comparison tolerance is relaxed.

Unit coverage includes a translated/rotated asymmetric circle and the stock TestPoint vertical radius point on front/back layers at 0 and 90 degrees. Deliberately removing radius-point preservation makes the TestPoint regression fail. The complete IPC unit suite passes.

Real KiCad 10.0.6 GUI acceptance used an isolated FC board copy on its dedicated IPC socket. Actual served MCP refresh cleared all 13 TestPoint library-parity warnings (13 before, 0 after) and preserved every physical pad geometry and net. Stale revision was refused, repeated refresh returned no change, and no automatic save occurred. Explicit MCP save for CLI DRC was restricted to that isolated copy; schematic bytes stayed identical. Formal FC design edits remain with its owner.

The existing full-library refresh rebuilds library-owned children, including new physical pad UUIDs. This change preserves the authored circle point; it does not change that existing identity behavior. Acceptance compares pad geometry and nets separately and records that the complete pad API snapshot including UUIDs differs. Do not claim this tool preserves physical pad UUIDs.

KiCad comparison source: https://docs.kicad.org/doxygen/drc__test__provider__library__parity_8cpp_source.html (Circle case compares GetLibraryStart and GetLibraryEnd).
