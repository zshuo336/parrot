#!/usr/bin/env python3
"""Verify image protocol bytes against the build input before importing them."""
import argparse
import hashlib
import importlib
import json
from pathlib import Path
import sys


def protocol_files(root):
    if root.is_symlink():
        raise ValueError(f"unexpected protocol directory symlink: {root}")
    if not root.is_dir():
        raise ValueError(f"packaged protocol directory missing: {root}")
    files = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if "__pycache__" in relative.parts or path.suffix == ".pyc":
            continue
        if path.is_symlink():
            raise ValueError(f"unexpected protocol symlink: {relative}")
        if path.is_file():
            files[relative.as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    if not files or "__init__.py" not in files:
        raise ValueError("packaged protocol is empty or missing __init__.py")
    return files


def verify(root, manifest):
    expected = json.loads(manifest.read_text())
    actual = protocol_files(root)
    if actual != expected:
        missing = sorted(expected.keys() - actual.keys())
        extra = sorted(actual.keys() - expected.keys())
        changed = sorted(key for key in actual.keys() & expected.keys() if actual[key] != expected[key])
        raise ValueError(f"packaged protocol differs: missing={missing}, extra={extra}, changed={changed}")
    sys.path.insert(0, str(root.parent))
    package = importlib.import_module("parrot_protocol")
    origin = Path(package.__file__).resolve()
    if origin != (root / "__init__.py").resolve():
        raise ValueError(f"protocol loaded from unexpected location: {origin}")
    print("RAY_IMAGE_PROTOCOL_OK " + json.dumps({"origin": str(origin), "files": len(actual)}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    verify(args.root, args.manifest)


if __name__ == "__main__":
    main()
