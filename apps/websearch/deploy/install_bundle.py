"""Receive a bounded, verified release on stdin; never overwrite an existing root."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import os
import sys
import tarfile
import tempfile

MAX_ARCHIVE = 512 * 1024 * 1024
MAX_CONTENT = 1024 * 1024 * 1024
MAX_FILES = 10000


def receive(root, expected_digest, stream):
    root = Path(root)
    if not root.is_absolute() or root == Path('/') or '..' in root.parts:
        raise ValueError('release root must be an absolute non-root path without ..')
    if len(expected_digest) != 64 or any(c not in '0123456789abcdef' for c in expected_digest):
        raise ValueError('invalid archive digest')
    root.parent.mkdir(parents=True, exist_ok=True)
    # Serialize installers for this destination. An abandoned lock fails closed;
    # it is never taken over or removed by a competing installer.
    lock = root.with_name(root.name + '.install-lock')
    fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    try:
        os.close(fd)
        if os.path.lexists(root):
            raise ValueError('release root already exists; choose a new versioned root')
        with tempfile.TemporaryDirectory(prefix='.'+root.name+'-', dir=root.parent) as temporary:
            work = Path(temporary)
            archive = work/'bundle.tar.gz'
            digest, size = hashlib.sha256(), 0
            with archive.open('wb') as output:
                while True:
                    data = stream.read(65536)
                    if not data:
                        break
                    size += len(data)
                    if size > MAX_ARCHIVE:
                        raise ValueError('archive exceeds size limit')
                    digest.update(data)
                    output.write(data)
            if digest.hexdigest() != expected_digest:
                raise ValueError('archive digest mismatch')
            payload = work/'release'; payload.mkdir(mode=0o700)
            with tarfile.open(archive, 'r:gz') as tar:
                first = tar.next()
                if first is None or first.name != 'MANIFEST.json' or not first.isfile() or first.size > 1024*1024:
                    raise ValueError('missing bounded manifest')
                manifest = json.load(tar.extractfile(first))
                if not isinstance(manifest, dict) or not 1 <= len(manifest) <= MAX_FILES:
                    raise ValueError('invalid manifest')
                for name, sha in manifest.items():
                    path = PurePosixPath(name)
                    if (path.is_absolute() or '..' in path.parts or str(path) != name or
                            name == 'MANIFEST.json' or '\\' in name or not path.parts or
                            not isinstance(sha, str) or len(sha) != 64 or any(c not in '0123456789abcdef' for c in sha)):
                        raise ValueError('invalid manifest entry')
                remaining, total = set(manifest), 0
                for member in iter(tar.next, None):
                    if member.name not in remaining or not member.isfile():
                        raise ValueError('unexpected, duplicate or nonregular member')
                    total += member.size
                    if member.size < 0 or total > MAX_CONTENT:
                        raise ValueError('expanded archive exceeds size limit')
                    target = payload/member.name
                    target.parent.mkdir(parents=True, exist_ok=True)
                    sha = hashlib.sha256()
                    with tar.extractfile(member) as source, target.open('xb') as output:
                        while True:
                            chunk = source.read(65536)
                            if not chunk: break
                            sha.update(chunk); output.write(chunk)
                    if sha.hexdigest() != manifest[member.name]:
                        raise ValueError('file digest mismatch: '+member.name)
                    target.chmod(0o700 if member.mode & 0o111 else 0o600)
                    remaining.remove(member.name)
                if remaining:
                    raise ValueError('missing release members')
            (payload/'MANIFEST.json').write_text(json.dumps(manifest, indent=2)+'\n')
            if os.path.lexists(root):
                raise ValueError('release root appeared during installation')
            os.rename(payload, root)
        return dict(root=str(root), files=len(manifest), archive_sha256=expected_digest)
    finally:
        lock.unlink()


def main():
    try:
        result = receive(sys.argv[1], sys.argv[2], sys.stdin.buffer)
        print(json.dumps(result), flush=True)
    except Exception as error:
        print('websearch install: '+str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
