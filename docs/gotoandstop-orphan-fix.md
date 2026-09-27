# Fixing the quadratic `gotoAndStop` cost in Ruffle, and the remaining startup time

Status: design, not implemented. The client currently avoids the problem with `--smoke-pool 50` (see `AGENTS.md`).
This supersedes the diagnosis in `startup-stall.md`, which blamed `BitmapData.draw`. Measurements disproved that.

## 1. What was measured

Baseline run (unpatched `main.swf`, `--script-timeout 600`, `[probe]` counters from `ruffle/core/src/xenomit_probe.rs`):

| operation during `ObjectPoolManager.init()` | calls | total |
|---|---|---|
| `MovieClip.gotoAndStop` | 16.2K | **81.8 s** |
| `BitmapData.draw` (display object source) | 184 | 60 ms |
| `new BitmapData` | ~1.1K | 35 ms, largest 110x120 |
| `getBounds` | 197 | 0.9 ms |

The cost of one `gotoAndStop` grows linearly with the number of clips created so far:

| gotos so far | orphans (≈ 2 per `BitmapClip`) | ms per goto in the last interval | µs per orphan |
|---|---|---|---|
| 5.4K | ~10.8K | 4.9 | 0.45 |
| 8.3K | ~16.6K | 8.4 | 0.51 |
| 12.4K | ~24.8K | 11.3 | 0.46 |

So one goto costs about 0.45 µs × (number of orphans). Summed over the roughly 15,000 engine-smoke `BitmapClip`s, that predicts about 100 s. The measured total was 82 s. The model fits.

With every pool capped at 50, the same init takes 0.6 s.

### Why there are so many orphans

`PatternManager` reads the pool sizes from the embedded `game.xml`: engine smoke 2500, 500, 2500, 2500, 2500, 2500 (plus a second 2500 for `engineSmoke3`), and rocket smoke 1000, 1000, 1000, 50. `createEngineSmokePool` / `createRocketSmokePool` construct one `BitmapClip` per slot:

```
_loc5_ = MovieClip(_loc3_.getEmbeddedMovieClip("mc"));   // new symbol instance, no parent
_loc6_ = new BitmapClip(_loc5_, param1);                  // Sprite subclass, no parent
```

`BitmapClip.flattenDisplayObject` always calls `MovieClip(param1).gotoAndStop(this._startFrame)`. The per-frame `draw` only runs for the first clip of each resKey; the rest hit the cache. So there is at least one goto per `BitmapClip`.

In Ruffle, every AS3-constructed `MovieClip` (and every `Sprite`, which Ruffle implements as a `MovieClip`) that has no parent is added to the orphan list in `MovieClip::set_object2` (`core/src/display_object/movie_clip.rs`). The pooled objects are strongly referenced by the pool vectors, so they stay orphans for the whole session. That makes about 2 orphans per `BitmapClip` (the `BitmapClip` Sprite plus the smoke `MovieClip`), about 36,000 in total.

## 2. The mechanism in Ruffle

`MovieClip::goto_frame_now` (`core/src/display_object/movie_clip.rs`) performs either `run_goto` or, for a goto to the current frame, `no_op_goto`. **Both end in `frame_lifecycle::run_inner_goto_frame`** (unless the clip's SWF is version 9 or lower):

```rust
// core/src/frame_lifecycle.rs, run_inner_goto_frame
OrphanManager::each_orphan_obj(context, |orphan, context| orphan.construct_frame(context));
stage.construct_frame(context);
broadcast_frame_constructed(context);
stage.run_frame_scripts(context);
OrphanManager::each_orphan_obj(context, |orphan, context| orphan.run_frame_scripts(context));
...
broadcast_frame_exited(context);
context.orphan_manager.cleanup_dead_orphans(context.gc());
```

This is intentional. It matches Flash semantics, introduced by ruffle-rs/ruffle#11261 ("Run 'nested goto frame' for entire Stage and orphans", needed by Steamlands). Any goto runs a nested construct/frame-script pass over the entire stage and **every orphan subtree**. Per orphan, that is two recursive walks (`MovieClip::construct_frame` and `MovieClip::run_frame_scripts`), plus a weak upgrade and a parent check in `valid_orphan`, plus the `retain` in `cleanup_dead_orphans`.

A loop that creates one new orphan and then calls one goto is therefore O(n²). Two smaller quadratic terms sit on top of that:

- `OrphanManager::add_orphan_obj` dedups with a linear `iter().all(ptr_eq)` scan. About 36K² / 2 ≈ 6.5·10⁸ pointer compares, roughly 0.3–0.6 s.
- `cleanup_dead_orphans` runs a full `retain` on every goto.

The same list is also walked by `run_all_phases_avm2` on every normal frame: `enter_frame`, `construct_frame` and `run_frame_scripts` for each orphan. With the unpatched pools, that is about 36K subtree walks × 3 per frame after startup, costing frame rate during play as well.

There is no upstream fix. Related upstream items: #11346 fixed orphans with broadcast listeners; #23315 notes that orphans get `enter_frame` every frame.

For the smoke clips, the nested pass is pure overhead. After construction, the orphan subtrees have every object constructed and no frame scripts, so `construct_frame` and `run_frame_scripts` do nothing observable on them.

## 3. The fix

Skip orphan subtrees that provably have no frame work, and keep that knowledge up to date cheaply. The design is layered so that each stage can land and be tested separately.

### Stage A — O(1) orphan dedup (small, safe)

Add a `DisplayObjectFlags::IN_ORPHAN_LIST` bit (`core/src/display_object.rs`, `bitflags! struct DisplayObjectFlags`; bit 17 is free).

- `add_orphan_obj`: `if !dobj.base().has(IN_ORPHAN_LIST) { set; push }` instead of the linear scan.
- `cleanup_dead_orphans`: clear the bit on entries that are dropped while still alive.
- Dead weak refs can't be reached, so their bit doesn't matter.

This removes one quadratic term and has no semantic effect.

### Stage B — skip idle orphans in the nested and normal construct/script passes (the main fix)

**New state.** Add a `DisplayObjectFlags::ORPHAN_FRAME_IDLE` bit. It is only meaningful on an orphan root, meaning the object that is actually in the orphan list. Invariant: *if set, running `construct_frame` and `run_frame_scripts` on this subtree right now would do nothing.*

**The skip.** In `run_inner_goto_frame`, and in the Construct and FrameScripts phases of `run_all_phases_avm2`:

```rust
OrphanManager::each_orphan_obj(context, |orphan, context| {
    if orphan.base().has(ORPHAN_FRAME_IDLE) { return; }
    orphan.construct_frame(context);
});
```

Leave `enter_frame` alone in this stage. It advances playing clips and runs queued PlaceObject tags, so it needs a stronger predicate (see Stage D).

**Setting the bit.** After an orphan has been processed in the FrameScripts pass (both nested and normal), and only if the bit is not already set, evaluate `subtree_frame_idle(orphan)`. If it returns true, set the bit. This is one extra walk per *active* orphan per pass, the same order as the existing cost. Idle orphans are never walked again until they are dirtied.

`subtree_frame_idle(node)` must be conservative. Return `false` whenever in doubt:

- **MovieClip (AS3):**
  - `object2` is `Some` and `initialized()` is true.
  - The clip has no frame scripts registered at all (`frame_scripts` empty). Conservative: clips with any `addFrameScript` never go idle.
  - `has_pending_script()` is false.
  - `queued_goto` is `None`.
  - The queued tag lists are empty.
  - Neither `EXECUTING_AVM2_FRAME_SCRIPT` nor `RUNNING_CONSTRUCT_FRAME` is set.
  - All children satisfy the predicate.
- **Graphic, MorphShape, Text, EditText, Video, Bitmap:** `object2` is `Some` (their `construct_frame` only acts when it is `None`).
- **Avm2Button, LoaderDisplay, Stage, any AVM1 object:** `false`. Their construct and script logic is more involved, and it's not worth proving idleness for them.

**Clearing the bit (invalidation).** Add `fn mark_frame_dirty(dobj)`. It walks `parent()` up to the root and clears `ORPHAN_FRAME_IDLE` on the root if set. Depth is small, so this is O(depth), and it is a no-op for objects under the Stage. Call it wherever an object can gain frame work:

| event | hook site |
|---|---|
| a MovieClip changes frame | `MovieClip::run_goto`, and everywhere `current_frame` is changed in `run_frame_internal` / `increment_current_frame` |
| a child is inserted, replaced or moved | `container.rs`: `insert_at_index`, `replace_at_depth`, `insert_child_into_depth_list`, and the swap helpers; also timeline `place_object` |
| a frame script is registered | `MovieClip::register_frame_script` (reached from `flash.display.MovieClip.addFrameScript`) |
| a goto is queued | `goto_frame` where `queued_goto.set(Some(..))`, and `frame_script_cleanup_queue.push_back` |
| a pending script is flagged | `set_has_pending_script(true)` |
| an object gets its AVM2 object | `set_object2` (a new orphan starts with the bit cleared, which is the default) |
| loader content changes | `LoaderDisplay` content set / unload |
| an object becomes an orphan again | `on_parent_removed`: clear the bit on the object itself |

If a subtree is re-parented under the Stage, the bit becomes irrelevant, because the orphan list drops it in `cleanup_dead_orphans`.

**Why this is exact.** Within one pass, an idle orphan skipped in the construct step could only need construction if an invalidating event happened first. That event clears the bit, and a later step (the frame-script pass, or the next goto or frame) processes it normally. The events that can create work for `construct_frame` are an unconstructed child appearing, or a frame change that places one. The events that can create work for `run_frame_scripts` are a pending script, a queued goto, or a new frame script. All of these are hooked above.

**Safety net.** Behind `cfg(debug_assertions)` or an env var such as `RUFFLE_VERIFY_ORPHAN_IDLE=1`, when an idle orphan is skipped, re-evaluate `subtree_frame_idle` and panic with the object's name if it is false. Run the whole AVM2 regression suite with this on. Any missing invalidation hook shows up as a panic instead of a subtle ordering bug.

**Expected gain.** A skipped orphan costs one weak upgrade, one parent check and one flag check, roughly 5–10 ns instead of about 450 ns. That is about 50–100× less per goto, so the smoke-pool init should drop from about 82 s to about 1–2 s. The iteration over the list itself remains O(n) per goto, so the cost is still quadratic, just with a tiny constant. Stage C removes that.

### Stage C — keep idle orphans out of the iteration (optional, removes the remaining O(n))

Give each orphan-list entry a monotonically increasing sequence number when it is first added (store it in the entry: `Vec<(u64, DisplayObjectWeak)>`). Keep two collections:

- `active: Vec<(seq, weak)>`, iterated by the passes, in insertion order.
- idle orphans are not in `active`. They are only reachable through their own `ORPHAN_FRAME_IDLE` bit and a stored `orphan_seq` field.

When an orphan goes idle, remove it from `active`. Do this lazily in `cleanup_dead_orphans` to avoid mutating the list during iteration. When `mark_frame_dirty` clears the bit on a root, re-insert it into `active` at its original position, found by binary search on `seq`. That keeps the relative order of orphans exactly as it is today, which matters because orphan frame-script order is observable (`avm2/orphan_movie_reorder`). Weak semantics are preserved: an idle orphan that becomes unreachable is simply collected.

Per-goto cost then becomes O(active orphans + stage), independent of the pool size.

### Stage D — also skip `enter_frame` for static orphans (runtime frame rate)

Add a stronger predicate `subtree_frame_static`: `subtree_frame_idle`, plus no `playing()` MovieClip in the subtree, no `SKIP_NEXT_ENTER_FRAME` flags, and no queued add actions. Only then skip `enter_frame` in `run_all_phases_avm2`. The invalidation hooks are the same as above, plus `play()` / `gotoAndPlay` (`set_playing(true)`). This matters for frame rate during gameplay with large pools, and for other games with many orphans (#23315).

### Tests

**Regression suite:**

```
cargo test -p tests --test tests -- avm2/orphan avm2/goto avm2/movieclip_goto avm2/movieclip_displayevents avm2/construct avm2/movieclip_addframescript
```

The relevant existing tests are `orphan_movie_complex`, `orphan_movie_reorder`, `orphan_removeobject`, `goto_on_orphan`, `goto_nested_framescript`, `goto_nested_construct_sibling`, `goto_framescript_queued*`, `instantiation_on_enterframe_gotoandstop`, and `movieclip_displayevents_*goto*`. Then run the full `avm2/` suite once with `RUFFLE_VERIFY_ORPHAN_IDLE=1`.

**New tests:**

- **Perf test:** a SWF that creates 20,000 orphan `MovieClip`s and calls `gotoAndStop(1)` on each. Assert it finishes well under the default 15 s script limit. Keep it modest for CI, e.g. 5,000 clips under 1 s.
- **Correctness test:** an idle orphan gets `addFrameScript` added on a later frame, and a child with a constructor is `addChild`ed into an idle orphan during another clip's goto. Assert that the script and constructor run in the same order as Flash. The expected output needs recording in Flash Player, which requires a GUI, so the user has to do this step.

**Game check:** run the client *without* `--smoke-pool`. Expect `[probe] goto=… ms` to stay in the low seconds and `InitObjectPoolManagerCommand` → `Socket.connect` within a couple of seconds.

### Alternatives considered and rejected

- **Only construct the goto target, not all orphans.** This reverts #11261 and breaks Steamlands and other SWFs that depend on the nested pass.
- **Treat no-op gotos as true no-ops.** Ruffle documents that AS3 no-op gotos have visible side effects (`goto_frame_now` comment, #23032).
- **Drop never-parented clips from the orphan list.** Flash runs frames of AS-created parentless clips (#9450: SteamBirds and others).
- **Cap pool sizes (current client workaround).** It works and also helps frame rate, but it edits the game file (the `game.xml` pool sizes and `gameXMLHash`) and reduces how many smoke particles can be on screen at once. Keep it until Stage B lands.

## 4. What to do about the remaining startup time

Timeline of the patched run (`--smoke-pool 50`), from `[boot]` and `[probe]`:

| t | phase |
|---|---|
| 0 – 4.7 s | authentication, preloader, `loadingscreen.swf`, `main.swf` (4.5 MB) download and init |
| 4.8 – 15.8 s | **`InitPatternManagerCommand`: `MD5.hash()` of the embedded 811 KB `game.xml` in interpreted AS3 (~11 s, ~20M AVM2 actions)**, then XML parse and `BitmapAtlas` |
| 15.8 – 16.4 s | object pools (capped) |
| 16.4 s | game socket connected |
| 16.4 – 22.5 s | map, UI and texture loading over HTTP; second socket (infinicast) at 22.5 s |

The `[probe]` stack tops at +5 s and +10 s are `com.adobe.crypto::MD5$/createBlocks`, `/transform`, `/hh`. MD5 is the single largest item left.

### Option 1 (recommended): run as3corelib's MD5 natively in Ruffle

`com.adobe.crypto.MD5` is a pure function, so it can be replaced exactly:

- `hash(s)` does `ByteArray.writeUTFBytes(s)` and then `hashBinary`.
- `hashBytes(b)` is `hashBinary(b)`.
- `hashBinary(b)` reads all of `b[0 .. b.length]` (it ignores `position`) and returns standard lowercase MD5 hex. Verified: the md5 of the raw `game.xml` bytes equals the embedded `Settings.gameXMLHash`.
- **One side effect:** `hashBinary` sets the static `MD5.digest` to a new `ByteArray`. It writes the four state words with `writeInt` (big-endian), so each 4-byte group of the standard 16-byte digest appears byte-reversed, and `position` is set to 0.

Implementation in the fork: in `core/src/avm2/function.rs::exec`, in the `MethodKind::Bytecode` branch (the same cheap `Method::raw_method_name()` gate used by the boot tracer):

1. If the name is `hashBinary` and the bound class is `com.adobe.crypto::MD5`, take the first argument as a ByteArray and compute MD5 with the `md-5` crate (already in the client's lockfile; add it to `ruffle_core`).
2. Build the `digest` ByteArray as described above and store it in the class's static `digest` slot (`class_object.set_public_property("digest", ...)`).
3. Return the hex string as an `AvmString`.
4. Fall back to the bytecode if anything doesn't match (wrong argument type, missing class).

Expected: about 11 s → under 10 ms. The same hook also speeds up `FileCollection` (`MD5.hash` of the resource XML) and `FileCollectionFinisher.getFileHash` (`MD5.hashBytes` of loaded files), whenever those run.

Test: a unit test that feeds known strings and bytes through the interpreted and native paths and compares both the hex result and the `MD5.digest` bytes. Also re-run the game and check that `InitPatternManagerCommand` → `InitObjectPoolManagerCommand` takes about 1 s.

This is a game-specific shim (a function intercept). It belongs in this fork, not upstream Ruffle, which does not special-case user code.

### Option 2: patch the check out of `main.swf`

`InitPatternManagerCommand.execute` only compares `MD5.hash(xml)` with `Settings.gameXMLHash`. A same-length bytecode patch (turn the comparison into an unconditional branch) would skip the hash. This is less robust than option 1: it depends on the exact bytecode of each `main.swf` build, and it touches the integrity check directly. Only use it if option 1 is not possible.

### Option 3: general interpreter speedups

MD5 in as3corelib does about three AS3 calls per round step (`ff` → `transform` → `f`, passing a `Function` object), roughly 2.5M calls for 811 KB. Ruffle's call overhead dominates, and faster calls would help everything, but that is a large, general Ruffle project. The draft integer-interpreter PRs (#23864, #23926) would not change much here. Not recommended for this goal.

### Smaller items

- **0 – 4.7 s** is network plus preloader. It could be shortened by caching `main.swf` and static resources on disk, keyed by the `gameclientHash`/`*XmlHash` flashvars. That would be a client-side HTTP cache in `navigator.rs`.
- **16 – 22 s** is mostly map/UI/texture downloads and the first Stage3D uploads. It is fine as is.
- After Stage B/D, check frame rate in-map without `--smoke-pool` before dropping the workaround.
