"""Local snapshot leases, inherited by Ray workers through their setup hook.

Recovery is cooperative, not a sandbox for hostile code running as this user.
No PID-based decisions and no cleanup of legacy/unregistered directories.
"""
import fcntl
import json
import importlib
import os
from pathlib import Path
import stat
import tempfile
import time

PREFIX = 'parrot-artifact-lease-'
FORMAT = 'parrot-snapshot-lease-v1'
SETUP_HOOK = 'parrot_protocol.ray_snapshot_lease.worker_setup'
_WORKER_LEASES = {}


def _identity(info):
    return [info.st_dev, info.st_ino]


def _open_directory(path):
    return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)


def _validate(path, directory, lease, require_payload=False):
    current = os.stat(path, follow_symlinks=False)
    info = os.fstat(directory)
    if (not stat.S_ISDIR(current.st_mode) or _identity(current) != _identity(info)
            or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700):
        raise ValueError('snapshot control directory identity/ownership changed')
    descriptor = os.open('manifest.json', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
    with os.fdopen(descriptor, 'rb') as source:
        registration = os.fstat(source.fileno())
        if (not stat.S_ISREG(registration.st_mode) or registration.st_uid != os.getuid()
                or stat.S_IMODE(registration.st_mode) != 0o600):
            raise ValueError('invalid snapshot registration file')
        raw = source.read(2049)
    if len(raw) > 2048:
        raise ValueError('snapshot registration exceeds its bound')
    try:
        manifest = json.loads(raw)
    except RecursionError as error:
        raise ValueError('snapshot registration nesting exceeds decoder limit') from error
    if (not isinstance(manifest, dict) or manifest.get('format') != FORMAT or
            manifest.get('uid') != os.getuid() or manifest.get('control') != _identity(info)):
        raise ValueError('invalid snapshot registration')
    for name in ('control', 'lease', 'payload'):
        identity = manifest.get(name)
        if (not isinstance(identity, list) or len(identity) != 2 or
                any(type(value) is not int or value < 0 for value in identity)):
            raise ValueError('invalid snapshot registered identity')
    lock_info = os.fstat(lease)
    named_lock = os.stat('lease', dir_fd=directory, follow_symlinks=False)
    if (not stat.S_ISREG(lock_info.st_mode) or lock_info.st_uid != os.getuid() or
            stat.S_IMODE(lock_info.st_mode) != 0o600 or
            manifest.get('lease') != _identity(lock_info) or _identity(named_lock) != _identity(lock_info)):
        raise ValueError('snapshot lease identity changed')
    try:
        payload = os.stat('payload', dir_fd=directory, follow_symlinks=False)
    except FileNotFoundError:
        if require_payload:
            raise
    else:
        if not stat.S_ISDIR(payload.st_mode) or manifest.get('payload') != _identity(payload):
            raise ValueError('snapshot payload identity changed')
    return manifest


def _remove_tree_at(parent, name, expected=None):
    """Delete through owned directory descriptors, including on Python 3.9."""
    info = os.stat(name, dir_fd=parent, follow_symlinks=False)
    if expected is not None and _identity(info) != expected:
        raise ValueError('snapshot payload was replaced')
    if not stat.S_ISDIR(info.st_mode):
        os.unlink(name, dir_fd=parent)
        return
    directory = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    try:
        if _identity(os.fstat(directory)) != _identity(info):
            raise ValueError('snapshot directory changed during removal')
        with os.scandir(directory) as entries:
            for entry in entries:
                _remove_tree_at(directory, entry.name)
        if _identity(os.stat(name, dir_fd=parent, follow_symlinks=False)) != _identity(info):
            raise ValueError('snapshot directory moved during removal')
        os.rmdir(name, dir_fd=parent)
    finally:
        os.close(directory)


def remove_snapshot_payload(directory, manifest):
    try:
        _remove_tree_at(directory, 'payload', manifest['payload'])
    except FileNotFoundError:
        # Retrying after a completed removal is safe; missing files inside a
        # partial removal are handled by the next attempt, not reported success.
        try:
            os.stat('payload', dir_fd=directory, follow_symlinks=False)
        except FileNotFoundError:
            return
        raise


def _remove_registered(path, directory, lease):
    manifest = _validate(path, directory, lease)
    remove_snapshot_payload(directory, manifest)
    os.unlink('manifest.json', dir_fd=directory)
    os.unlink('lease', dir_fd=directory)
    if _identity(os.stat(path, follow_symlinks=False)) != manifest['control']:
        raise ValueError('snapshot control directory moved during removal')
    os.rmdir(path)


class SnapshotLease:
    def __init__(self):
        self.directory = self.lease = None
        self.path = Path(tempfile.mkdtemp(prefix=PREFIX)).resolve()
        self.root = self.path / 'payload'
        try:
            self.directory = _open_directory(self.path)
            self.lease = os.open('lease', os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o600, dir_fd=self.directory)
            fcntl.flock(self.lease, fcntl.LOCK_SH)
            self.root.mkdir()
            manifest = dict(format=FORMAT, uid=os.getuid(), control=_identity(os.fstat(self.directory)),
                            lease=_identity(os.fstat(self.lease)), payload=_identity(self.root.stat()))
            descriptor = os.open('manifest.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                 0o600, dir_fd=self.directory)
            with os.fdopen(descriptor, 'w') as target:
                json.dump(manifest, target)
        except BaseException:
            self._close_descriptors()
            # No user source is copied until registration completes. Incomplete
            # allocation is left unregistered if cleanup itself fails.
            from shutil import rmtree
            rmtree(self.path)
            raise

    def _close_descriptors(self):
        for name in ('lease', 'directory'):
            value = getattr(self, name, None)
            if value is not None:
                setattr(self, name, None)
                os.close(value)

    def __del__(self):
        # Losing an in-memory owner releases only its lease. Recovery must still
        # acquire exclusivity against every worker before removing files.
        try:
            self._close_descriptors()
        except Exception:
            pass

    def close(self, timeout=5):
        if self.lease is None:
            return
        deadline = time.monotonic() + timeout
        while True:
            try:
                fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError('snapshot is still leased by a live worker')
                time.sleep(.01)
        try:
            _remove_registered(self.path, self.directory, self.lease)
        except FileNotFoundError:
            # A concurrent recovery may finish after an upgrade released our
            # shared lock. Do not confuse a missing manifest in a live directory
            # with successful retirement.
            if os.path.lexists(self.path):
                raise
        self._close_descriptors()


def collect_orphaned_snapshots(parent=None):
    report = dict(removed=[], busy=[], skipped=[], errors=[])
    parent = Path(tempfile.gettempdir() if parent is None else parent)
    for path in parent.iterdir():
        if not path.name.startswith(PREFIX):
            continue
        directory = lease = None
        try:
            directory = _open_directory(path)
            lease = os.open('lease', os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
            _validate(path, directory, lease)
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                report['busy'].append(str(path))
                continue
            _remove_registered(path, directory, lease)
            report['removed'].append(str(path))
        except (ValueError, FileNotFoundError, NotADirectoryError):
            report['skipped'].append(str(path))
        except OSError as error:
            report['errors'].append(dict(path=str(path), error=str(error)))
        finally:
            if lease is not None:
                os.close(lease)
            if directory is not None:
                os.close(directory)
    return report


def hold_snapshot_lease(control):
    if control in _WORKER_LEASES:
        return
    directory = _open_directory(control)
    lease = None
    try:
        lease = os.open('lease', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
        fcntl.flock(lease, fcntl.LOCK_SH | fcntl.LOCK_NB)
        _validate(control, directory, lease, require_payload=True)
        _WORKER_LEASES[control] = (directory, lease)
    except BaseException:
        if lease is not None:
            os.close(lease)
        os.close(directory)
        raise


def setup_hook(control, user_hook=None):
    def setup():
        hold_snapshot_lease(control)
        if user_hook is not None:
            hook = user_hook
            if isinstance(hook, str):
                from ray._private.runtime_env.setup_hook import (
                    RUNTIME_ENV_FUNC_IDENTIFIER, load_and_execute_setup_hook)
                if hook.startswith(RUNTIME_ENV_FUNC_IDENTIFIER):
                    # Job-level callable hooks are exported by Ray to GCS.
                    # Their runtime_env field contains only a display name;
                    # execute the inherited opaque reference using Ray's loader.
                    error = load_and_execute_setup_hook(hook)
                    if error is not None:
                        raise RuntimeError(error)
                    return
                module, name = hook.rsplit('.', 1)
                hook = getattr(importlib.import_module(module), name)
            hook()
    return setup


def effective_setup_hook(environment):
    """Resolve Ray's exported job hook or a child's explicit override."""
    from ray._private.ray_constants import WORKER_PROCESS_SETUP_HOOK_ENV_VAR
    variables = environment.get('env_vars', {})
    exported = variables.get(WORKER_PROCESS_SETUP_HOOK_ENV_VAR)
    configured = environment.get('worker_process_setup_hook')
    if exported == SETUP_HOOK:
        # Child actor options merge fields and env_vars independently. The
        # lease wrapper remains installed, but an explicit child hook replaces
        # the parent's user hook, as it would without this adapter.
        if configured is not None and configured != SETUP_HOOK:
            return configured
        return variables.get('PARROT_SNAPSHOT_USER_HOOK')
    return exported if exported is not None else configured


def worker_setup():
    # Actor runtime_env is JSON, unlike Ray's job-level callable-hook upload.
    # The stable import path and private Ray hook env key are both installed by
    # host_environment; children inherit them and acquire their own OS lease.
    # RuntimeContext.runtime_env is unsafe here: Ray has connected the worker,
    # but has not yet installed a current task (native code can segfault).
    # Resolve child overrides in the submitting parent, before serialization.
    from .ray_environments import install_child_hook_adapter
    from .ray_tasks import install_task_worker_lifetime
    control = os.environ['PARROT_SNAPSHOT_CONTROL']
    hold_snapshot_lease(control)
    install_child_hook_adapter()
    install_task_worker_lifetime()
    setup_hook(control, os.environ.get('PARROT_SNAPSHOT_USER_HOOK') or None)()
