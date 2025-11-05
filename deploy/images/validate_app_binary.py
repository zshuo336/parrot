#!/usr/bin/env python3
"""Select a release artifact and check its ELF architecture before image assembly.

This is a format/architecture gate, not a complete loader or dependency verifier.
Dynamic ELF executables remain supported; their libraries must exist at runtime.
"""
import argparse
from pathlib import Path
import re
import struct
import sys


TARGETS = {
    "amd64": ("x86_64-unknown-linux-musl", 62),
    "arm64": ("aarch64-unknown-linux-musl", 183),
}


def artifact_path(root, target_os, arch, binary, triple):
    if target_os != "linux" or arch not in TARGETS:
        raise ValueError(f"unsupported image platform: {target_os}/{arch}")
    for name, value in [("BIN", binary), ("TRIPLE", triple)]:
        if (name == "TRIPLE" and value == ""):
            continue
        if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.+-]*", value):
            raise ValueError(f"{name} must be a single non-traversing path component")
    selected = TARGETS[arch][0] if triple == "auto" else triple
    return root / selected / "release" / binary


def validate_elf(data, arch):
    if len(data) < 64 or data[:7] != b"\x7fELF\x02\x01\x01":
        raise ValueError("expected a little-endian ELF64 executable (not Mach-O/PE)")
    (kind, machine, version, entry, phoff, _shoff, _flags, ehsize,
     phsize, phnum, _shsize, _shnum, _shstr) = struct.unpack_from("<HHIQQQIHHHHHH", data, 16)
    if machine != TARGETS[arch][1]:
        raise ValueError(f"ELF machine {machine} does not match image architecture {arch}")
    if kind not in (2, 3) or version != 1 or ehsize != 64:
        raise ValueError("invalid ELF executable header")
    if phsize != 56 or phnum == 0 or phnum == 65535 or phoff < 64 or phoff + phsize * phnum > len(data):
        raise ValueError("invalid or truncated ELF program headers")
    executable_entry = False
    for index in range(phnum):
        ptype, flags, offset, address, _physical, size, memory, _align = struct.unpack_from(
            "<IIQQQQQQ", data, phoff + index * phsize)
        if offset + size > len(data):
            raise ValueError("truncated ELF segment")
        if ptype == 1:
            if size > memory:
                raise ValueError("ELF load segment file size exceeds memory size")
            if flags & 1 and address <= entry < address + size:
                executable_entry = True
    if entry == 0 or not executable_entry:
        raise ValueError("ELF entry point is not backed by an executable load segment")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-os", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--bin", default="websearch")
    parser.add_argument("--triple", default="auto")
    parser.add_argument("--target-root", type=Path, default=Path("target"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        source = artifact_path(args.target_root, args.target_os, args.arch, args.bin, args.triple)
        data = source.read_bytes()
        validate_elf(data, args.arch)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        # Copy exactly the checked bytes, not a second read of a mutable source.
        args.output.write_bytes(data)
        args.output.chmod(0o755)
    except (OSError, ValueError) as error:
        print(f"application image validation failed: {error}", file=sys.stderr)
        return 1
    print(f"validated {source}: linux/{args.arch} ({len(data)} bytes) -> {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
