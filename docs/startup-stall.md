# Why the client stops at 100% login

The headed client authenticates, plays the preloader, downloads `main.swf`, and then sits on the loading screen at the login step. No game TCP socket is opened. `flash.ocx` running the same movie gets past this point.

This note records what the logs and the `main.swf` bytecode actually show, which earlier guesses were wrong, and why a general Ruffle fix is not available.

## What the logs already ruled out

The movie host is `darkorbit-22.bpsecure.com`. The instance host is `tr3.darkorbit.com`. Bootstrap flashvars include `autoStartEnabled=1`, `display2d=2`, `lang=tr`, and both `userID` and `sessionID`.

A successful headed run does all of the following:

1. `loadingscreen.swf` returns HTTP 200.
2. The loading screen's own SWFs and XML finish, and their `LoaderInfo` `complete` events fire.
3. `main.swf` is request `#36`, HTTP 200, about 4.55 MB. Its loader `complete` fires.
4. Flex dispatches `creationComplete` on `MainClientApplication`.
5. The game then loads its own XML and SWF set: language, `flashres.xml`, quests, items, `profile.xml` (HTTP 200, 941 bytes), maps, UI, lasers, ships, mines, and so on, through `eurostileFonts.swf`.
6. `SystemManager` progress events climb (1, 32, 64, 96, 128, then the 150s) in step with those loads.
7. The player-thread executor drains to 0 pending Flash tasks.

`[navigator] game socket requested` never appears. `GameNavigator::connect_socket` is implemented and logs that line before `TcpStream::connect`. ActionScript never calls `Socket.connect`.

`main.swf` is not loaded over TCP. HTTP fetches the SWF. TCP is the game socket, and it is only opened later by `InitConnectionCommand`.

One launch failed with DNS error 11001 on `https://tr3.darkorbit.com/indexInternal.es?action=internalMapRevolution`. The next launch reached the same stall, so that DNS failure is separate.

Ruffle warnings that show up and do not explain the stall:

- AVM2 stubs (`Dictionary` weak keys, a few getters, `addChild` timing).
- `Known default device font not found: _sans`.
- Duplicate video frames inside `common.swf`.

There is no `[ruffle] ERROR ruffle_core::avm2` uncaught exception.

## Where the movie actually stops

A later run printed this interpreter stack after 15 seconds in one player tick:

```
BitmapClip$/search()
  BitmapClip/flattenDisplayObject()
    BitmapClip/init()
      BitmapClip()
        ObjectPoolManager$/createEngineSmokePool()
          InitObjectPoolManagerCommand/execute()
            AsyncMacroCommand/nextCommand()
              ...
                Main/initApplication()
                  MainClientApplication/startGame()
                    GameclientLoader/startGame()
                      ...
                        InitApplicationFinishedCommand/execute()
                          AsyncMacroCommand/nextCommand()
                            PreloadResourcesCommand/handleResourceLoaded()
```

Startup got further than the loading screen:

- `PreloadResourcesCommand` finished a file collection and called `commandComplete`.
- The async init macro advanced through several commands, including `InitApplicationFinishedCommand`.
- That handed off to `GameclientLoader`, which called `startGame`.
- `Main.initApplication` sent the notification that runs `InitObjectPoolManagerCommand`.
- That command calls `ObjectPoolManager.createEngineSmokePool`.
- The pool constructs a `BitmapClip`. The constructor calls `init`, which calls `flattenDisplayObject`.

`InitConnectionCommand` is later in this same async command chain. It never runs, because this command has not returned. The missing socket is a consequence of the object-pool init still being inside the first smoke clip.

## What `BitmapClip.search` actually is

The class is `net.bigpoint.as3toolbox.bitmapclip.BitmapClip` in `main.swf` (DoABC2, method 33713 for the static method). The static method `search` is 20 bytes. It does not scan pixels:

```
getlocal0
pushscope
getlex        cache
getlocal1                 // key
getproperty   <runtime name>
pushnull
ifeq          notCached
pushtrue
returnvalue               // cache[key] != null
notCached:
pushfalse
returnvalue               // cache[key] == null
```

`BitmapClip$/search` means the static method on the class object. It answers one question: is this frame already in the global `cache` object?

`flattenDisplayObject` (instance method 33722, 446 bytes) is the loop. For a `MovieClip` source it does roughly this:

1. Read `_endFrame`. If it is `255`, set `frames` from `MovieClip.totalFrames`. Otherwise require `_endFrame >= _startFrame` and use `_endFrame`.
2. `gotoAndStop(_startFrame)`, then set the loop index to `_startFrame - 1` and jump to the loop test.
3. Each iteration calls `search(cacheId + separator + frame)`.
   - Cache hit: copy `cache[key]` into `bitmapDataPlayList[frame]` and skip drawing.
   - Cache miss: if `frame > 0`, `gotoAndStop(frame + 1)`. Then `getBounds` on the clip, `ceil` the width, height, x, and y, `new BCBitmapData(width, height, true, 0)`, build a `Matrix`, `translate(-x, -y)`, and `bitmap.draw(clip, matrix, _colorTransform)`. Store x/y on `bitmap.offset`. Write the bitmap into the playlist. If `cacheId` is non-null, call `addBitmapData(key, bitmap)`.
4. Increment the frame and repeat while `frame < frames`.
5. Set `_frame` to `_startFrame - 1` and call `updateFrame()`.

`BCBitmapData` extends `flash.display.BitmapData` and only adds an `offset` field. `draw` is `BitmapData.draw` with three arguments: the clip, the matrix, and the color transform. There is no `getPixel` or `getPixel32` anywhere in `search`, `flattenDisplayObject`, `init`, or `createEngineSmokePool`.

`createEngineSmokePool` (static method 65389) loads one embedded clip through `ResourceManager.fileCollection.getFinisher` and `SWFFinisher.getEmbeddedMovieClip`, then constructs `count` `BitmapClip` objects around that `MovieClip`. The 15-second stack still includes the `BitmapClip` constructor, so the player is inside the first clip, not a later pooled copy. The global cache cannot help yet. The first flatten has to draw the frames.

The interpreter timeout checks wall time every 10,000 actions, from the start of the player tick. It does not run while a native call such as `BitmapData.draw` is inside wgpu. When that call returns, the next check can fire on the following bytecode. The loop's next interpreted call is `search`. A stack that is topped by `search` after 15 seconds means the tick has been inside this frame loop for 15 seconds. It does not mean the 20-byte cache probe itself is a 15-second loop.

## Why `flash.ocx` gets through

Both players run this bytecode. The difference is the cost of one iteration.

In ActiveX Flash Player:

- `BitmapData` is a CPU pixel buffer in the player.
- `BitmapData.draw` rasterizes the current frame with the native renderer.
- AVM2 is JIT-compiled. `gotoAndStop`, `getBounds`, the `Math.ceil` calls, and the cache probe are cheap next to that rasterizer.
- `Capabilities.playerType` is `"ActiveX"`.

In this desktop Ruffle build:

- `BitmapData.draw` of a display object always takes the GPU path. The CPU blit fast path in `operations::draw` only applies when the source is already a `BitmapData` and the matrix is a plain translation. A smoke `MovieClip` does not qualify.
- Each draw records an offscreen wgpu pass at stage quality. The default quality is `High` (4x MSAA). The pass copies the current texture into the MSAA target, draws the clip, and resolves it back. `render_offscreen` returns a `SyncHandle` and does not read the pixels back. Readback waits until something calls `getPixel` / `getPixel32`. This loop does not.
- `BitmapData`'s constructor still allocates and fills a full CPU buffer (`width * height` premultiplied colors) before that draw. `bitmap_handle` then uploads that buffer to a texture so the offscreen pass has a starting image. For a transparent `new BitmapData(w, h, true, 0)` the upload is a full-frame copy of zeros, and the MSAA path copies it again.
- The AVM2 interpreter runs the ActionScript around those calls. Ruffle's optimizer is peephole and type-aware cleanup. It does not compile this loop.
- `Capabilities.playerType` is `"StandAlone"` (the wasm build reports `"PlugIn"`). Both players have already entered `flattenDisplayObject`, so this stall is not a branch that only the ActiveX player takes. The player-type mismatch can still matter later, after the pool init returns.

`flash.ocx` finishes the same frame loop quickly enough that `InitObjectPoolManagerCommand` returns and the init chain can reach `InitConnectionCommand`. Ruffle is still inside the first `BitmapClip` constructor when the 15-second warning fires. The script limit in the headed player is 60 seconds. Hitting that limit aborts the script and leaves the pool unfinished, which also never opens the socket.

## The Ruffle comment, and what it does and does not mean

Ruffle developers have looked at profiles of `BitmapData.draw` immediately followed by `getPixel` and written, in substance, that they are not sure how much they can do about it.

That pattern is real, and it is a bad fit for this renderer:

- `draw` of a display object is a GPU render.
- `getPixel` / `getPixel32` need CPU pixels, so the first read copies the texture back.
- Games then call `getPixel32` once per pixel from interpreted ActionScript. Flash's JIT turns that into a pointer walk. Ruffle pays a native call per pixel. Bulk APIs such as `getVector` exist, but the SWF does not call them, and the optimizer does not rewrite the loop into one.

Open pull requests do not fix that pattern or this boot path:

- [#23530](https://github.com/ruffle-rs/ruffle/pull/23530) and [#23529](https://github.com/ruffle-rs/ruffle/pull/23529) batch many `draw` / `copyPixels` calls into one GPU submit. They help games that draw tens of thousands of times per frame. They do not make one clip's per-frame rasterize cheap, and keeping pixels on the GPU makes a later `getPixel` more expensive.
- [#23006](https://github.com/ruffle-rs/ruffle/pull/23006) speeds up CPU `copyPixels` and blends. Merged [#24490](https://github.com/ruffle-rs/ruffle/pull/24490) already did the row-copy version of that. `flattenDisplayObject` does not call `copyPixels`.
- Draft [#23864](https://github.com/ruffle-rs/ruffle/pull/23864) and [#23926](https://github.com/ruffle-rs/ruffle/pull/23926) are an integer interpreter. The published result is about 20–50% on loops that pull pixels once with `getPixels` and then use `domainMemory`. [#23926](https://github.com/ruffle-rs/ruffle/pull/23926) has no branches yet. A native `draw` per frame is outside that speedup.

So the comment is right about a general `draw` + per-pixel `getPixel` loop: Ruffle has no open change that makes an arbitrary loop of that shape as fast as Flash, and the project is not treating a JIT as something about to land.

It is not a statement that `BitmapData.draw` is unsupported. Draw works. It is too expensive, repeated once per frame, inside interpreted setup, during the first smoke-pool `BitmapClip`.

This game's stall is the frame loop above, not a `getPixel32` scan. An earlier pass assumed `BitmapClip.search` walked pixels because the name and the stack looked like a bounds search. The method body says otherwise.

## What a local Ruffle change could still try

The sibling checkout is already what this client builds (`[patch]` in `Cargo.toml` points at `../ruffle`). A change there is the right place. It does not require teaching the interpreter to run arbitrary pixel loops at ActiveX speed.

The work that matches this bytecode:

- Make `new BitmapData(w, h, true, 0)` followed by `draw(displayObject)` skip allocating and uploading a full transparent CPU buffer. The offscreen pass has to clear the GPU target to transparent instead of copying an uploaded zero texture, including the 4x MSAA copy. The CPU buffer only needs to exist if something later reads pixels. Smoke playback renders the GPU texture.
- If one frame's `getBounds` is far larger than the art, that dominates everything else. The constructor accepts anything up to the player's undocumented cap (`width` and `height` up to `0x6666666`, and `width * height` up to `0x20000000` for SWF version 13+). `main.swf` is version 21. A bad bounds rect becomes a huge texture on every frame of this loop. Confirming the width and height of the `BCBitmapData` constructed inside `flattenDisplayObject` is the check that decides whether the fix is "skip the zero upload" or "the bounds are wrong".

Replacing `search` with a Rust opaque-bounds scan would not run this code. `search` is the cache probe.

## Client-side gaps that are not this stall

These are real differences from the browser player and can matter after pool init:

- The headed and headless players are built without `with_external_interface`. `ExternalInterface.available` is false. The movie contains `JavaScriptDelayedCallsProxy` and `JavaScriptCallNotification`. No uncaught error 2067 is logged, so those calls are either guarded or not reached yet.
- `playerType` is `StandAlone` rather than `ActiveX`.
- There is no `port` flashvar. The socket port is expected to come from profile or settings after `InitConnectionCommand`, which has not run.

None of those open the socket while `createEngineSmokePool` is still constructing the first `BitmapClip`.
