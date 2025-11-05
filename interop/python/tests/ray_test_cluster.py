"""Owned local Ray clusters for integration tests, with retained diagnostic logs."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import secrets
import tempfile
import time

import psutil

from parrot_protocol.ray_startup import initialize


def refresh_authentication():
    # Ray 2.59 caches both native auth mode and token. Its own test helper
    # refreshes config before dropping the token cache, including at teardown.
    # Older supported Ray versions have no token-auth subsystem.
    try:
        from ray._private.authentication_test_utils import reset_auth_token_state
    except ModuleNotFoundError as error:
        if error.name != 'ray._private.authentication_test_utils':
            raise
    else:
        reset_auth_token_state()


def descendants():
    result = set()
    for process in psutil.Process().children(recursive=True):
        try:
            result.add((process.pid, process.create_time()))
        except psutil.NoSuchProcess:
            pass
    return result


def live(identities):
    result = []
    for pid, created in identities:
        try:
            process = psutil.Process(pid)
            if process.create_time() == created and process.status() != psutil.STATUS_ZOMBIE:
                result.append((pid, created))
        except psutil.NoSuchProcess:
            pass
    return result


@contextmanager
def owned_ray_cluster(ray, *, evidence=None, label='cluster'):
    """Never attach to an ambient cluster or retire a previously initialized one.

    The synchronous test owner is exclusive within this Python process. It only
    asks Ray to shut down its own new local cluster; process identities are used
    for observing retirement, never as permission to signal enumerated PIDs.
    """
    if ray.is_initialized():
        raise RuntimeError('Ray is already initialized; test must own a new local cluster')
    if evidence is not None:
        root = Path(evidence)
    else:
        configured = os.environ.get('PARROT_RAY_TEST_EVIDENCE')
        root = Path(configured if configured is not None else tempfile.mkdtemp(prefix='prt-evidence-')) / label
    root.mkdir(parents=True, exist_ok=False)
    short = Path(tempfile.mkdtemp(prefix='prt-', dir='/tmp'))
    runtime = root / 'runtime'; runtime.mkdir()
    alias = short / 'r'; alias.symlink_to(runtime.resolve(), target_is_directory=True)
    token = short / 'auth_token'
    with os.fdopen(os.open(token, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'w') as target:
        target.write(secrets.token_hex(32))
    keys = ('RAY_AUTH_TOKEN', 'RAY_AUTH_TOKEN_PATH', 'RAY_AUTH_MODE')
    previous = {key: os.environ.get(key) for key in keys}
    starting = False
    errors = []
    observed = set()
    baseline = set()
    try:
        baseline = descendants()
        os.environ.pop('RAY_AUTH_TOKEN', None)
        os.environ['RAY_AUTH_TOKEN_PATH'] = str(token)
        os.environ['RAY_AUTH_MODE'] = 'token'
        refresh_authentication()
        starting = True
        initialize(ray, dict(address='local', num_cpus=2, include_dashboard=False,
            log_to_driver=False, object_store_memory=80 * 1024 * 1024,
            _temp_dir=str(alias)))
        observed.update(descendants() - baseline)
        yield ray
    finally:
        try:
            if starting:
                try:
                    observed.update(descendants() - baseline)
                except BaseException as error:
                    errors.append(error)
                try:
                    ray.shutdown()
                except BaseException as error:
                    errors.append(error)
                try:
                    deadline = time.monotonic() + 30
                    while live(observed):
                        if time.monotonic() >= deadline:
                            raise TimeoutError('owned Ray processes have not retired')
                        time.sleep(0.02)
                except BaseException as error:
                    errors.append(error)
            receipt = dict(ok=not errors, started=starting, observed=sorted(observed),
                           errors=[type(error).__name__ for error in errors])
            (root / 'cluster-retirement.json').write_text(json.dumps(receipt, indent=2))
        finally:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
            refresh_authentication()
        if not errors:
            token.unlink()
            alias.unlink()
            short.rmdir()
        if errors:
            raise RuntimeError('test cluster retirement could not be confirmed') from errors[0]
