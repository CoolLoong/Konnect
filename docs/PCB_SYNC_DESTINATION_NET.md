# Joining a routed destination net

During `update_pcb_from_schematic`, a pad changing from an unrouted old net
may join an already routed destination net. This changes only the pad's net
assignment and does not reassign any existing copper.

The old implementation refused a change whenever either old or destination
net appeared in the live routed-net inventory. For D2.1/J8.1 on the FC board,
old `FC_5V` had no tracks, arcs, vias or copper zones, but the destination
`FC_5V_BATT` contained buck output tracks. The destination-wide check blocked
those unconnected pads despite the absence of old-net copper.

The guard now checks old-net copper. It remains deliberately conservative:
if any copper uses the old net, the entire sync plan is refused, even if that
copper may be distant from this pad. Removing/changing a pad on an old routed
net remains blocked. Native name-only copper-zone nets continue to be resolved
against the live net table; unknown zone evidence remains a refusal. No net
code defaults or copper coverage have been relaxed.

Tests permit an unrouted or unassigned pad to join a routed destination,
including a name-only destination copper zone, and preserve old track/via/zone
protection. Formal-board acceptance is read-only: preview first, then the FC
owner applies using the new current revision.
