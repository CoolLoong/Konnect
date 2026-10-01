# Preserve library circle radius points through IPC

Library circles persist a center and circumference control point. Replacing the latter with center + (radius, 0) preserves the rendered circle but changes its authored geometry. KiCad compares both circle points for library parity, so a library refresh could leave all 13 stock TestPoint footprints reporting a mismatch.

The footprint graphic converter now transforms both authored points into board coordinates, including the existing side-mirroring and rotation transforms. It retains the shared circle builder for stroke/fill/UUID defaults, then sets the exact transformed radius point. Other graphic types and pad conversion are unchanged; no comparison tolerance is relaxed.

Unit coverage includes a translated/rotated asymmetric circle and the stock TestPoint vertical radius point on front/back layers at 0 and 90 degrees. Deliberately removing radius-point preservation makes the TestPoint regression fail. The complete IPC unit suite passes.

Real GUI acceptance uses an isolated FC board copy on a dedicated IPC socket. It must demonstrate the stock library refresh clears the 13 TestPoint parity warnings, preserves every physical pad UUID/geometry/net, rejects a stale plan revision and converges to no change. Saving for CLI DRC is restricted to that isolated copy. Formal FC design edits remain with its owner.

KiCad comparison source: https://docs.kicad.org/doxygen/drc__test__provider__library__parity_8cpp_source.html (Circle case compares GetLibraryStart and GetLibraryEnd).
