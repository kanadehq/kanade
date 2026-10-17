#!/usr/bin/env python3
"""Read-only check that a shipped kanade-agent still carries its release metadata.

    verify-agent-dist.py <binary|.tar.gz|.zip> <expected-version> <expected-platform>

Locates the dedicated section (`.kanade` in PE / ELF, `__DATA,__kanade` in
Mach-O) by walking the container's section table -- the same rule as
`kanade_shared::agent_metadata`, which the backend applies at publish time --
and checks the envelope, product, version and platform. Nothing is executed
or modified, so it is safe after optimisation, strip, macOS signing and
packaging. Exit 0 = ok, 1 = verification failed, 2 = usage.
"""

import json
import struct
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path


def sections(data):
    """Yield (segment, name, bytes) for every section of a PE / ELF / Mach-O."""
    if data[:2] == b"MZ":
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        if data[pe:pe + 4] != b"PE\0\0":
            raise ValueError("MZ file without a PE signature")
        nsec, = struct.unpack_from("<H", data, pe + 6)
        opt, = struct.unpack_from("<H", data, pe + 20)
        for i in range(nsec):
            h = pe + 24 + opt + i * 40
            name = data[h:h + 8].rstrip(b"\0").decode("latin-1")
            vsize, _, rawsize, rawptr = struct.unpack_from("<IIII", data, h + 8)
            yield "", name, data[rawptr:rawptr + (min(vsize, rawsize) if vsize else rawsize)]
    elif data[:4] == b"\x7fELF":
        if data[4] != 2 or data[5] != 1:
            raise ValueError("only little-endian ELF64 is shipped")
        shoff, = struct.unpack_from("<Q", data, 40)
        shentsize, shnum, shstrndx = struct.unpack_from("<HHH", data, 58)
        hdr = lambda i: struct.unpack_from("<IIQQQQ", data, shoff + i * shentsize)
        stroff = hdr(shstrndx)[4]
        for i in range(shnum):
            name_idx, _, _, _, off, size = hdr(i)
            end = data.index(b"\0", stroff + name_idx)
            yield "", data[stroff + name_idx:end].decode("latin-1"), data[off:off + size]
    elif data[:4] == b"\xcf\xfa\xed\xfe":
        ncmds, = struct.unpack_from("<I", data, 16)
        off = 32
        for _ in range(ncmds):
            cmd, size = struct.unpack_from("<II", data, off)
            if cmd == 0x19:  # LC_SEGMENT_64
                nsects, = struct.unpack_from("<I", data, off + 64)
                for j in range(nsects):
                    s = off + 72 + j * 80
                    name = data[s:s + 16].rstrip(b"\0").decode("latin-1")
                    seg = data[s + 16:s + 32].rstrip(b"\0").decode("latin-1")
                    sz, = struct.unpack_from("<Q", data, s + 40)
                    fo, = struct.unpack_from("<I", data, s + 48)
                    yield seg, name, data[fo:fo + sz]
            off += size
    else:
        raise ValueError("not a PE, ELF64 or thin 64-bit Mach-O executable")


def verify(data, version, platform):
    macho = data[:4] == b"\xcf\xfa\xed\xfe"
    want = ("__DATA", "__kanade") if macho else ("", ".kanade")
    found = [d for seg, name, d in sections(data) if (seg, name) == want]
    if len(found) != 1:
        raise ValueError(f"expected exactly one {want[0]},{want[1]} section, found {len(found)}")
    raw = found[0]
    if len(raw) < 16 or raw[:8] != b"KNAGMETA":
        raise ValueError("bad metadata envelope")
    schema, flags, length = struct.unpack_from("<HHI", raw, 8)
    if schema != 1 or flags != 0 or 16 + length > len(raw) or any(raw[16 + length:]):
        raise ValueError("unsupported or corrupt metadata envelope")
    meta = json.loads(raw[16:16 + length])
    expected = {"product": "kanade-agent", "version": version, "platform": platform}
    if meta != expected:
        raise ValueError(f"metadata {meta} != expected {expected}")
    return meta


def binaries(path, tmp):
    name = path.name
    if name.endswith((".tar.gz", ".tgz")):
        with tarfile.open(path) as t:
            for m in t.getmembers():
                if m.isfile() and not Path(m.name).name.startswith("._"):
                    yield m.name, t.extractfile(m).read()
    elif name.endswith(".zip"):
        with zipfile.ZipFile(path) as z:
            for n in z.namelist():
                yield n, z.read(n)
    else:
        yield name, path.read_bytes()


def main():
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    path, version, platform = Path(sys.argv[1]), sys.argv[2], sys.argv[3]
    try:
        with tempfile.TemporaryDirectory() as tmp:
            items = list(binaries(path, tmp))
            if len(items) != 1:
                raise ValueError(f"{path}: expected exactly one file, found {len(items)}")
            member, data = items[0]
            meta = verify(data, version, platform)
    except Exception as e:  # noqa: BLE001 - report any failure as a verification error
        print(f"::error::{path}: agent release metadata check failed: {e}", file=sys.stderr)
        return 1
    print(f"{path} ({member}): {meta['product']} {meta['version']} {meta['platform']} ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
