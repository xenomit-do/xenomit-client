# Bridge state snapshot

Run `python control.py <PID> state` while the client is running. An optional
display target can follow `state`, for example `state root`. The command runs
on the player thread and returns `OK` followed by JSON with `status: "ok"` and
`data.entities`. When the game map is active, `data.map` also carries the map
ID/name, world dimensions, the hero's world coordinates, and safe-place
markers for portals, bases, and battle stations. This map data is refreshed
with each state request.

Each entity has a `path`, ActionScript `className`, `name`, stage coordinates
`x`/`y`, stage-space `bounds`, and primitive gameplay fields. While a map and
player are active, snapshots read the game's map model directly and include
stable `id` values, `worldX`/`worldY`, NPC/player markers, and ship health
fields. The bridge skips the display-list walk when those model entities are
available. Before a map is ready, it falls back to visible display-list objects
and reads a fixed set of common fields.
`visited` and `truncated` describe the active snapshot source.

Reflected calls can pass `{"$xenomit_entity_id":123}` as an argument value to
resolve the current map object with that ID. The behavior controller uses this
for the game's collectable movement callback.

The committed `DobotBridge.abc` is compiled from `DobotBridge.as`. After
changing the source, run `bridge/build.ps1` with the Flex SDK and
`playerglobal32_0.swc` in `%TEMP%\dobot-flex`, as described in the original
ActiveX bridge setup. The script regenerates the ABC used by `swf_bridge.rs`.
