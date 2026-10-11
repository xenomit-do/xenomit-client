"""Extract the DobotBridge DoABC payload into the bytes injected by Rust."""

from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path


def main() -> None:
    source = Path(sys.argv[1])
    dest = Path(sys.argv[2])
    swf = source.read_bytes()
    if swf[:3] == b"CWS":
        swf = b"FWS" + swf[3:8] + zlib.decompress(swf[8:])
    if swf[:3] != b"FWS":
        raise SystemExit(f"unsupported SWF signature {swf[:3]!r}")
    nbits = swf[8] >> 3
    pos = 8 + (5 + nbits * 4 + 7) // 8 + 4
    abc = None
    while pos + 2 <= len(swf):
        header = struct.unpack_from("<H", swf, pos)[0]
        pos += 2
        code, length = header >> 6, header & 0x3F
        if length == 0x3F:
            length = struct.unpack_from("<I", swf, pos)[0]
            pos += 4
        payload = swf[pos:pos + length]
        pos += length
        if code != 82:
            continue
        name_at = 4
        while payload[name_at] != 0:
            name_at += 1
        abc = payload[name_at + 1:]
        break
    if not abc or b"invokeAS" not in abc or b"stateAS" not in abc:
        raise SystemExit("compiled bridge is missing expected callbacks")
    dest.write_bytes(abc)
    print(f"wrote {dest} ({len(abc)} bytes)")


if __name__ == "__main__":
    main()
