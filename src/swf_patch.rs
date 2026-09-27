//! Patches `main.swf` to shrink the ObjectPoolManager engine/rocket smoke
//! pools, so the startup `BitmapClip` construction loop stays under the script
//! timeout while we probe the real bottleneck.
//!
//! Only `CWS` (zlib) SWFs are handled. The embedded game XML lives in a
//! `DefineBinaryData` tag; every `poolSize="N"` inside the `<engineSmokes>` and
//! `<rocketSmokes>` regions is rewritten as `poolSize="<min(cap, N)>"` padded
//! with trailing spaces, so the uncompressed length is unchanged. The game
//! verifies the XML blob against an md5 stored in the ABC, so every occurrence
//! of the original hash is replaced with the hash of the patched blob.

use std::io::Read;

use md5::Digest;

/// Result of a successful patch; fields are exposed for tests and logging.
struct PatchOutcome {
    bytes: Vec<u8>,
    engine: usize,
    rocket: usize,
    old_hash: String,
    new_hash: String,
    occurrences: usize,
}

/// Patch `main.swf` bytes, capping engine/rocket smoke pool sizes at `cap`.
/// On any failure logs the reason and returns the input unchanged.
pub fn patch_main_swf(bytes: &[u8], cap: u32) -> Vec<u8> {
    match try_patch(bytes, cap) {
        Ok(outcome) => {
            eprintln!(
                "[patch] main.swf: {} engine + {} rocket poolSize entries capped at {}, \
                 gameXmlHash {}…→{}… ({} occurrences)",
                outcome.engine,
                outcome.rocket,
                cap,
                &outcome.old_hash[..8],
                &outcome.new_hash[..8],
                outcome.occurrences,
            );
            outcome.bytes
        }
        Err(reason) => {
            eprintln!("[patch] main.swf not patched: {reason}");
            bytes.to_vec()
        }
    }
}

fn try_patch(bytes: &[u8], cap: u32) -> Result<PatchOutcome, String> {
    if bytes.len() < 8 || &bytes[..3] != b"CWS" {
        return Err("not a zlib-compressed (CWS) SWF".into());
    }
    let header = &bytes[..8];
    let mut body = Vec::new();
    flate2::read::ZlibDecoder::new(&bytes[8..])
        .read_to_end(&mut body)
        .map_err(|e| format!("zlib decompress failed: {e}"))?;

    let tags_end = parse_tags(&body)?;
    let Some((blob_start, blob_end)) = find_game_xml_blob(&body, tags_end) else {
        return Err("no DefineBinaryData tag containing <engineSmokes>".into());
    };

    let mut blob = body[blob_start..blob_end].to_vec();
    let engine = cap_pool_sizes(&mut blob, b"<engineSmokes", b"</engineSmokes>", cap);
    let rocket = cap_pool_sizes(&mut blob, b"<rocketSmokes", b"</rocketSmokes>", cap);
    if engine + rocket == 0 {
        return Err("no poolSize attributes inside the smoke pool regions".into());
    }

    let old_hash = md5_hex(&body[blob_start..blob_end]);
    let new_hash = md5_hex(&blob);

    let mut patched = body;
    patched[blob_start..blob_end].copy_from_slice(&blob);
    let occurrences = replace_all(&mut patched, old_hash.as_bytes(), new_hash.as_bytes());
    if occurrences == 0 {
        return Err(format!("gameXmlHash {old_hash} not found in SWF"));
    }

    let mut compressed = Vec::new();
    {
        let mut encoder = flate2::write::ZlibEncoder::new(
            &mut compressed,
            flate2::Compression::default(),
        );
        use std::io::Write;
        encoder
            .write_all(&patched)
            .and_then(|_| encoder.finish())
            .map_err(|e| format!("zlib recompress failed: {e}"))?;
    }

    let mut out = Vec::with_capacity(8 + compressed.len());
    out.extend_from_slice(header);
    out.extend_from_slice(&compressed);
    Ok(PatchOutcome {
        bytes: out,
        engine,
        rocket,
        old_hash,
        new_hash,
        occurrences,
    })
}

fn md5_hex(data: &[u8]) -> String {
    md5::Md5::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Byte offset where the SWF tag list starts; also returns nothing else, but
/// validates the header enough to know the file parses.
fn parse_tags(body: &[u8]) -> Result<usize, String> {
    let Some(&first) = body.first() else {
        return Err("empty SWF body".into());
    };
    // RECT: 5-bit field count then 4 coordinates of that many bits.
    let nbits = usize::from(first >> 3);
    let rect_len = (5 + 4 * nbits).div_ceil(8);
    let tags_start = rect_len + 4; // u16 frame rate + u16 frame count
    if body.len() < tags_start {
        return Err("SWF header truncated".into());
    }
    Ok(tags_start)
}

/// Returns the data range of the first DefineBinaryData (code 87) tag whose
/// payload contains `<engineSmokes`. `tags_end` is the offset of the first tag.
fn find_game_xml_blob(body: &[u8], tags_start: usize) -> Option<(usize, usize)> {
    let mut offset = tags_start;
    while offset + 2 <= body.len() {
        let header = u16::from_le_bytes([body[offset], body[offset + 1]]);
        let code = header >> 6;
        let mut len = usize::from(header & 0x3f);
        let mut data_start = offset + 2;
        if len == 0x3f {
            if data_start + 4 > body.len() {
                return None;
            }
            len = u32::from_le_bytes(body[data_start..data_start + 4].try_into().unwrap())
                as usize;
            data_start += 4;
        }
        let data_end = data_start.checked_add(len)?;
        if data_end > body.len() {
            return None;
        }
        if code == 0 {
            return None;
        }
        if code == 87 && len >= 6 {
            // u16 id + u32 reserved, then the embedded data.
            let blob = &body[data_start + 6..data_end];
            if contains(blob, b"<engineSmokes") {
                return Some((data_start + 6, data_end));
            }
        }
        offset = data_end;
    }
    None
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|pos| from + pos)
}

/// Replace `search` with `replacement` (same length) throughout `data`;
/// returns the number of occurrences replaced.
fn replace_all(data: &mut [u8], search: &[u8], replacement: &[u8]) -> usize {
    debug_assert_eq!(search.len(), replacement.len());
    let mut count = 0;
    let mut offset = 0;
    while let Some(pos) = find(data, search, offset) {
        data[pos..pos + search.len()].copy_from_slice(replacement);
        count += 1;
        offset = pos + search.len();
    }
    count
}

/// Caps every `poolSize="N"` inside the `<region…>…</region>` slice of `blob`
/// at `cap`, padding with spaces after the closing quote so the byte length is
/// unchanged. Returns the number of entries rewritten.
fn cap_pool_sizes(blob: &mut [u8], open: &[u8], close: &[u8], cap: u32) -> usize {
    let Some(open_pos) = find(blob, open, 0) else {
        return 0;
    };
    let Some(open_end) = find(blob, b">", open_pos) else {
        return 0;
    };
    let Some(close_pos) = find(blob, close, open_end) else {
        return 0;
    };
    let region = &mut blob[open_end + 1..close_pos];
    let mut count = 0;
    let mut offset = 0;
    while let Some(pos) = find(region, b"poolSize=\"", offset) {
        let digits_start = pos + b"poolSize=\"".len();
        let digits_end = region[digits_start..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map(|p| digits_start + p)
            .unwrap_or(digits_start);
        if digits_end == digits_start || region.get(digits_end) != Some(&b'"') {
            offset = digits_start;
            continue;
        }
        let original: u32 = std::str::from_utf8(&region[digits_start..digits_end])
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(u32::MAX);
        let capped = original.min(cap);
        let mut entry = format!("poolSize=\"{capped}\"").into_bytes();
        entry.resize(digits_end + 1 - pos, b' ');
        region[pos..digits_end + 1].copy_from_slice(&entry);
        count += 1;
        offset = digits_end + 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAME_XML: &[u8] = br#"<gam><engineSmokes><s poolSize="2500"/><s poolSize="500"/><s poolSize="2500"/></engineSmokes><rocketSmokes><r poolSize="1000"/><r poolSize="50"/></rocketSmokes></gam>"#;

    /// Builds a minimal CWS SWF containing `tags`: (code, payload) pairs.
    fn make_swf(tags: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = vec![0x00]; // RECT: nbits=0, padded to one byte
        body.extend_from_slice(&[0x00, 0x18]); // frame rate 24.0
        body.extend_from_slice(&1u16.to_le_bytes()); // frame count
        for &(code, payload) in tags {
            if payload.len() >= 0x3f {
                let header = (code << 6) | 0x3f;
                body.extend_from_slice(&header.to_le_bytes());
                body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            } else {
                let header = (code << 6) | payload.len() as u16;
                body.extend_from_slice(&header.to_le_bytes());
            }
            body.extend_from_slice(payload);
        }
        body.extend_from_slice(&0u16.to_le_bytes()); // End tag

        let mut compressed = Vec::new();
        {
            let mut encoder = flate2::write::ZlibEncoder::new(
                &mut compressed,
                flate2::Compression::default(),
            );
            use std::io::Write;
            encoder.write_all(&body).unwrap();
            encoder.finish().unwrap();
        }
        let mut swf = Vec::new();
        swf.extend_from_slice(b"CWS");
        swf.push(21);
        swf.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
        swf.extend_from_slice(&compressed);
        swf
    }

    fn decompress(swf: &[u8]) -> Vec<u8> {
        assert_eq!(&swf[..3], b"CWS");
        let mut body = Vec::new();
        flate2::read::ZlibDecoder::new(&swf[8..])
            .read_to_end(&mut body)
            .unwrap();
        body
    }

    fn binary_data_tag(id: u16, data: &[u8]) -> Vec<u8> {
        let mut tag = Vec::new();
        tag.extend_from_slice(&id.to_le_bytes());
        tag.extend_from_slice(&0u32.to_le_bytes());
        tag.extend_from_slice(data);
        tag
    }

    #[test]
    fn caps_pool_sizes_and_replaces_hash() {
        let old_hash = md5_hex(GAME_XML);
        // Second tag stands in for the ABC holding the gameXmlHash default.
        let swf = make_swf(&[
            (87, &binary_data_tag(1, GAME_XML)),
            (82, old_hash.as_bytes()),
        ]);

        let outcome = try_patch(&swf, 100).expect("patch should succeed");
        assert_eq!(outcome.engine, 3);
        assert_eq!(outcome.rocket, 2);
        assert_eq!(outcome.occurrences, 1);
        assert_eq!(outcome.old_hash, old_hash);

        let body = decompress(&outcome.bytes);
        assert_eq!(body.len() + 8, u32::from_le_bytes(swf[4..8].try_into().unwrap()) as usize);

        let tags_end = parse_tags(&body).unwrap();
        let (start, end) = find_game_xml_blob(&body, tags_end).unwrap();
        let blob = &body[start..end];
        assert!(blob.starts_with(b"<gam"));
        let xml = std::str::from_utf8(blob).unwrap();
        assert!(xml.contains("poolSize=\"100\""), "{xml}");
        assert!(xml.contains("poolSize=\"50\""), "{xml}");
        assert!(!xml.contains("poolSize=\"2500\""), "{xml}");

        let new_hash = md5_hex(blob);
        assert_eq!(new_hash, outcome.new_hash);
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains(&new_hash));
        assert!(!body_str.contains(&old_hash));
    }

    #[test]
    fn leaves_uncompressed_swf_alone() {
        let fws = b"FWS\x0d\x2a\x00\x00\x00rest".to_vec();
        assert_eq!(patch_main_swf(&fws, 10), fws);
    }

    #[test]
    fn patches_real_main_swf() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../game_resources/main.swf");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("skipping: {path} not found");
            return;
        };
        let outcome = try_patch(&bytes, 100).expect("real main.swf should patch");
        assert_eq!(outcome.engine, 7);
        assert_eq!(outcome.rocket, 4);
        assert_eq!(outcome.occurrences, 1);

        let body = decompress(&outcome.bytes);
        let tags_end = parse_tags(&body).unwrap();
        let (start, end) = find_game_xml_blob(&body, tags_end).unwrap();
        assert!(body[start..end].starts_with(b"<gam"));
    }
}
