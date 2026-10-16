# xenomit-client notes

## Bridge
- The reflection bridge from `DOBOT/windows_flash_mvp/activex_player` (see its
  `BRIDGE.md`) is enabled by default; `--no-bridge` disables it.
- `src/swf_bridge.rs` splices `DoABC2`+`DefineSprite`+`SymbolClass`+`PlaceObject2`
  for `DobotBridge` into the root movie before `SwfMovie::from_data`. The ABC
  blob is `bridge/DobotBridge.abc` (compiled from `bridge/DobotBridge.as`,
  copied from `bridge_abc.inc`). FWS/CWS/ZWS inputs decompress to FWS before
  splicing; on failure it logs and runs the movie unpatched.
- `src/bridge.rs` installs an `ExternalInterfaceProvider` (spoofs
  `location.href`, marks `ready` on `pingBridge`) and serves
  `\\.\pipe\xenomit_bridge_<PID>` (Windows). The legacy commands
  `BRIDGE`/`STATE`/`INSPECT`/`INVOKE`/`FINVOKE` all stay supported:
  `INVOKE`/`INSPECT`/`BRIDGE`/`STATE` run between frames (`CallFunction`
  equivalent); `FINVOKE` runs inside a real frame via `post_frame_callbacks`
  — the equivalent of the ActiveX enter-frame hook, use it for
  `moveHeroToCordinates`. Legacy `STATE`/`INVOKE`/`FINVOKE` execute AS3
  reflection on the player thread and stall the game per request.
- `control.py` mirrors the ActiveX one: `python control.py <PID> bridge`,
  `inspect root`, `invoke <target> <method> '[json]'`, `finvoke ...`.
- `NINVOKE`/`NFINVOKE` are the same wire shape but resolve and call natively
  via `ruffle_core::introspect` — no `DobotBridge` needed. `STATUS` reports
  `frames=`/`capture_us_avg=`/`capture_us_max=` for the native capture.
- `src/state_reader.rs` captures the map model straight from AVM2 slots once
  per frame; `src/state_shm.rs` publishes it to `Local\xenomit_state_<pid>`
  (layout shared with `xenomit-game-api`'s `live::layout`).
- `xenomit-game-api` exposes two backends: `Native` (default — shared-memory
  `LiveFrame` reads plus `NINVOKE`/`NFINVOKE`) and `LegacyPipe`
  (`STATE`/`INSPECT`/`INVOKE`/`FINVOKE`, for old clients and diagnostics).
  There is no automatic fallback in either direction: a player too old for
  the native commands answers `ERR unknown command`, and native
  `snapshot(None)` fails with `Unavailable` until shared memory is
  published — select `LegacyPipe` for those clients.
- Replies: `OK <json>` / `ERR <msg>`; `"status":"error"` inside `OK` means the
  call failed. Do not run a second client with the same dosid.

## Build

- Native shared memory now uses `live::layout::VERSION = 2` with a 2 MiB mapping;
  rebuild both client and bot after wire changes. Navigation capture is cached
  at 4 Hz and publishes current-map NOA geometry, directed map/portal catalogs,
  manager facts, pet/items/configuration and eligible free revival offers.
  Missing/truncated geometry must remain `geometry_ready = false`.
- `src/native_actions.rs` handles `xenomit/gameCommand` via NINVOKE using real
  game request constructors. Keep state validation and free-only revival.
  `Obj::construct` resolves constructors through the class's AVM2 domain.
  Never infer active pet gear from the gear-window selection cursor.

- `StateReader` also emits `[game-log]` JSON for new log-buffer objects and
  `[game-assets]` JSON for Hero credits/Uridium/honor/experience once a minute.
  The controller persists these per account. Reward values match localized
  `getcredits`/`geturidium`/`gatherep`/`getexp` templates. Rooted message handles
  preserve identity across GC and repeated identical reward text.
- Ruffle comes from the sibling checkout `../ruffle` (`[patch]` in Cargo.toml).
- The `jpegxr` feature needs libclang: set `LIBCLANG_PATH=C:\Users\Yagiz\AppData\Roaming\Python\Python313\site-packages\clang\native` before `cargo build --release`.
- Tests: `cargo test` (client, includes `swf_patch`), `cargo test -p ruffle_render atf` in `../ruffle`.

## Run
`target/release/xenomit_client.exe --domain tr3 --dosid <dosid> --script-timeout 600 --smoke-pool 50`
- `--smoke-pool N` patches `main.swf` on download: caps engine/rocket smoke pool sizes in the embedded game XML and swaps the matching `gameXMLHash`. Without it, `ObjectPoolManager.init()` builds ~18k `BitmapClip`s and takes ~85 s, because every `MovieClip.gotoAndStop` runs `run_inner_goto_frame` over all orphan clips (quadratic).
- `--quality low|medium|high|best`, `--headless`.
- Diagnostics (`[probe]` timings, `[boot]` traces) are not in the working tree. They are saved in `../ruffle` as `stash@{0}` ("xenomit full diagnostics backup"). Re-apply with `git stash apply stash@{0}` when profiling.
