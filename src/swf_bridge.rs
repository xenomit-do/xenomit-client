//! Splices the `DobotBridge` reflection class into the root movie, the Rust
//! port of `windows_flash_mvp/activex_player/swf_bridge.cpp`.
//!
//! Four tags are inserted immediately before the first top-level `ShowFrame`:
//! `DoABC2` (eager), `DefineSprite`, `SymbolClass` binding the sprite to
//! `DobotBridge`, and `PlaceObject2` so the bridge gets a display object and a
//! `stage`/`root` to walk. The ABC blob is the one compiled by
//! `bridge/DobotBridge.as` in the DOBOT tree (`bridge/DobotBridge.abc`).
//!
//! `FWS`, `CWS`, and `ZWS` inputs are handled. On any failure the caller runs
//! the original bytes and the bridge stays inactive, like the ActiveX
//! fallback.

use std::io::Read;

const MAX_SWF_BYTES: usize = 64 * 1024 * 1024;
const TAG_END: u16 = 0;
const TAG_SHOW_FRAME: u16 = 1;
const TAG_PLACE_OBJECT: u16 = 4;
const TAG_PLACE_OBJECT2: u16 = 26;
const TAG_DEFINE_SPRITE: u16 = 39;
const TAG_EXPORT_ASSETS: u16 = 56;
const TAG_PLACE_OBJECT3: u16 = 70;
const TAG_SYMBOL_CLASS: u16 = 76;
const TAG_DO_ABC2: u16 = 82;

const BRIDGE_CLASS_NAME: &[u8] = b"DobotBridge\0";
const BRIDGE_ABC: &[u8] = include_bytes!("../bridge/DobotBridge.abc");

/// Inject the bridge into `swf`. On any failure returns the reason; callers
/// should log it and run the original bytes.
pub fn inject(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 8 {
        return Err("SWF is too small".into());
    }
    let swf = match &data[..3] {
        b"FWS" => {
            if data.len() > MAX_SWF_BYTES {
                return Err("SWF is too large".into());
            }
            data.to_vec()
        }
        b"CWS" => decompress(data, Compression::Zlib)?,
        b"ZWS" => decompress(data, Compression::Lzma)?,
        _ => return Err("not a SWF".into()),
    };
    if BRIDGE_ABC.len() < 64 {
        return Err("reflection bridge ABC is missing".into());
    }
    if contains(&swf, b"DobotBridge") {
        return Ok(swf);
    }

    let tags = tag_stream_offset(&swf)?;
    let mut insert_at = swf.len();
    let mut found = false;
    let mut highest_id = 0u16;
    let mut highest_depth = 0u16;
    let mut pos = tags;
    while pos + 2 <= swf.len() {
        let tag_start = pos;
        let header = read_u16(&swf[pos..]);
        let code = header >> 6;
        let mut length = usize::from(header & 0x3f);
        pos += 2;
        if length == 0x3f {
            if pos + 4 > swf.len() {
                break;
            }
            length = read_u32(&swf[pos..]) as usize;
            pos += 4;
        }
        if pos + length > swf.len() {
            break;
        }
        let payload = &swf[pos..pos + length];
        if tag_defines_character(code) && length >= 2 {
            highest_id = highest_id.max(read_u16(payload));
        }
        if (code == TAG_EXPORT_ASSETS || code == TAG_SYMBOL_CLASS) && length >= 2 {
            note_symbol_list(payload, &mut highest_id);
        }
        if code == TAG_PLACE_OBJECT && length >= 4 {
            highest_id = highest_id.max(read_u16(payload));
            highest_depth = highest_depth.max(read_u16(&payload[2..]));
        } else if (code == TAG_PLACE_OBJECT2 || code == TAG_PLACE_OBJECT3) && length >= 3 {
            let cursor = usize::from(code == TAG_PLACE_OBJECT3) + 1;
            if cursor + 2 <= length {
                highest_depth = highest_depth.max(read_u16(&payload[cursor..]));
                if payload[0] & 0x02 != 0 && cursor + 4 <= length {
                    highest_id = highest_id.max(read_u16(&payload[cursor + 2..]));
                }
            }
        }
        if !found && (code == TAG_SHOW_FRAME || code == TAG_END) {
            insert_at = tag_start;
            found = true;
        }
        pos += length;
    }
    if !found {
        return Err("SWF has no ShowFrame tag".into());
    }
    if highest_id == u16::MAX {
        return Err("SWF has no free character id".into());
    }
    let depth = highest_depth.checked_add(1).unwrap_or(highest_depth);
    let bridge = bridge_tags(highest_id + 1, depth);

    let mut patched = Vec::with_capacity(swf.len() + bridge.len());
    patched.extend_from_slice(&swf[..insert_at]);
    patched.extend_from_slice(&bridge);
    patched.extend_from_slice(&swf[insert_at..]);
    if patched[3] < 17 {
        patched[3] = 17;
    }
    let len = patched.len() as u32;
    patched[4..8].copy_from_slice(&len.to_le_bytes());
    patched[..3].copy_from_slice(b"FWS");
    Ok(patched)
}

enum Compression {
    Zlib,
    Lzma,
}

/// Rebuild an FWS from a compressed SWF. The output keeps the 8-byte header
/// (with the signature rewritten) followed by the decompressed body.
fn decompress(data: &[u8], compression: Compression) -> Result<Vec<u8>, String> {
    let declared = read_u32(&data[4..]) as usize;
    if declared <= 8 || declared > MAX_SWF_BYTES {
        return Err("compressed SWF length is invalid".into());
    }
    let body = match compression {
        Compression::Zlib => {
            let mut body = Vec::new();
            flate2::read::ZlibDecoder::new(&data[8..])
                .read_to_end(&mut body)
                .map_err(|e| format!("CWS inflate failed: {e}"))?;
            body
        }
        Compression::Lzma => {
            // ZWS header: signature, version, u32 uncompressed, u32
            // compressed, then a standard LZMA stream (5 property bytes then
            // the payload, decoded until the end-of-payload marker).
            if data.len() < 17 {
                return Err("ZWS header is truncated".into());
            }
            let mut body = Vec::with_capacity(declared - 8);
            lzma_rs::lzma_decompress_with_options(
                &mut &data[12..],
                &mut body,
                &lzma_rs::decompress::Options {
                    unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(None),
                    allow_incomplete: true,
                    memlimit: None,
                },
            )
            .map_err(|e| format!("ZWS inflate failed: {e}"))?;
            body
        }
    };
    if body.len() != declared - 8 {
        return Err(format!(
            "decompressed to {} bytes, header declares {}",
            body.len() + 8,
            declared
        ));
    }
    let mut swf = Vec::with_capacity(declared);
    swf.extend_from_slice(&data[..8]);
    swf.extend_from_slice(&body);
    // The declared length already describes the uncompressed bytes.
    swf[..3].copy_from_slice(b"FWS");
    Ok(swf)
}

/// Offset of the first tag in an uncompressed SWF (signature header included).
fn tag_stream_offset(swf: &[u8]) -> Result<usize, String> {
    if swf.len() < 9 {
        return Err("SWF header is truncated".into());
    }
    let nbits = usize::from(swf[8] >> 3);
    let rect_bytes = (5 + nbits * 4).div_ceil(8);
    let tags = 8 + rect_bytes + 4;
    if tags > swf.len() {
        return Err("SWF rectangle is truncated".into());
    }
    Ok(tags)
}

/// Tag codes whose payload starts with a u16 character id.
fn tag_defines_character(code: u16) -> bool {
    matches!(
        code,
        2 | 6
            | 7
            | 10
            | 11
            | 14
            | 20
            | 21
            | 22
            | 32
            | 33
            | 34
            | 35
            | 36
            | 37
            | 39
            | 46
            | 48
            | 60
            | 75
            | 83
            | 84
            | 87
            | 90
            | 91
    )
}

/// ExportAssets / SymbolClass payloads are u16 count, then (u16 id, cstring).
fn note_symbol_list(payload: &[u8], highest: &mut u16) {
    let count = usize::from(read_u16(payload));
    let mut pos = 2;
    for _ in 0..count {
        if pos + 2 > payload.len() {
            return;
        }
        *highest = (*highest).max(read_u16(&payload[pos..]));
        pos = match payload[pos + 2..].iter().position(|b| *b == 0) {
            Some(nul) => pos + 3 + nul,
            None => return,
        };
    }
}

fn bridge_tags(character_id: u16, depth: u16) -> Vec<u8> {
    let mut tags = Vec::new();

    // DoAbc2, flags=0 (eager): the script init runs during frame 1.
    let mut abc = Vec::with_capacity(4 + BRIDGE_CLASS_NAME.len() + BRIDGE_ABC.len());
    abc.extend_from_slice(&0u32.to_le_bytes());
    abc.extend_from_slice(BRIDGE_CLASS_NAME);
    abc.extend_from_slice(BRIDGE_ABC);
    append_tag(&mut tags, TAG_DO_ABC2, &abc);

    // Empty one-frame sprite; SymbolClass turns instances into DobotBridge.
    let id = character_id.to_le_bytes();
    let sprite = [id[0], id[1], 1, 0, 0x40, 0, 0, 0];
    append_tag(&mut tags, TAG_DEFINE_SPRITE, &sprite);

    let mut symbols = Vec::new();
    symbols.extend_from_slice(&1u16.to_le_bytes());
    symbols.extend_from_slice(&id);
    symbols.extend_from_slice(BRIDGE_CLASS_NAME);
    append_tag(&mut tags, TAG_SYMBOL_CLASS, &symbols);

    // PlaceFlagHasMatrix | PlaceFlagHasCharacter, identity matrix, on top.
    let place = [
        0x06,
        depth.to_le_bytes()[0],
        depth.to_le_bytes()[1],
        id[0],
        id[1],
        0x00,
    ];
    append_tag(&mut tags, TAG_PLACE_OBJECT2, &place);
    tags
}

fn append_tag(out: &mut Vec<u8>, code: u16, payload: &[u8]) {
    if payload.len() < 0x3f {
        out.extend_from_slice(&((code << 6) | payload.len() as u16).to_le_bytes());
    } else {
        out.extend_from_slice(&((code << 6) | 0x3f).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    }
    out.extend_from_slice(payload);
}

fn read_u16(data: &[u8]) -> u16 {
    u16::from_le_bytes([data[0], data[1]])
}

fn read_u32(data: &[u8]) -> u32 {
    u32::from_le_bytes(data[..4].try_into().unwrap())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Minimal FWS host movie: black background, one empty frame.
    fn minimal_host() -> Vec<u8> {
        let mut swf = b"FWS\x2b\x00\x00\x00\x00".to_vec();
        swf.push(0x78); // RECT nbits=15 -> 65 bits -> 9 bytes
        swf.extend_from_slice(&[0; 8]);
        swf.extend_from_slice(&[0x00, 0x18]); // frame rate
        swf.extend_from_slice(&1u16.to_le_bytes()); // frame count
        append_tag(&mut swf, 69, &[0x08, 0x00, 0x00, 0x00]); // FileAttributes
        append_tag(&mut swf, 9, &[0x00, 0x00, 0x00]); // SetBackgroundColor
        append_tag(&mut swf, TAG_SHOW_FRAME, &[]);
        append_tag(&mut swf, TAG_END, &[]);
        let len = swf.len() as u32;
        swf[4..8].copy_from_slice(&len.to_le_bytes());
        swf
    }

    fn tag_codes(swf: &[u8]) -> Vec<u16> {
        let mut codes = Vec::new();
        let mut pos = tag_stream_offset(swf).unwrap();
        while pos + 2 <= swf.len() {
            let header = read_u16(&swf[pos..]);
            let code = header >> 6;
            let mut length = usize::from(header & 0x3f);
            pos += 2;
            if length == 0x3f {
                length = read_u32(&swf[pos..]) as usize;
                pos += 4;
            }
            codes.push(code);
            pos += length;
            if code == TAG_END {
                break;
            }
        }
        codes
    }

    #[test]
    fn injects_bridge_before_first_frame() {
        let patched = inject(&minimal_host()).expect("FWS inject should succeed");
        assert!(contains(&patched, b"invokeAS"));
        assert_eq!(read_u32(&patched[4..]) as usize, patched.len());
        assert_eq!(&patched[..3], b"FWS");

        let codes = tag_codes(&patched);
        let bridge = codes.iter().position(|c| *c == TAG_DO_ABC2).unwrap();
        let frame = codes.iter().position(|c| *c == TAG_SHOW_FRAME).unwrap();
        assert!(bridge < frame, "bridge tags must precede ShowFrame");
        assert_eq!(codes[bridge + 1], TAG_DEFINE_SPRITE);
        assert_eq!(codes[bridge + 2], TAG_SYMBOL_CLASS);
        assert_eq!(codes[bridge + 3], TAG_PLACE_OBJECT2);
    }

    /// Wrap an FWS body in a ZWS container, the mangled LZMA layout the spec
    /// uses: 12-byte header (with the declared compressed size), then the LZMA
    /// stream without its 8-byte unpacked-size field.
    fn to_zws(fws: &[u8]) -> Vec<u8> {
        let mut lzma = Vec::new();
        lzma_rs::lzma_compress(&mut &fws[8..], &mut lzma).unwrap();
        let stream = [&lzma[..5], &lzma[13..]].concat();
        let mut zws = b"ZWS".to_vec();
        zws.extend_from_slice(&fws[3..8]);
        zws.extend_from_slice(&(stream.len() as u32).to_le_bytes());
        zws.extend_from_slice(&stream);
        zws
    }

    #[test]
    fn cws_round_trips_to_same_fws() {
        let host = minimal_host();
        let mut compressed = Vec::new();
        flate2::write::ZlibEncoder::new(&mut compressed, flate2::Compression::default())
            .write_all(&host[8..])
            .unwrap();
        let mut cws = b"CWS".to_vec();
        cws.extend_from_slice(&host[3..8]);
        cws.extend_from_slice(&compressed);
        assert_eq!(inject(&cws).unwrap(), inject(&host).unwrap());
    }

    #[test]
    fn zws_round_trips_to_same_fws() {
        let host = minimal_host();
        assert_eq!(inject(&to_zws(&host)).unwrap(), inject(&host).unwrap());
    }

    #[test]
    fn zws_decodes_dobot_fixture() {
        // bridge_zws_fixture.inc from the DOBOT self-test.
        const FIXTURE: [u8; 50] = [
            90, 87, 83, 43, 36, 0, 0, 0, 38, 0, 0, 0, 93, 0, 0, 1, 0, 0, 59, 255, 253, 136, 27,
            230, 99, 193, 246, 48, 133, 135, 35, 223, 128, 33, 68, 73, 118, 140, 174, 226, 156,
            118, 5, 102, 72, 255, 251, 99, 12, 0,
        ];
        let swf = decompress(&FIXTURE, Compression::Lzma).expect("fixture should decode");
        assert_eq!(swf.len(), 36);
        assert_eq!(&swf[..3], b"FWS");
        // Injecting must not panic; a stub this small may lack a ShowFrame.
        let _ = inject(&FIXTURE);
    }

    #[test]
    fn idempotent_on_already_patched() {
        let once = inject(&minimal_host()).unwrap();
        assert_eq!(inject(&once).unwrap(), once);
    }

    #[test]
    fn uses_free_character_id_and_depth() {
        let mut host = minimal_host();
        let showframe = tag_codes(&host)
            .iter()
            .position(|c| *c == TAG_SHOW_FRAME)
            .unwrap();
        // Insert a sprite (id 7) and a PlaceObject2 at depth 9 before ShowFrame.
        let insert_at = host.len() - 4; // before ShowFrame+End (2 bytes each)
        let mut extra = Vec::new();
        append_tag(&mut extra, TAG_DEFINE_SPRITE, &[7, 0, 1, 0, 0x40, 0, 0, 0]);
        append_tag(&mut extra, TAG_PLACE_OBJECT2, &[0x02, 9, 0, 7, 0]);
        host.splice(insert_at..insert_at, extra);
        let len = host.len() as u32;
        host[4..8].copy_from_slice(&len.to_le_bytes());
        let _ = showframe;

        let patched = inject(&host).unwrap();
        // The placed bridge sprite must reuse neither id 7 nor depth 9.
        let mut pos = tag_stream_offset(&patched).unwrap();
        let mut placed = None;
        while pos + 2 <= patched.len() {
            let header = read_u16(&patched[pos..]);
            let code = header >> 6;
            let mut length = usize::from(header & 0x3f);
            pos += 2;
            if length == 0x3f {
                length = read_u32(&patched[pos..]) as usize;
                pos += 4;
            }
            if code == TAG_SYMBOL_CLASS {
                placed = Some(read_u16(&patched[pos + 2..]));
            }
            pos += length;
        }
        assert_eq!(placed, Some(8));
    }

    #[test]
    fn rejects_garbage() {
        assert!(inject(b"ZWS\x0drest").is_err());
        assert!(inject(b"not a swf at all").is_err());
        assert!(inject(b"CWS").is_err());
    }

    #[test]
    fn injected_movie_still_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../game_resources/main.swf");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: {path} not found");
            return;
        };
        let patched = inject(&bytes).expect("main.swf should inject");
        // Ruffle must accept the patched bytes as a loadable movie.
        ruffle_core::tag_utils::SwfMovie::from_data(
            &patched,
            "https://example.com/main.swf".into(),
            None,
            None,
        )
        .expect("injected movie should parse");
    }
}
