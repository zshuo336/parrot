"""Bounded, deployment-owned local source snapshots and synchronous imports.

This is source/version isolation, not a sandbox for untrusted Python code.
Production hosts live in separate Ray processes; the context adapter is for the
synchronous in-process executor and does not isolate arbitrary user threads.
"""
from contextlib import contextmanager
import importlib
from importlib.machinery import PathFinder
import os
from pathlib import Path
import re
import stat
import sys
import tempfile
import threading
from urllib.parse import unquote_to_bytes, urlsplit

from .ray_snapshot_lease import SnapshotLease, collect_orphaned_snapshots

MAX_FILES = 4096
MAX_BYTES = 64 * 1024 * 1024
MAX_DEPTH = 32
MAX_OWNED_BYTES = 256 * 1024 * 1024
_IMPORT_LOCK = threading.RLock()
_RESERVED = {'ray', 'parrot_protocol', 'sys', 'builtins', 'importlib', 'threading'}


def source_directory(module, uri):
    if (not isinstance(module, str) or not module or
            any(not part.isidentifier() for part in module.split('.'))):
        raise ValueError('artifact module must be a dotted Python identifier')
    if module.split('.')[0] in _RESERVED:
        raise ValueError('artifact cannot replace the gateway runtime')
    if uri is not None:
        if (not isinstance(uri, str) or not uri or any(ord(c) < 33 for c in uri)
                or re.search(r'%(?![0-9a-fA-F]{2})', uri)):
            raise ValueError('invalid artifact file URI')
        parts = urlsplit(uri)
        if (parts.scheme != 'file' or parts.netloc not in ('', 'localhost') or
                '?' in uri or '#' in uri or not parts.path.startswith('/')):
            raise ValueError('artifact URI requires an absolute local file directory')
        path = unquote_to_bytes(parts.path).decode('utf-8', errors='strict')
        if '\x00' in path:
            raise ValueError('NUL in artifact path')
        return str(Path(path).resolve())
    # In-memory modules are an explicit local embedding/testing facility.
    module_object = sys.modules.get(module)
    if module_object is not None and not getattr(module_object, '__file__', None):
        return None
    # Resolve only the top package; find_spec('pkg.entry') would execute pkg.
    top = module.split('.')[0]
    spec = PathFinder.find_spec(top)
    if spec is None or not spec.origin or spec.origin in ('built-in', 'frozen'):
        return None  # Resolution may belong to a remote worker; it must not import here.
    path = Path(spec.origin).resolve()
    if path.suffix != '.py':
        raise ValueError('artifact entry requires Python source')
    return str(path.parent.parent if spec.submodule_search_locations is not None else path.parent)


class Snapshot:
    def __init__(self, source, budget=MAX_BYTES):
        self.root = None
        self.size = 0
        source = Path(source).resolve()
        if not source.is_dir():
            raise ValueError('artifact source is not a directory')
        collect_orphaned_snapshots()
        self._lease = SnapshotLease()
        root = self._lease.root
        self.root = str(root)
        try:
            if source == root or source in root.parents:
                raise ValueError('artifact source contains the snapshot directory')
            entries = 0
            def copy(directory, target, depth):
                nonlocal entries
                if depth > MAX_DEPTH:
                    raise ValueError('artifact directory depth exceeded')
                with os.scandir(directory) as listing:
                    for entry in listing:
                        if entry.name in ('.git', '__pycache__') or entry.name.endswith(('.pyc', '.pyo')):
                            continue
                        entries += 1
                        if entries > MAX_FILES:
                            raise ValueError('artifact entry capacity exceeded')
                        info = entry.stat(follow_symlinks=False)
                        dest = target / entry.name
                        if stat.S_ISDIR(info.st_mode):
                            # Reject symlink replacement of a directory while copying.
                            fd = os.open(entry.name if isinstance(directory, int) else entry.path,
                                         os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                         dir_fd=directory if isinstance(directory, int) else None)
                            try:
                                current = os.fstat(fd)
                                if (current.st_dev, current.st_ino) != (info.st_dev, info.st_ino):
                                    raise ValueError('artifact directory changed during snapshot')
                                dest.mkdir()
                                copy(fd, dest, depth+1)
                            finally:
                                os.close(fd)
                        elif stat.S_ISREG(info.st_mode):
                            # scandir(fd) returns names; open relative to that owned fd.
                            parent_fd = directory if isinstance(directory, int) else None
                            fd = os.open(entry.name if parent_fd is not None else entry.path,
                                         os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent_fd)
                            with os.fdopen(fd, 'rb') as src, dest.open('wb') as out:
                                before = os.fstat(src.fileno())
                                if not stat.S_ISREG(before.st_mode):
                                    raise ValueError('artifact entry changed type')
                                while True:
                                    data = src.read(min(65536, max(0, min(MAX_BYTES, budget)-self.size)+1))
                                    if not data: break
                                    self.size += len(data)
                                    if self.size > min(MAX_BYTES, budget):
                                        raise ValueError('artifact byte capacity exceeded')
                                    out.write(data)
                                after = os.fstat(src.fileno())
                                if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                                    raise ValueError('artifact source changed during snapshot')
                        else:
                            raise ValueError('artifact symlinks and special files are unsupported')
            source_fd = os.open(source, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                copy(source_fd, root, 0)
            finally:
                os.close(source_fd)
            self.tops = {p.stem if p.is_file() else p.name for p in root.iterdir()
                         if p.is_dir() or p.suffix == '.py'}
            if self.tops & _RESERVED:
                raise ValueError('artifact cannot replace the gateway runtime')
        except BaseException:
            self.close()
            raise

    def close(self):
        if self.root is not None:
            self._lease.close()
            self.root = None


class Imports:
    def __init__(self, root, tops):
        self.root, self.tops = root, tops
        self.modules = {}

    def owns(self, name):
        return name.split('.')[0] in self.tops

    @contextmanager
    def enter(self):
        # Both the runtime and this lock are reentrant for callbacks during
        # construction/retirement. Ordinary host imports do not take this lock;
        # callers needing background imports must use the production Ray host.
        with _IMPORT_LOCK:
            saved = {n:m for n,m in list(sys.modules.items()) if self.owns(n)}
            saved_path = list(sys.path)
            saved_bytecode = sys.dont_write_bytecode
            for name in saved: sys.modules.pop(name, None)
            sys.modules.update(self.modules)
            sys.path.insert(0, self.root)
            sys.dont_write_bytecode = True
            importlib.invalidate_caches()
            try:
                yield
            finally:
                self.modules = {n:m for n,m in list(sys.modules.items()) if self.owns(n)}
                for name in self.modules: sys.modules.pop(name, None)
                sys.modules.update(saved)
                sys.path[:] = saved_path
                sys.dont_write_bytecode = saved_bytecode
                importlib.invalidate_caches()
