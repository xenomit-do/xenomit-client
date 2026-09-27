# xenomit-client notes

## Build
- Ruffle comes from the sibling checkout `../ruffle` (`[patch]` in Cargo.toml).
- The `jpegxr` feature needs libclang: set `LIBCLANG_PATH=C:\Users\Yagiz\AppData\Roaming\Python\Python313\site-packages\clang\native` before `cargo build --release`.
- Tests: `cargo test` (client, includes `swf_patch`), `cargo test -p ruffle_render atf` in `../ruffle`.

## Run
`target/release/xenomit_client.exe --domain tr3 --dosid <dosid> --script-timeout 600 --smoke-pool 50`
- `--smoke-pool N` patches `main.swf` on download: caps engine/rocket smoke pool sizes in the embedded game XML and swaps the matching `gameXMLHash`. Without it, `ObjectPoolManager.init()` builds ~18k `BitmapClip`s and takes ~85 s, because every `MovieClip.gotoAndStop` runs `run_inner_goto_frame` over all orphan clips (quadratic).
- `--quality low|medium|high|best`, `--headless`.
- Diagnostics (`[probe]` timings, `[boot]` traces) are not in the working tree. They are saved in `../ruffle` as `stash@{0}` ("xenomit full diagnostics backup"). Re-apply with `git stash apply stash@{0}` when profiling.
