#!/usr/bin/env python3
"""Snapshot every classpath JAR into a deterministic, atomically published archive."""
import gzip
import hashlib
import io
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import zipfile


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def prepare(root):
    cp = root / "interop/jvm/target/cp.txt"
    original = cp.read_bytes()
    text = original.decode("utf-8").rstrip("\r\n")
    if not text or "\n" in text or "\r" in text:
        raise ValueError("classpath must contain one nonempty path list")
    dependencies = {}
    for entry in text.split(os.pathsep):
        if not entry:
            raise ValueError("empty classpath element")
        path = Path(entry)
        if not path.is_absolute() or not path.is_file() or path.suffix != ".jar":
            raise ValueError("missing or invalid absolute JAR path: " + entry)
        basename = path.name
        path = path.resolve()
        previous = dependencies.get(basename)
        if previous is not None and previous != path:
            raise ValueError("conflicting JAR basenames: " + str(previous) + " and " + str(path))
        dependencies[basename] = path

    output = root / "gw-jvm-libs.tar"
    descriptor, name = tempfile.mkstemp(prefix=output.name + ".", dir=root)
    temporary = Path(name)
    fingerprints = {}
    try:
        with os.fdopen(descriptor, "wb") as raw:
            with gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.PAX_FORMAT) as archive:
                    for basename, path in sorted(dependencies.items()):
                        data = path.read_bytes()
                        with zipfile.ZipFile(io.BytesIO(data)) as jar:
                            invalid = jar.testzip()
                            if invalid is not None:
                                raise ValueError("invalid JAR member: " + str(path) + ":" + invalid)
                        fingerprints[path] = hashlib.sha256(data).hexdigest()
                        item = tarfile.TarInfo(basename)
                        item.size = len(data)
                        item.mode = 0o644
                        archive.addfile(item, io.BytesIO(data))
            raw.flush()
            os.fsync(raw.fileno())
        if cp.read_bytes() != original or any(digest(path) != value for path, value in fingerprints.items()):
            raise ValueError("classpath or dependency changed while preparing archive; retry after the build finishes")
        os.replace(temporary, output)
    finally:
        temporary.unlink(missing_ok=True)
    print(f"Prepared {len(dependencies)} verified JARs: {output.name}")


if __name__ == "__main__":
    try:
        prepare(Path(__file__).resolve().parents[2])
    except (OSError, ValueError, UnicodeError, zipfile.BadZipFile) as error:
        print("JVM dependency preparation failed: " + str(error), file=sys.stderr)
        sys.exit(1)
