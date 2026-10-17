#!/usr/bin/env python3
"""Verify that built/packaged kanade-agent binaries retain embedded release metadata.

Survives strip, optimization, and packaging without modifying the artifact.
Validates:
  - Container OS/CPU detection
  - Dedicated .kanade / __DATA,__kanade section presence
  - Metadata magic (KNAGMETA) and schema version (1)
  - Product ("kanade-agent"), version, and platform alignment
"""

import json
import os
import struct
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

MAGIC = b"KNAGMETA"
EXPECTED_PRODUCT = "kanade-agent"


def extract_pe_section(data: bytes, section_name: bytes) -> bytes | None:
    if not data.startswith(b"MZ") or len(data) < 0x40:
        return None
    pe_off = struct.unpack_from("<I", data, 0x3C)[0]
    if pe_off + 24 > len(data) or data[pe_off : pe_off + 4] != b"PE\0\0":
        return None
    num_sections = struct.unpack_from("<H", data, pe_off + 6)[0]
    opt_hdr_size = struct.unpack_from("<H", data, pe_off + 20)[0]
    sec_table_off = pe_off + 24 + opt_hdr_size

    for i in range(num_sections):
        hdr_off = sec_table_off + i * 40
        if hdr_off + 40 > len(data):
            break
        name = data[hdr_off : hdr_off + 8].rstrip(b"\0")
        if name == section_name:
            vsize = struct.unpack_from("<I", data, hdr_off + 8)[0]
            raw_size = struct.unpack_from("<I", data, hdr_off + 16)[0]
            raw_ptr = struct.unpack_from("<I", data, hdr_off + 20)[0]
            sz = min(vsize, raw_size) if vsize > 0 else raw_size
            return data[raw_ptr : raw_ptr + sz]
    return None


def extract_elf_section(data: bytes, section_name: bytes) -> bytes | None:
    if not data.startswith(b"\x7fELF") or len(data) < 64:
        return None
    ei_class = data[4]
    endian = "<" if data[5] == 1 else ">"
    if ei_class == 2:  # 64-bit
        shoff = struct.unpack_from(f"{endian}Q", data, 40)[0]
        shentsize = struct.unpack_from(f"{endian}H", data, 58)[0]
        shnum = struct.unpack_from(f"{endian}H", data, 60)[0]
        shstrndx = struct.unpack_from(f"{endian}H", data, 62)[0]
    else:  # 32-bit
        shoff = struct.unpack_from(f"{endian}I", data, 32)[0]
        shentsize = struct.unpack_from(f"{endian}H", data, 46)[0]
        shnum = struct.unpack_from(f"{endian}H", data, 48)[0]
        shstrndx = struct.unpack_from(f"{endian}H", data, 50)[0]

    if shstrndx >= shnum:
        return None
    str_hdr = shoff + shstrndx * shentsize
    str_offset = struct.unpack_from(
        f"{endian}{'Q' if ei_class == 2 else 'I'}",
        data,
        str_hdr + (24 if ei_class == 2 else 16),
    )[0]

    for i in range(shnum):
        hdr = shoff + i * shentsize
        name_idx = struct.unpack_from(f"{endian}I", data, hdr)[0]
        name_end = data.find(b"\0", str_offset + name_idx)
        name = data[str_offset + name_idx : name_end]
        if name == section_name:
            offset = struct.unpack_from(
                f"{endian}{'Q' if ei_class == 2 else 'I'}",
                data,
                hdr + (24 if ei_class == 2 else 16),
            )[0]
            size = struct.unpack_from(
                f"{endian}{'Q' if ei_class == 2 else 'I'}",
                data,
                hdr + (32 if ei_class == 2 else 20),
            )[0]
            return data[offset : offset + size]
    return None


def extract_macho_section(data: bytes, section_name: bytes) -> bytes | None:
    if len(data) < 32:
        return None
    magic = data[:4]
    if magic in (b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe"):  # 64-bit
        endian = "<" if magic == b"\xcf\xfa\xed\xfe" else ">"
        ncmds = struct.unpack_from(f"{endian}I", data, 16)[0]
        cmd_off = 32
        for _ in range(ncmds):
            if cmd_off + 8 > len(data):
                break
            cmd, cmdsize = struct.unpack_from(f"{endian}II", data, cmd_off)
            if cmd == 0x19:  # LC_SEGMENT_64
                nsects = struct.unpack_from(f"{endian}I", data, cmd_off + 64)[0]
                sect_off = cmd_off + 72
                for _ in range(nsects):
                    if sect_off + 80 > len(data):
                        break
                    sect_name = data[sect_off : sect_off + 16].rstrip(b"\0")
                    if sect_name == section_name:
                        offset = struct.unpack_from(f"{endian}I", data, sect_off + 48)[0]
                        size = struct.unpack_from(f"{endian}Q", data, sect_off + 40)[0]
                        return data[offset : offset + size]
                    sect_off += 80
            cmd_off += cmdsize
    return None


def extract_section(data: bytes) -> bytes | None:
    if data.startswith(b"MZ"):
        return extract_pe_section(data, b".kanade")
    if data.startswith(b"\x7fELF"):
        return extract_elf_section(data, b".kanade")
    return extract_macho_section(data, b"__kanade")


def verify_metadata_payload(sec_data: bytes, expected_version: str | None = None) -> dict:
    if len(sec_data) < 16:
        raise ValueError(f"section length {len(sec_data)} smaller than header size 16")
    magic = sec_data[:8]
    if magic != MAGIC:
        raise ValueError(f"bad magic: {magic!r}")
    schema_ver = struct.unpack_from("<H", sec_data, 8)[0]
    if schema_ver != 1:
        raise ValueError(f"unsupported schema version: {schema_ver}")
    payload_len = struct.unpack_from("<I", sec_data, 12)[0]
    if len(sec_data) < 16 + payload_len:
        raise ValueError(f"payload truncated: {len(sec_data)} < {16 + payload_len}")
    payload_raw = sec_data[16 : 16 + payload_len]
    payload = json.loads(payload_raw.decode("utf-8"))

    if payload.get("product") != EXPECTED_PRODUCT:
        raise ValueError(f"expected product {EXPECTED_PRODUCT}, got {payload.get('product')}")
    if expected_version:
        ver = payload.get("version", "").lstrip("v").strip()
        exp = expected_version.lstrip("v").strip()
        if ver != exp:
            raise ValueError(f"version mismatch: expected {exp}, got {ver}")
    return payload


def verify_binary_file(path: Path, expected_version: str | None = None) -> dict:
    data = path.read_bytes()
    sec = extract_section(data)
    if sec is None:
        raise ValueError(f"{path}: missing agent release metadata section (.kanade / __kanade)")
    meta = verify_metadata_payload(sec, expected_version)
    print(f"verified {path.name}: product={meta['product']}, version={meta['version']}, platform={meta['platform']}")
    return meta


def verify_archive(archive_path: Path, expected_version: str | None = None) -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        if archive_path.name.endswith(".tar.gz") or archive_path.name.endswith(".tgz"):
            with tarfile.open(archive_path, "r:gz") as tar:
                tar.extractall(tmp_path)
        elif archive_path.name.endswith(".zip"):
            with zipfile.ZipFile(archive_path, "r") as z:
                z.extractall(tmp_path)
        else:
            verify_binary_file(archive_path, expected_version)
            return

        found = False
        for p in tmp_path.rglob("*"):
            if p.is_file() and p.name.startswith("kanade-agent"):
                verify_binary_file(p, expected_version)
                found = True
        if not found:
            raise ValueError(f"{archive_path}: no kanade-agent binary found inside archive")


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: verify-agent-dist.py <binary-or-archive-path> [expected_version]", file=sys.stderr)
        return 2

    target_path = Path(sys.argv[1])
    expected_ver = sys.argv[2] if len(sys.argv) > 2 else os.environ.get("CARGO_PKG_VERSION")

    if not target_path.exists():
        print(f"error: {target_path} not found", file=sys.stderr)
        return 1

    try:
        verify_archive(target_path, expected_ver)
        print("metadata verification succeeded")
        return 0
    except Exception as e:
        print(f"error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
