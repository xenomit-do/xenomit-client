# AVM2 orphan lifecycle optimization — implementation report

Implementation of `docs/gotoandstop-orphan-fix.md` against the sibling
Ruffle checkout (`../ruffle`), verified end-to-end with headed
xenomit-client runs against `tr3`.

## Results

Headed run, no `--smoke-pool`, `RUFFLE_VERIFY_ORPHAN_IDLE=1`:

| Milestone                              | Baseline | Stage A | Stage B (+bitset) | + MD5 hook |
|----------------------------------------|----------|---------|-------------------|------------|
| `InitObjectPoolManagerCommand` entered | +14.9s   | ~+15s   | +15.1s            | **+10.6s** |
| Cumulative `gotoAndStop` work          | ~85s     | ~82s    | ~7–13s            | ~12–14s    |
| Game socket `Socket.connect`           | +97.5s   | +99.0s  | +22.7–31.5s       | **+26.3s** |
| Infinicast socket connect              | +104.9s  | —       | ~+30s             | **+29.5s** |

Goto count is unchanged (~16–19K during pool init) — the fix removes the
per-goto orphan scans, not the gotos themselves. The game then runs
normally (starfield, tweens, HUD, map rendering) for 8+ minutes with no
invariant failures; gameplay gotos cost ~0.2ms each afterwards.

## The problem

In AVM2, display objects with no parent still tick. Ruffle keeps them in
`OrphanManager.orphans`, a `Vec<DisplayObjectWeak>`, and every frame pass
(`enter_frame`, `construct_frame`, `run_frame_scripts`) iterates the whole
list. `MovieClip.goto_frame` additionally runs a *nested* lifecycle pass
(`run_inner_goto_frame`) that re-iterates the entire orphan list plus the
stage tree.

DarkOrbit's `ObjectPoolManager.init` constructs ~18,000 `BitmapClip`s, each
performing several `gotoAndStop`s while parentless. Each of those ~36K
gotos ran full Construct + FrameScripts passes over an orphan list that
grows to ~36K entries → O(n²) with n ≈ 36K ≈ **82 seconds** of pure
lifecycle overhead, before the game socket can even connect.

A secondary cost: the same startup window executes as3corelib's
`com.adobe.crypto::MD5.hashBinary` interpreted (~20M bytecode actions over
~800KB inputs), contributing ~11–13s on its own.

## Design constraints

The semantics must be indistinguishable from iterating every orphan on
every pass:

- An orphan that gains frame work mid-pass must be processed **in that
  same pass** (nested gotos depend on this), not deferred.
- Entry order and the weak-ref / dead-entry behavior are preserved.
- `enter_frame` still runs on all orphans — only `construct_frame` and
  `run_frame_scripts` are skippable, since those are the passes that can
  be proven no-ops.

## Fix 1 — O(1) orphan membership (Stage A)

`add_orphan_obj` previously scanned `orphans` linearly to dedup. Each
display object now carries a bookkeeping cell, whose presence means "in
the list":

```rust
// core/src/display_object.rs:340
orphan_idle: RefCell<Option<(Rc<RefCell<Vec<u64>>>, usize)>>,
```

```rust
// core/src/orphan_manager.rs:64-85 — O(1) dedup via the cell
if dobj.orphan_idle_cell().is_none() {
    let index = self.orphans.len();
    self.orphans_mut().push(dobj.downgrade());
    ...
    dobj.set_orphan_idle_cell(Some((self.idle_flags.clone(), index)));
}
```

The cell is a `(shared bitset, index)` tuple — it simultaneously proves
membership and gives O(1) access to the object's idle bit. Stale entries
(object died or re-entered) are handled by `valid_orphan`
(`orphan_manager.rs:268-273`) during iteration and cleared in
`cleanup_dead_orphans` (`orphan_manager.rs:233,242`), which also calls
`set_orphan_idle_cell(None)` on removal so a re-orphaned object re-joins
cleanly.

**Expected effect:** only removes the add-scan term (~0.5s of ~82s) —
verified: socket +99.0s, pool init unchanged. It is the foundation the
idle tracking builds on.

## Fix 2 — idle orphan subtree tracking (Stage B)

### Shared idle bit

`OrphanManager` holds `idle_flags: Rc<RefCell<Vec<u64>>>`, one bit per
orphan-list slot, parallel to `orphans` (`orphan_manager.rs:30-52`). The
object's cell points into the same vec, so `mark_frame_dirty` can flip the
bit without a context, and an in-progress iteration sees marks on entries
it hasn't reached yet — exactly matching unconditional iteration.

```rust
// core/src/display_object.rs:2425-2431 — wake the orphan root, O(depth)
fn mark_frame_dirty(self) {
    let mut cur: DisplayObject<'gc> = self.into();
    while let Some(parent) = cur.parent() {
        cur = parent;
    }
    cur.set_orphan_frame_idle(false);
}
```

### Where work is skipped

Only the passes that can be proven no-ops use the idle filter
(`core/src/frame_lifecycle.rs`):

```rust
// :80-89  — Enter still iterates ALL orphans
OrphanManager::each_orphan_obj(context, |orphan, context| {
    orphan.enter_frame(context);
});
// :87-89 — Construct skips idle
OrphanManager::each_active_orphan_obj(context, |orphan, context| {
    orphan.construct_frame(context);
});
// :93-102 — FrameScripts skips idle, then *earns* the mark:
OrphanManager::each_active_orphan_obj(context, |orphan, context| {
    orphan.run_frame_scripts(context);
    if crate::orphan_manager::orphan_subtree_frame_idle(orphan) {
        orphan.set_orphan_frame_idle(true);
    }
});
```

The same pair is applied inside the nested goto pass
(`run_inner_goto_frame`, `frame_lifecycle.rs:152-168`) — which is where
the quadratic cost actually lived.

### The idle predicate

`MovieClip::frame_idle` (`movie_clip.rs:845-897`) returns true only when a
Construct + FrameScripts pass on the clip would do nothing observable:

- AVM2 object exists and `initialized` (first construction also defers
  child construction),
- no `loop_queued` (Construct clears it unconditionally — skipping would
  let it linger and change later gotos),
- no `queued_goto`,
- no *fresh* frame script: `last_queued_script_frame != queued_script_frame`
  AND a script exists on the queued or current frame,
- no non-`None` `QueuedTagList` entries,
- not currently inside `EXECUTING_AVM2_FRAME_SCRIPT` /
  `RUNNING_CONSTRUCT_FRAME`.

The subtlety that made this work: `has_pending_script` is **sticky** —
`check_has_pending_script` re-arms it on every Construct pass for clips
parked on scripted frames, and `run_local_frame_scripts` only clears it
when a script actually runs. Gating on it kept every clip non-idle
forever. The real work condition is the freshness check that gates actual
execution (`movie_clip.rs:864-878`), which is what the predicate mirrors.

Leaf display objects (Bitmap, EditText, Graphic, MorphShape, Text,
TextLine, Video) are idle once `object2().is_some()` — their
`construct_frame` only allocates the AVM2 object
(`orphan_manager.rs:278-296`). Stage/buttons/LoaderDisplay are never
considered idle — conservative by default.

### Wakeup hooks

Any state change that could give an orphan subtree work calls
`mark_frame_dirty`, which clears the root's bit:

| Site | File:line | Why |
|---|---|---|
| `set_parent` | `display_object.rs:1996-2004` | Single funnel for every reparent (add/remove child, timeline placement, Loader content, AS3 `addChild`/`removeChild`) |
| `goto_frame` | `movie_clip.rs:970` | Explicit goto queues FrameScripts work |
| `set_has_pending_script` | `movie_clip.rs:830-837` | Pending frame script = next-pass work |
| `set_current_frame` | `movie_clip.rs:1227` | Frame position changed |
| `run_frame_internal` | `movie_clip.rs:1459` | Frame advance queues timeline tags |
| `run_goto` | `movie_clip.rs:1737` | Executing a queued/goto |
| `register_frame_script` | `movie_clip.rs:2232` | `addFrameScript` adds/removes callables |
| `replace_with_movie` | `movie_clip.rs:408` | Fresh unconstructed timeline |

A clip that becomes an orphan always starts with its bit cleared
(`orphan_manager.rs:73-74`), so it is always processed by the next pass —
the idle mark is only ever *earned* after a pass completes with no
remaining work.

## Fix 3 — flat bitset iteration + gated cleanup (Stage B perf)

Two costs remained after idle tracking worked:

1. **The O(n) scan itself.** Originally each entry carried an
   `Rc<Cell<bool>>` — 36K scattered pointer chases per pass. The flags
   were flattened into the `Vec<u64>` bitset described above; iteration
   walks 64-entry chunks and skips a whole chunk when its word is
   all-idle without touching the weak refs at all
   (`orphan_manager.rs:111-163`). The word is **re-read before every
   processed entry** — a `mark_frame_dirty` during `f` re-activates a
   still-unvisited bit exactly like a plain left-to-right scan.

2. **`cleanup_dead_orphans` ran a full `retain` every pass.** Dead weak
   refs and re-parented objects are already filtered during iteration;
   the retain is only *required* for `placed_by_avm2_script() == false`
   entries (they must stop receiving passes at end-of-frame). The sweep
   is now gated: `needs_cleanup` forces it, otherwise it runs once per GC
   collection (`mc.metrics().total_gc_count()`,
   `orphan_manager.rs:168-183`). The rebuild compacts both the list and
   the bitset and fixes each survivor's index (`orphan_manager.rs:220`).

### Invariant checker

`RUFFLE_VERIFY_ORPHAN_IDLE=1` re-evaluates `subtree_frame_idle` on a
sampled (1-in-101) basis whenever an entry is skipped, and panics if it
turns out non-idle — turning a missing dirty hook into a crash instead of
a subtle ordering bug (`orphan_manager.rs:304-337`). Sampled because
full checking would dominate the pass it verifies. No violations in any
run; the only output was the routine `skipped dead orphan entry` notice.

## Fix 4 — native MD5 hook (xenomit-specific)

as3corelib's `MD5.hashBinary` runs interpreted during startup (~20M
actions). `core/src/avm2/function.rs:394-460` intercepts it in `exec`
(`function.rs:326`) before activation init:

- Gate order: `bound_class` → `i_class()` → QName check
  (`com.adobe.crypto::MD5`) — cheap, no trait lookups for foreign calls.
  `hashBinary` is static, so it binds to the c_class `MD5$`; `i_class()`
  resolves to the real class for both the name check and the domain lookup.
- Method identity comes from the **dispatching trait's** name, not the
  ABC `method_info` name — release builds omit those
  (`function.rs:416-429`, same approach as `display_function`).
- Computes via RustCrypto `md-5` (`core/Cargo.toml:38`), writes
  `MD5.digest` as a ByteArray with the same byte-reversed word order and
  `position = 0` semantics as the AS3 `writeInt` loop, returns the
  lowercase hex string.
- Every mismatch falls back to the interpreter (`Option<Value>` returns
  `None`).

Effect: the InitConnection→InitObjectPool gap dropped from ~10–14s to
~6s; the ~20M MD5 actions are gone from the action counter.

**Note:** this hook is a client-fork convenience, not a general Ruffle
feature — see the PR section.

## Verification

- `cargo test -p tests --test tests` filters, all pass (242 total):
  - `orphan` — 4/4 (`orphan_movie_complex`, `goto_on_orphan`,
    `orphan_movie_reorder`, `orphan_removeobject`)
  - `goto` — 68/68
  - `framescript` — 11/11
  - `displayevents` — 19/19
  - `construct` — 35/35
  - `movieclip` — 105/105
- xenomit-client `cargo test` — 11/11 pass.
- Headed client, no `--smoke-pool`: pool init ~82s → ~9–16s, game socket
  +97.5s → +26.3s, infinicast +29.5s, stable for 520+s of gameplay.

## Files changed (Ruffle side)

- `core/src/orphan_manager.rs` — bitset, dedup, `each_active_orphan_obj`,
  gated cleanup, `subtree_frame_idle`, invariant checker
- `core/src/display_object.rs` — `orphan_idle` cell on `DisplayObjectBase`,
  accessors, `mark_frame_dirty`, `set_parent` hook
- `core/src/display_object/movie_clip.rs` — `frame_idle`, dirty-mark sites
- `core/src/frame_lifecycle.rs` — `each_active_orphan_obj` in
  `run_all_phases_avm2` and `run_inner_goto_frame`
- `core/src/avm2/function.rs` + `core/Cargo.toml` — MD5 hook (client-fork)

---

## PR description (for upstream Ruffle)

> **Important:** the upstreamable part is the orphan optimization only.
> The `hashBinary` native intercept hardcodes a DarkOrbit/as3corelib class
> name and is a client-fork hack — upstreaming it as-is would likely be
> rejected; if you want native MD5 upstream, propose a general mechanism
> (e.g. a per-method native override table) instead. Exclude
> `function.rs`/`Cargo.toml` from the PR, and also exclude the diagnostics
> (`[boot]`/`[probe]` tracing in `activation.rs`, `call_stack.rs`, etc. —
> those live in `stash@{0}` of this fork, not in the diff below).
> `verify_orphan_idle` is worth keeping: it's zero-cost unless the env var
> is set and documents the invariant the optimization depends on.

```markdown
# Optimize AVM2 orphan frame passes over large orphan lists

## Problem

AVM2 orphan display objects (no parent) receive `enter_frame`,
`construct_frame`, and `run_frame_scripts` every frame — and, crucially,
on **every nested frame pass triggered by `gotoAndStop`/`gotoAndPlay`**
(`run_inner_goto_frame`). Each pass iterates the entire `OrphanManager`
list, so a SWF that performs N gotos while holding M orphans costs
O(N·M) lifecycle work.

Content that builds large object pools of unattached MovieClips hits this
quadratically: e.g. constructing ~18k pooled clips (each doing a few
`gotoAndStop`s while parentless) produces ~36k orphan entries and ~36k
gotos → ~80s of pure frame-pass overhead before the movie finishes
initializing. `add_orphan_obj` also dedups with a linear scan (O(M) per
add).

## Approach

Keep the observable semantics of "iterate every orphan every pass", but
make the no-op case cheap:

1. **O(1) membership.** Each `DisplayObjectBase` carries
   `orphan_idle: RefCell<Option<(Rc<RefCell<Vec<u64>>>, usize)>>` — a
   shared bitset + index. Its presence replaces the linear dedup scan, and
   removal clears it so re-orphaning re-joins cleanly.

2. **Idle-subtree bits.** `idle_flags` is a `Vec<u64>` parallel to the
   orphan list. A bit means "the last pass proved this subtree has no
   remaining `construct_frame`/`run_frame_scripts` work". The bit is only
   *set* after a pass completes idle (`MovieClip::frame_idle` →
   `subtree_frame_idle`: no AVM2-object construction pending, no queued
   goto/tags, no *fresh* frame script — `last_queued_script_frame !=
   queued_script_frame` mirrors the freshness check that actually gates
   execution, since `has_pending_script` is sticky — and no frame/construct
   script currently executing).

3. **Wake on any state change.** `mark_frame_dirty` walks the `parent()`
   chain to the orphan root and clears its bit in O(depth), called from
   `set_parent` (covers every reparent path), `goto_frame`, `run_goto`,
   `run_frame_internal`, `set_current_frame`, `set_has_pending_script`,
   `register_frame_script`, and `replace_with_movie`. Because the bitset
   is shared between objects and the manager, a mid-pass dirty mark is
   visible to entries the iteration hasn't reached yet — a reactivated
   orphan is processed **in the same pass**, preserving nested-goto
   ordering exactly.

4. **Chunked iteration.** `each_active_orphan_obj` (used by the Construct
   and FrameScripts phases of `run_all_phases_avm2` and
   `run_inner_goto_frame`) scans the bitset one word per 64 entries; a
   fully-idle chunk is skipped without touching its weak refs, and the
   word is re-read before every processed entry so mid-pass reactivation
   behaves identically to a plain scan. `enter_frame` still iterates all
   orphans via `each_orphan_obj` — unchanged.

5. **Gated cleanup.** `cleanup_dead_orphans`' `retain` is only required
   for `placed_by_avm2_script() == false` entries (they must leave the
   list at end-of-frame); dead weak refs and re-parented objects are
   already filtered during iteration. The sweep now runs unconditionally
   only when such an entry was added, or once per GC collection
   (`Mutation::metrics().total_gc_count()`), keeping cleanup O(1) in the
   common case.

6. **Invariant checker (debug aid).** `RUFFLE_VERIFY_ORPHAN_IDLE=1`
   re-evaluates `subtree_frame_idle` on a sampled basis (1-in-101) for
   skipped entries and panics on a false positive, so a missing
   `mark_frame_dirty` hook crashes the test suite instead of silently
   reordering.

## Testing

- `cargo test -p tests --test tests` — orphan ×4, goto ×68, framescript
  ×11, displayevents ×19, construct ×35, movieclip ×105: all pass.
- Against a real AVM2-heavy production SWF (~36k orphans, ~36k startup
  gotos): startup goto overhead dropped ~82s → ~7s; total time-to-connect
  ~97s → ~23s, with identical movie behavior.
```
