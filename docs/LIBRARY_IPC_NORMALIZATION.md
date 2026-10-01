# Footprint library update: KiCad pad angle readback

KiCad 10.0.6 serializes a pad's signed `-90` degree stack angle as `270`
degrees. Library refresh previously compared those protobuf values literally,
so a successful update of rotated solder lands became `uncertain` and the next
dry run proposed the same update again. The failure message broadly named
attributes even when only pads differed.

The pad comparison now canonicalizes finite angles modulo 360, including
positive zero. It does not round angles or tolerate position/size differences.
Independent readback still compares every supported library domain and placed
instance state. Readback mismatches identify the changed domains and remain
`uncertain` with zero confirmed updates; no automatic save or rollback is added.

`tests/fixtures/ipc_library_pad_angle_{prepared,after}.bin` are the exact typed
Pad payloads sent to and read independently from a separately opened disposable
FC project copy through KiCad 10.0.6 IPC. The regression checks their equivalence
and detects a one-degree angle change, one-nanometre size or mask-margin change,
paste-mode change and pad-shape change.

The ignored `live_library_refresh_converges_and_preserves_instance_state` test
requires `KONNECT_LIVE_LIBRARY_COPY` pointing to that disposable project, all eight
regression references (D2/D3/D4/J2/J5/J6/U2/U4), changed copied library definitions,
and `KICAD_API_SOCKET` pointing to its separate `api-<pid>.sock`. It rejects the
shared `api.sock`, explicitly binds the open document, applies through the tool
handler, independently checks readback and convergence, checks all footprint
instance state, unselected raw items, and raw trace/arc/via/zone payloads. It never
saves the board or restarts/closes editors. Optional `KONNECT_LIVE_LIBRARY_EVIDENCE`
records original/prepared/observed protobuf payloads and the apply response.
