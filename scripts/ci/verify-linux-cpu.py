#!/usr/bin/env python3
"""Execute trusted Linux release artifacts on the original x86-64 CPU floor.

    verify-linux-cpu.py <dist-directory> <expected-version>

Requires qemu-x86_64 or qemu-x86_64-static (override with QEMU_X86_64).
Checks the packaged agent, backend and CLI, not the unsupported Linux GUI shim.
Only use with trusted local build artifacts: QEMU user mode is not a sandbox.
Exit 0 = all three passed, 1 = verification failed, 2 = usage.
"""

import os
import resource
import shutil
import signal
import struct
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path


TARGET = "x86_64-unknown-linux-musl"
BINARIES = ("kanade-agent", "kanade-backend", "kanade")
CPU = "qemu64,-sse3,-cx16,-lahf-lm"


def read_binary(path, name):
    """Accept only the exact single regular executable, never archive paths."""
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        if len(members) != 1:
            raise ValueError(f"expected exactly one archive member, found {len(members)}")
        member = members[0]
        if member.name != name:
            raise ValueError(f"unexpected archive member {member.name!r}; expected {name!r}")
        if member.type not in (tarfile.REGTYPE, tarfile.AREGTYPE):
            raise ValueError("archive member must be a regular file, not a link or special entry")
        if not member.mode & 0o111:
            raise ValueError("archive member is not executable")
        with archive.extractfile(member) as source:
            data = source.read()
        if not data or len(data) != member.size:
            raise ValueError("empty or truncated archive member")
        return data


def verify_elf(data):
    """Validate ELF64 structure and reject interpreter/shared-library dependencies."""
    if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
        raise ValueError("expected a little-endian ELF64 executable, version 1")
    (kind, machine, version, entry, phoff, shoff, flags, ehsize,
     phentsize, phnum, shentsize, shnum, shstrndx) = struct.unpack_from("<HHIQQQIHHHHHH", data, 16)
    if kind not in (2, 3) or machine != 62 or version != 1 or ehsize != 64 or flags != 0:
        raise ValueError("not a valid x86_64 ELF executable or static PIE")
    if phentsize != 56 or not 0 < phnum < 0xFFFF or phoff < ehsize:
        raise ValueError("invalid or unsupported ELF program header table")
    if phoff + phnum * phentsize > len(data):
        raise ValueError("truncated ELF program header table")
    if shoff:
        if (shoff < ehsize or shentsize != 64 or shnum == 0
                or shoff + shnum * shentsize > len(data)
                or shstrndx >= shnum):
            raise ValueError("invalid or unsupported ELF section header table")
        for i in range(shnum):
            _, section_type, _, _, offset, size, _, _, _, _ = struct.unpack_from(
                "<IIQQQQIIQQ", data, shoff + i * shentsize)
            if section_type not in (0, 8) and offset + size > len(data):
                raise ValueError("ELF section extends beyond the executable")
    elif shnum or shstrndx:
        raise ValueError("ELF section counts without a section header table")

    executable_entry = False
    for i in range(phnum):
        (segment_type, segment_flags, offset, address, _, file_size,
         memory_size, alignment) = struct.unpack_from("<IIQQQQQQ", data, phoff + i * phentsize)
        if offset + file_size > len(data):
            raise ValueError("ELF segment extends beyond the executable")
        if segment_type == 3:  # PT_INTERP
            raise ValueError("ELF requests a dynamic interpreter; a static executable is required")
        if segment_type == 1:  # PT_LOAD
            if file_size > memory_size:
                raise ValueError("ELF load segment file size exceeds memory size")
            if alignment > 1 and (alignment & (alignment - 1) or offset % alignment != address % alignment):
                raise ValueError("invalid ELF load segment alignment")
            if segment_flags & 1 and address <= entry < address + file_size:
                executable_entry = True
        if segment_type == 2:  # PT_DYNAMIC: static PIE is allowed, DT_NEEDED is not.
            if file_size % 16:
                raise ValueError("invalid ELF dynamic table size")
            terminated = False
            for pos in range(offset, offset + file_size, 16):
                tag, _ = struct.unpack_from("<qQ", data, pos)
                if tag == 0:  # DT_NULL
                    terminated = True
                    break
                if tag == 1:  # DT_NEEDED
                    raise ValueError("ELF needs shared libraries; a static executable is required")
            if not terminated:
                raise ValueError("unterminated ELF dynamic table")
    if not executable_entry:
        raise ValueError("ELF entry point is not inside an executable file-backed load segment")


def run_binary(qemu, executable, binary, version):
    command = [qemu, "-cpu", CPU, str(executable), "--version"]
    try:
        result = subprocess.run(command, capture_output=True, text=True, errors="replace", timeout=30)
    except subprocess.TimeoutExpired as error:
        raise ValueError(
            f"{binary} --version timed out after 30s on {CPU}; "
            f"stdout={error.stdout!r}, stderr={error.stderr!r}") from error
    if result.returncode:
        if result.returncode < 0:
            status = f"signal {signal.Signals(-result.returncode).name}"
        elif result.returncode == 128 + signal.SIGILL:
            status = f"exit status {result.returncode} (SIGILL)"
        else:
            status = f"exit status {result.returncode}"
        raise ValueError(
            f"{binary} --version failed on {CPU}: {status}; "
            f"stdout={result.stdout!r}, stderr={result.stderr!r}")
    expected = f"{binary} {version}"
    if result.stdout.strip() != expected:
        raise ValueError(
            f"unexpected --version stdout {result.stdout!r}; expected {expected!r}; "
            f"stderr={result.stderr!r}")


def main():
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    dist, version = Path(sys.argv[1]), sys.argv[2]
    override = os.environ.get("QEMU_X86_64")
    qemu = shutil.which(override) if override else (
        shutil.which("qemu-x86_64") or shutil.which("qemu-x86_64-static"))
    if not qemu:
        detail = f"QEMU_X86_64={override!r} is not executable" if override else "QEMU x86_64 user-mode emulator is missing"
        print(
            f"::error::{detail}; install qemu-user/qemu-user-static or set QEMU_X86_64 "
            "to an executable qemu-x86_64[-static] path",
            file=sys.stderr)
        return 1

    # Expected SIGILL failures must not leave large QEMU core files in the checkout.
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

    failed = False
    with tempfile.TemporaryDirectory(prefix="kanade-linux-cpu-") as tmp:
        for binary in BINARIES:
            name = f"{binary}-{TARGET}"
            archive = dist / f"{name}.tar.gz"
            try:
                data = read_binary(archive, name)
                verify_elf(data)
                # The output path and mode come only from us, never tar metadata.
                executable = Path(tmp) / binary
                executable.write_bytes(data)
                executable.chmod(0o700)
                run_binary(qemu, executable, binary, version)
            except Exception as error:  # noqa: BLE001 - report every artifact failure
                print(f"::error::{archive}: Linux x86-64 baseline check failed: {error}", file=sys.stderr)
                failed = True
                continue
            print(f"{archive}: {binary} {version} ok on {CPU}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
