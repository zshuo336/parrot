"""OS leases across real processes; recovery must not guess from PIDs."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys

import pytest

from parrot_protocol import ray_artifacts, ray_snapshot_lease as leases


@pytest.fixture
def area(tmp_path, monkeypatch):
    cache = tmp_path/'cache';cache.mkdir()
    source = tmp_path/'source';source.mkdir();(source/'data').write_text('owned')
    monkeypatch.setattr(leases.tempfile, 'tempdir', str(cache))
    return cache, source


def child(code, *args, cache):
    package = str(Path(ray_artifacts.__file__).resolve().parents[1])
    return subprocess.Popen([sys.executable, '-u', '-c', code, *map(str,args)],
        env=dict(os.environ, PYTHONPATH=package, TMPDIR=str(cache)),
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def finish(process):
    if process.poll() is None:
        process.kill()
    process.wait(timeout=5)
    process.stdin.close();process.stdout.close();process.stderr.close()


def test_recovery_preserves_live_snapshot_and_legacy_directories(area):
    cache, source = area
    legacy = cache/'parrot-artifact-unregistered';legacy.mkdir()
    (legacy/'data').write_text('foreign')
    snapshot = ray_artifacts.Snapshot(source)
    try:
        result = leases.collect_orphaned_snapshots(cache)
        assert result['removed'] == [] and str(snapshot._lease.path) in result['busy']
        assert (Path(snapshot.root)/'data').read_text() == 'owned'
        assert (legacy/'data').read_text() == 'foreign'
    finally:
        snapshot.close()


def test_process_crash_releases_lease_and_recovery_removes_only_registered_snapshot(area):
    cache, source = area
    process = child("from parrot_protocol.ray_artifacts import Snapshot; import os,sys; "
                    "s=Snapshot(sys.argv[1]); print(s.root,flush=True); os._exit(23)", source, cache=cache)
    try:
        root = Path(process.stdout.readline().strip())
        assert process.wait(timeout=5) == 23 and root.is_dir()
        result = leases.collect_orphaned_snapshots(cache)
        assert result['removed'] == [str(root.parent)]
        assert not root.exists() and (source/'data').read_text() == 'owned'
    finally:
        finish(process)


def test_worker_lease_outlives_creator_and_protects_files_until_worker_exits(area):
    cache, source = area
    creator = child("from parrot_protocol.ray_artifacts import Snapshot; import os,sys; "
                    "s=Snapshot(sys.argv[1]); print(s.root,flush=True); sys.stdin.readline(); os._exit(23)", source, cache=cache)
    worker = None
    try:
        root = Path(creator.stdout.readline().strip())
        worker = child("from parrot_protocol.ray_snapshot_lease import hold_snapshot_lease; import sys; "
                       "hold_snapshot_lease(sys.argv[1]); print('leased',flush=True); sys.stdin.readline()",
                       root.parent, cache=cache)
        assert worker.stdout.readline().strip() == 'leased'
        creator.stdin.write('crash\n');creator.stdin.flush()
        assert creator.wait(timeout=5) == 23
        result = leases.collect_orphaned_snapshots(cache)
        assert str(root.parent) in result['busy'] and root.is_dir()
        worker.send_signal(signal.SIGTERM);worker.wait(timeout=5)
        assert leases.collect_orphaned_snapshots(cache)['removed'] == [str(root.parent)]
        assert not root.exists()
    finally:
        finish(creator)
        if worker is not None:finish(worker)


def test_normal_close_waits_for_worker_and_retry_does_not_lose_owner(area):
    cache, source = area
    snapshot = ray_artifacts.Snapshot(source)
    root = Path(snapshot.root)
    worker = child("from parrot_protocol.ray_snapshot_lease import hold_snapshot_lease; import sys; "
                   "hold_snapshot_lease(sys.argv[1]); print('leased',flush=True); sys.stdin.readline()",
                   root.parent, cache=cache)
    try:
        assert worker.stdout.readline().strip() == 'leased'
        with pytest.raises(TimeoutError, match='still leased'):
            snapshot._lease.close(timeout=.01)
        assert root.is_dir() and not leases.collect_orphaned_snapshots(cache)['removed']
    finally:
        finish(worker)
        snapshot.close()
    assert not root.exists()


@pytest.mark.parametrize('damage', ['manifest', 'payload-link', 'lease-link'])
def test_recovery_refuses_replaced_metadata_or_payload(area, damage):
    cache, source = area
    snapshot = ray_artifacts.Snapshot(source)
    control = snapshot._lease.path
    snapshot._lease._close_descriptors()
    foreign = source/'foreign';foreign.write_text('must survive')
    if damage == 'manifest':
        path = control/'manifest.json';saved=path.read_bytes();path.write_text('{}')
        restore = lambda: path.write_bytes(saved)
    else:
        name = 'payload' if damage == 'payload-link' else 'lease'
        path = control/name;parked=control/('saved-'+name);path.rename(parked)
        path.symlink_to(source if name=='payload' else foreign, target_is_directory=name=='payload')
        def restore():
            path.unlink();parked.rename(path)
    try:
        result = leases.collect_orphaned_snapshots(cache)
        assert result['removed'] == [] and control.exists()
        assert foreign.read_text() == 'must survive'
    finally:
        restore()
        assert leases.collect_orphaned_snapshots(cache)['removed'] == [str(control)]


def test_setup_hook_takes_lease_before_user_hook_and_keeps_it_for_worker_lifetime(area):
    cache, source = area
    snapshot = ray_artifacts.Snapshot(source)
    control = str(snapshot._lease.path)
    snapshot._lease._close_descriptors()
    calls = []
    def user_hook():
        assert leases.collect_orphaned_snapshots(cache)['busy'] == [control]
        calls.append('user')
    try:
        leases.setup_hook(control, user_hook)()
        assert calls == ['user'] and Path(snapshot.root).is_dir()
    finally:
        directory, lease = leases._WORKER_LEASES.pop(control)
        os.close(lease);os.close(directory)
    assert leases.collect_orphaned_snapshots(cache)['removed'] == [control]


@pytest.mark.parametrize('name', ['manifest.json', 'lease'])
def test_recovery_does_not_block_on_fifo_metadata(area, name):
    cache, source = area
    snapshot = ray_artifacts.Snapshot(source)
    control = snapshot._lease.path
    snapshot._lease._close_descriptors()
    path = control/name; saved = control/(name+'.saved'); path.rename(saved)
    os.mkfifo(path, 0o600)
    process = child("from parrot_protocol.ray_snapshot_lease import collect_orphaned_snapshots; "
                    "import json,sys; print(json.dumps(collect_orphaned_snapshots(sys.argv[1])))",
                    cache, cache=cache)
    try:
        out, err = process.communicate(timeout=5)
        assert process.returncode == 0, err
        result = json.loads(out)
        assert result['removed'] == [] and str(control) in result['skipped']
        assert (Path(snapshot.root)/'data').read_text() == 'owned'
    finally:
        finish(process)
        path.unlink();saved.rename(path)
        assert leases.collect_orphaned_snapshots(cache)['removed'] == [str(control)]


@pytest.mark.parametrize('raw', [b'\xff', b'{', b' ' * 2049, b'[' * 1000 + b'0' + b']' * 1000],
                         ids=['encoding', 'syntax', 'size', 'nesting'])
def test_unreadable_registration_is_preserved_without_stopping_scan(area, raw):
    cache, source = area
    damaged = ray_artifacts.Snapshot(source)
    valid = ray_artifacts.Snapshot(source)
    damaged._lease._close_descriptors();valid._lease._close_descriptors()
    path = damaged._lease.path/'manifest.json';saved = path.read_bytes();path.write_bytes(raw)
    try:
        result = leases.collect_orphaned_snapshots(cache)
        assert result['removed'] == [str(valid._lease.path)]
        assert str(damaged._lease.path) in result['skipped']
        assert (Path(damaged.root)/'data').read_text() == 'owned'
    finally:
        path.write_bytes(saved)
        leases.collect_orphaned_snapshots(cache)


def test_failed_payload_removal_keeps_registration_for_recovery_retry(area, monkeypatch):
    cache, source = area
    snapshot = ray_artifacts.Snapshot(source)
    control = snapshot._lease.path
    snapshot._lease._close_descriptors()
    remove = leases.remove_snapshot_payload
    def partial_failure(directory, manifest):
        os.unlink(Path(snapshot.root)/'data')
        raise PermissionError('injected partial removal')
    monkeypatch.setattr(leases, 'remove_snapshot_payload', partial_failure)
    result = leases.collect_orphaned_snapshots(cache)
    assert not result['removed'] and result['errors'][0]['path'] == str(control)
    assert (control/'manifest.json').is_file() and Path(snapshot.root).is_dir()
    monkeypatch.setattr(leases, 'remove_snapshot_payload', remove)
    assert leases.collect_orphaned_snapshots(cache)['removed'] == [str(control)]
    assert (source/'data').read_text() == 'owned'


def test_concurrent_collectors_remove_orphan_once(area):
    cache, source = area
    snapshot = ray_artifacts.Snapshot(source)
    control = str(snapshot._lease.path)
    snapshot._lease._close_descriptors()
    code = ("from parrot_protocol.ray_snapshot_lease import collect_orphaned_snapshots; "
            "import json,sys; sys.stdin.readline(); print(json.dumps(collect_orphaned_snapshots(sys.argv[1])))")
    processes = [child(code, cache, cache=cache) for _ in range(3)]
    try:
        for process in processes:
            process.stdin.write('go\n');process.stdin.flush()
        reports = []
        for process in processes:
            out, err = process.communicate(timeout=5)
            assert process.returncode == 0, err
            reports.append(json.loads(out))
        assert sum(report['removed'].count(control) for report in reports) == 1
        assert all(not report['errors'] for report in reports)
        assert not Path(control).exists() and (source/'data').read_text() == 'owned'
    finally:
        for process in processes:finish(process)


@pytest.mark.parametrize('identity', [None, [1], [True, False]])
def test_missing_payload_still_requires_valid_registered_identity(area, identity):
    cache, source = area
    snapshot = ray_artifacts.Snapshot(source)
    control = snapshot._lease.path
    snapshot._lease._close_descriptors()
    root = Path(snapshot.root);(root/'data').unlink();root.rmdir()
    path = control/'manifest.json';saved = path.read_bytes()
    manifest = json.loads(saved)
    if identity is None:
        del manifest['payload']
    else:
        manifest['payload'] = identity
    path.write_text(json.dumps(manifest))
    try:
        result = leases.collect_orphaned_snapshots(cache)
        assert result['removed'] == [] and str(control) in result['skipped']
        assert control.is_dir()
    finally:
        # A broken implementation may already have removed this fixture.
        if control.exists():
            path.write_bytes(saved)
            leases.collect_orphaned_snapshots(cache)
