"""Test infrastructure must not borrow or shut down a caller's Ray cluster."""
from pathlib import Path
import os
import stat
import json
import signal
import socket
import subprocess
import sys

import pytest
import ray
import test_ray_adapter
import test_admin_v2
from parrot_protocol.ray_job import send, receive
import ray_test_cluster


@pytest.mark.parametrize('module', [test_ray_adapter, test_admin_v2])
def test_ray_fixture_requires_explicit_local_cluster_and_private_credentials(monkeypatch, tmp_path, module):
    calls = []
    inherited = tmp_path / 'caller-token'
    inherited.write_text('caller-secret')
    monkeypatch.setenv('RAY_ADDRESS', '198.51.100.9:6380')
    monkeypatch.setenv('RAY_AUTH_TOKEN', 'caller-inline-secret')
    monkeypatch.setenv('RAY_AUTH_TOKEN_PATH', str(inherited))
    monkeypatch.setenv('PARROT_RAY_TEST_EVIDENCE', str(tmp_path / 'fixture-evidence'))
    monkeypatch.setattr(ray, 'is_initialized', lambda: False)
    def initialize(**kwargs):
        token = os.environ.get('RAY_AUTH_TOKEN_PATH')
        calls.append(dict(kwargs=kwargs, inline=os.environ.get('RAY_AUTH_TOKEN'), token=token,
                          mode=None if token is None else stat.S_IMODE(Path(token).stat().st_mode)))
    monkeypatch.setattr(ray, 'init', initialize)
    monkeypatch.setattr(ray, 'shutdown', lambda: calls.append('shutdown'))
    fixture = module.ray_ctx.__wrapped__()
    try:
        next(fixture)
        assert calls[0]['kwargs'].get('address') == 'local'
        assert calls[0]['inline'] is None
        assert calls[0]['token'] != str(inherited) and calls[0]['mode'] == 0o600
    finally:
        fixture.close()
    assert calls[-1] == 'shutdown'
    assert inherited.read_text() == 'caller-secret'
    assert os.environ['RAY_AUTH_TOKEN'] == 'caller-inline-secret'
    assert os.environ['RAY_AUTH_TOKEN_PATH'] == str(inherited)
    assert os.environ['RAY_ADDRESS'] == '198.51.100.9:6380'


@pytest.mark.parametrize('module', [test_ray_adapter, test_admin_v2])
def test_existing_ray_session_is_rejected_before_any_cluster_action(monkeypatch, module):
    calls = []
    monkeypatch.setattr(ray, 'is_initialized', lambda: True)
    monkeypatch.setattr(ray, 'init', lambda **kw: calls.append('init'))
    monkeypatch.setattr(ray, 'shutdown', lambda: calls.append('shutdown'))
    fixture = module.ray_ctx.__wrapped__()
    try:
        with pytest.raises(RuntimeError, match='already initialized'):
            next(fixture)
    finally:
        fixture.close()
    assert not calls


@pytest.mark.parametrize('fault', ['init', 'body', 'observation', 'shutdown'])
def test_failure_restores_environment_and_still_attempts_owned_shutdown(monkeypatch, tmp_path, fault):
    calls = []
    observations = []
    inherited = tmp_path / 'inherited-token'
    inherited.write_text('caller-secret')
    monkeypatch.setenv('RAY_AUTH_TOKEN_PATH', str(inherited))
    monkeypatch.setenv('RAY_AUTH_TOKEN', 'caller-inline-secret')
    monkeypatch.setattr(ray, 'is_initialized', lambda: False)
    def observe():
        observations.append(True)
        if fault == 'observation' and len(observations) == 3:
            raise OSError('injected observation failure')
        return set()
    def initialize(**kwargs):
        calls.append('init')
        if fault == 'init':
            raise OSError('injected init failure')
    def shutdown():
        calls.append('shutdown')
        if fault == 'shutdown':
            raise OSError('injected shutdown failure')
    monkeypatch.setattr(ray_test_cluster, 'descendants', observe)
    monkeypatch.setattr(ray, 'init', initialize)
    monkeypatch.setattr(ray, 'shutdown', shutdown)
    with pytest.raises((OSError, RuntimeError)):
        with ray_test_cluster.owned_ray_cluster(ray, evidence=tmp_path / 'owned'):
            if fault == 'body':
                raise OSError('injected business failure')
    assert calls == ['init', 'shutdown']
    assert os.environ['RAY_AUTH_TOKEN_PATH'] == str(inherited)
    assert os.environ['RAY_AUTH_TOKEN'] == 'caller-inline-secret'
    receipt = json.loads((tmp_path / 'owned/cluster-retirement.json').read_text())
    assert receipt['ok'] == (fault in ('init', 'body'))
    assert inherited.read_text() == 'caller-secret'


def test_owned_cluster_ignores_sentinel_address_and_preserves_sentinel(monkeypatch, tmp_path):
    tests = Path(__file__).resolve().parent
    protocol = tests.parent
    local, child = socket.socketpair()
    local.settimeout(45)
    process = None
    try:
        env = dict(os.environ, PYTHONPATH=os.pathsep.join((str(protocol), str(tests))))
        with (tmp_path / 'sentinel.log').open('xb') as log:
            process = subprocess.Popen([sys.executable, str(tests / 'support/ray_sentinel.py'),
                str(child.fileno()), str(tmp_path / 'sentinel')], env=env,
                pass_fds=(child.fileno(),), stdin=subprocess.DEVNULL,
                stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        child.close()
        before = receive(local)
        assert before['ready'] and before['counter'] == 1
        monkeypatch.setenv('RAY_ADDRESS', before['address'])
        monkeypatch.setenv('RAY_AUTH_TOKEN', 'must-not-be-used')
        with ray_test_cluster.owned_ray_cluster(ray, evidence=tmp_path / 'owned'):
            @ray.remote
            class Echo:
                def echo(self):
                    return os.getpid()
            handle = Echo.remote()
            own_pid = ray.get(handle.echo.remote(), timeout=20)
            assert own_pid != before['pid']
            assert ray._private.worker.global_worker.node.address_info['gcs_address'] != before['address']
            from parrot_protocol.ray_gw import _kill_and_confirm
            _kill_and_confirm(ray, handle, handle.__ray_ready__)
        send(local, {'command': 'ping'})
        after = receive(local)
        assert after['counter'] == 2 and after['pid'] == before['pid']
        assert after['job'] == before['job'] and after['actor'] == before['actor']
        assert json.loads((tmp_path / 'owned/cluster-retirement.json').read_text())['ok']
        send(local, {'command': 'exit'})
        assert receive(local) == {'retired': True}
        assert process.wait(timeout=20) == 0
        (tmp_path / 'sentinel-proof.json').write_text(json.dumps(dict(
            before=before, after=after, owned_actor_pid=own_pid, sentinel_exit=0,
            independent_cluster=True, caller_environment_address_ignored=True), indent=2))
    finally:
        child.close(); local.close()
        if process is not None and process.poll() is None:
            try:
                process.wait(timeout=40)  # EOF gives the sentinel its owned cleanup path.
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGTERM)
                try: process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL); process.wait()
                pytest.fail('sentinel did not retire on owned control EOF')
