"""Bounded IPC and retryable ownership of a deployment's actual child process."""
import json
import socket
import struct
import subprocess
import types

import pytest

from parrot_protocol import ray_job


class Fragmented:
    def __init__(self, data): self.data = bytearray(data)
    def recv(self, size):
        result = bytes(self.data[:min(size, 1)])
        del self.data[:len(result)]
        return result


def test_fragmented_unicode_frame_and_clean_eof():
    data = json.dumps({'value': '部署'}, ensure_ascii=False).encode()
    connection = Fragmented(struct.pack('!I', len(data)) + data)
    assert ray_job.receive(connection) == {'value': '部署'}
    assert ray_job.receive(connection) is None


@pytest.mark.parametrize('data', [b'\x00', struct.pack('!I', 3) + b'{}'])
def test_partial_frame_is_not_clean_eof(data):
    with pytest.raises(EOFError): ray_job.receive(Fragmented(data))


@pytest.mark.parametrize('length', [0, ray_job.MAX_CONTROL_BYTES + 1, 2**32 - 1])
def test_invalid_length_rejected_before_body_read(length):
    with pytest.raises(ValueError, match='length'):
        ray_job.receive(Fragmented(struct.pack('!I', length)))


def test_send_bounds_encoded_bytes_before_writing():
    sent = []
    connection = types.SimpleNamespace(sendall=sent.append)
    with pytest.raises(ValueError, match='limit'):
        ray_job.send(connection, '界' * (ray_job.MAX_CONTROL_BYTES // 3))
    assert sent == []
    ray_job.send(connection, {'ok': True})
    assert ray_job.receive(Fragmented(sent[0])) == {'ok': True}


def job():
    worker = types.SimpleNamespace(node=types.SimpleNamespace(address_info={'gcs_address': 'existing:1234'}))
    ray = types.SimpleNamespace(_private=types.SimpleNamespace(worker=types.SimpleNamespace(global_worker=worker)),
                                get_actor=lambda *a, **k: 'host')
    context = types.SimpleNamespace(get_node_id=lambda: 'node', get_job_id=lambda: '01000000')
    return ray_job.DeploymentJob(ray, context, '/snapshot/payload', {})


@pytest.mark.parametrize('fault', ['timeout', 'log', 'spawn'])
def test_start_allocation_failure_leaves_every_resource_closeable(monkeypatch, fault):
    owned = job(); endpoints = []
    original = socket.socketpair
    class Endpoint:
        def __init__(self, raw): self.raw = raw
        def settimeout(self, timeout):
            if fault == 'timeout': raise OSError('timeout fault')
            self.raw.settimeout(timeout)
        def close(self): self.raw.close()
        def fileno(self): return self.raw.fileno()
    def pair():
        endpoints.extend(original())
        return Endpoint(endpoints[0]), endpoints[1]
    def fail(*a, **k): raise OSError(fault + ' fault')
    monkeypatch.setattr(ray_job.socket, 'socketpair', pair)
    if fault == 'log': monkeypatch.setattr(ray_job, 'BoundedOutput', fail)
    if fault == 'spawn': monkeypatch.setattr(ray_job.subprocess, 'Popen', fail)
    try:
        with pytest.raises(OSError, match=fault): owned.start()
        owned.close()
        assert all(endpoint.fileno() == -1 for endpoint in endpoints)
        assert owned.log is None and owned.process is None
    finally:
        for endpoint in endpoints: endpoint.close()
        owned.close()


def test_configuration_limit_precedes_resource_allocation(monkeypatch):
    owned = job(); owned.runtime_env = {'env_vars': {'BIG': '界' * ray_job.MAX_CONTROL_BYTES}}
    def forbidden(): raise AssertionError('resources allocated before validation')
    monkeypatch.setattr(ray_job.socket, 'socketpair', forbidden)
    with pytest.raises(ValueError, match='configuration exceeds'): owned.start()


def test_close_sends_eof_and_preserves_timed_out_process_for_retry():
    owned = job(); owned.connection, peer = socket.socketpair()
    peer.settimeout(1)
    class Process:
        exited = False
        def poll(self): return 23 if self.exited else None
        def wait(self, timeout): raise subprocess.TimeoutExpired('owned driver', timeout)
    owned.process = process = Process()
    owned.log = log = ray_job.BoundedOutput()
    try:
        with pytest.raises(TimeoutError, match='not exited'): owned.close()
        assert peer.recv(1) == b''
        assert owned.process is process and owned.log is log and not log.closed
        process.exited = True
        owned.close(); owned.close()
        assert log.closed and owned.log is None
    finally:
        peer.close(); process.exited = True; owned.close()


@pytest.mark.parametrize('receipt', [None, {}, {'job': 'bogus', 'namespace': 'match'},
    {'job': '02000000', 'namespace': 'wrong'}, {'job': '01000000', 'namespace': 'match'},
    {'job': '02000000', 'namespace': 'match'}])
def test_start_validates_job_identity_and_retains_process_on_lost_receipt(monkeypatch, receipt):
    owned = job(); owned.namespace = 'match'
    process = types.SimpleNamespace(poll=lambda: 0)
    monkeypatch.setattr(ray_job.subprocess, 'Popen', lambda *a, **k: process)
    monkeypatch.setattr(ray_job, 'send', lambda *a: None)
    monkeypatch.setattr(ray_job, 'receive', lambda *a: receipt)
    try:
        if receipt == {'job': '02000000', 'namespace': 'match'}:
            assert owned.start() == 'host' and owned.job_id == '02000000'
        else:
            with pytest.raises(RuntimeError): owned.start()
        assert owned.process is process
    finally: owned.close()


@pytest.mark.parametrize('fault', [None, 'init', 'host'])
def test_driver_eof_or_startup_failure_shuts_down_own_job_and_closes_control(monkeypatch, fault):
    import ray
    from parrot_protocol import ray_snapshot_lease
    events = []
    config = dict(address='existing:1234', namespace='unique', node='0'*56, root='/payload',
                  runtime_env={'env_vars': {'PARROT_SNAPSHOT_CONTROL': '/control'}})
    parent, child = socket.socketpair(); parent.settimeout(1)
    ray_job.send(parent, config)
    parent.shutdown(socket.SHUT_WR)
    def init(**kwargs):
        assert events == [('lease', '/control')]
        assert kwargs == dict(address='existing:1234', namespace='unique', log_to_driver=False)
        events.append('init')
        if fault == 'init': raise RuntimeError('init fault')
    def options(**kwargs):
        assert kwargs['runtime_env']['env_vars']['PARROT_DEPLOYMENT_JOB'] == '02000000'
        assert kwargs['name'] == 'host' and kwargs['num_cpus'] == 0
        def create(root):
            assert root == '/payload'; events.append('host')
            if fault == 'host': raise RuntimeError('host fault')
            return object()
        return types.SimpleNamespace(remote=create)
    monkeypatch.setattr(ray_snapshot_lease, 'hold_snapshot_lease', lambda root:events.append(('lease',root)))
    monkeypatch.setattr(ray, 'init', init)
    monkeypatch.setattr(ray, 'get_runtime_context', lambda:types.SimpleNamespace(get_job_id=lambda:'02000000'))
    monkeypatch.setattr(ray, 'remote', lambda cls:types.SimpleNamespace(options=options))
    monkeypatch.setattr(ray, 'shutdown', lambda:events.append('shutdown'))
    try:
        if fault:
            with pytest.raises(RuntimeError, match=fault): ray_job.run_driver(child.detach())
        else:
            ray_job.run_driver(child.detach())
            assert ray_job.receive(parent) == {'job': '02000000', 'namespace': 'unique'}
        assert events[-1] == 'shutdown' and events.count('shutdown') == 1
        assert parent.recv(1) == b''
    finally: parent.close(); child.close()
