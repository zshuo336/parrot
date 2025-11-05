"""Management capacity through real Wire drivers and controlled local factories.

The socket/parser/dispatcher/registry/outbox are production code. Only the
component's user code is blocked; no Ray installation is needed for these cases.
"""
from contextlib import contextmanager
import socket
import sys
import threading
import types

import pytest
from parrot_protocol import admin_v2, ray_gw as gateway, wire


class Client:
    def __init__(self, state):
        self.peer, self.local = socket.socketpair()
        self.peer.settimeout(3)
        self.decoder = wire.FrameDecoder()
        self.errors = []
        self.closed = False
        def run():
            try:
                gateway._serve_connection_guarded(self.local, gateway.ParrotDispatcher(), None,
                    types.SimpleNamespace(), state)
            except BaseException as error:
                self.errors.append(error)
        self.thread = threading.Thread(target=run)
        self.thread.start()
        self.peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 0, '', '', wire.handshake_body('management')))
        assert self.read().ft == wire.FT_HANDSHAKE_ACK

    def read(self):
        while True:
            frame = self.decoder.next_frame()
            if frame is not None:
                return frame
            data = self.peer.recv(65536)
            assert data, 'connection closed before terminal reply'
            self.decoder.feed(data)

    def send(self, req_id, *, name=None, path='/reply', wire_size=None, header_size=0, kind='deploy'):
        if kind == 'deploy':
            command = dict(kind=kind, req_id=req_id, component=dict(name=name or f'item{req_id}', version='1',
                artifact=dict(kind='pymodule', module='management_contract'),
                instances=dict(kind='singleton'), config=b''))
        else:
            command = dict(kind=kind, req_id=req_id)
        key = 'k' * header_size
        def encoded():
            return wire.build_frame(wire.FT_SYSTEM_EVENT, req_id, path, key, admin_v2.encode_admin_cmd_v2(command))
        frame = encoded()
        if wire_size is not None:
            for _ in range(3):
                config = command['component']['config']
                command['component']['config'] = b'x' * (len(config) + wire_size-len(frame))
                frame = encoded()
            assert len(frame) == wire_size
        self.peer.sendall(frame)

    def reply(self):
        frame = self.read()
        assert frame.ft == wire.FT_SYSTEM_EVENT
        reply = admin_v2.decode_admin_reply_v2(frame.payload)
        assert frame.cid == reply['req_id'] and frame.path == '/reply'
        return reply

    def barrier(self):
        self.peer.sendall(wire.build_frame(wire.FT_HEARTBEAT, 99999, '', '', b''))
        frame = self.read()
        assert frame.ft == wire.FT_HEARTBEAT_ACK and frame.cid == 99999

    def close(self):
        if not self.closed:
            self.closed = True
            self.peer.close()
            self.thread.join(5)
            if self.thread.is_alive():
                self.local.shutdown(socket.SHUT_RDWR)
                self.thread.join(5)
            assert not self.thread.is_alive(), 'connection driver leaked'
            assert not self.errors, self.errors


@contextmanager
def fixture(monkeypatch):
    entered, release = threading.Event(), threading.Event()
    calls, threads = [], []
    module = types.ModuleType('management_contract')
    def entry(ctx):
        calls.append(ctx['path'])
        if ctx['path'].endswith('/blocked'):
            entered.set()
            assert release.wait(20), 'test never released factory'
        return gateway.ParrotDispatcher()
    module.parrot_entry = entry
    monkeypatch.setitem(sys.modules, module.__name__, module)
    start = threading.Thread.start
    def remember(thread):
        target = getattr(thread, '_target', None)
        owner = getattr(target, '__self__', None)
        if (getattr(target, '__qualname__', '') == 'Client.__init__.<locals>.run' or
                type(owner).__name__ in ('_ConnectionOutbox', '_ManagementQueue', 'RayAdminExecutor')):
            threads.append(thread)
        return start(thread)
    monkeypatch.setattr(threading.Thread, 'start', remember)
    clients = []
    def connect(state):
        client = Client(state)
        clients.append(client)
        return client
    try:
        yield connect, entered, release, calls, threads
    finally:
        release.set()
        cleanup_errors = []
        for client in clients:
            try:
                client.close()
            except BaseException as error:
                cleanup_errors.append(error)
        for thread in threads:
            if thread.ident is not None:
                thread.join(5)
                if thread.is_alive():
                    cleanup_errors.append(AssertionError(f'owned management thread leaked: {thread.name}'))
        assert not cleanup_errors, cleanup_errors


def overload(client, req_id):
    reply = client.reply()
    assert reply['kind'] == 'failed' and reply['req_id'] == req_id, reply
    assert reply['code'] == wire.ERR_OVERLOADED, reply


def test_owner_count_includes_running_and_cross_connection_queued_work(monkeypatch):
    with fixture(monkeypatch) as (connect, entered, release, calls, threads):
        state = gateway.RayAdminState()
        first, second = connect(state), connect(state)
        first.send(1, name='blocked')
        assert entered.wait(3)
        for req_id in range(2, 129): first.send(req_id)
        first.barrier()
        for req_id in range(129, 257): second.send(req_id)
        second.barrier()
        second.send(257)
        overload(second, 257)
        first.barrier(); second.barrier()
        assert calls == ['/user/blocked']
        # The fixture includes two driver/writer pairs. Management cannot create
        # hundreds of OS threads merely to wait for its own registry lock.
        assert len(threads) <= 5
        release.set()
        assert [first.reply()['req_id'] for _ in range(128)] == list(range(1, 129))
        assert [second.reply()['req_id'] for _ in range(128)] == list(range(129, 257))
        assert calls == ['/user/blocked'] + [f'/user/item{i}' for i in range(2, 257)]
        second.send(258)
        assert second.reply()['kind'] == 'deployed'


def test_encoded_byte_capacity_counts_wire_headers_and_releases_for_reuse(monkeypatch):
    with fixture(monkeypatch) as (connect, entered, release, calls, _):
        client = connect(gateway.RayAdminState())
        client.send(1, name='blocked', wire_size=512*1024, header_size=128*1024)
        assert entered.wait(3)
        for req_id in range(2, 33): client.send(req_id, wire_size=512*1024, header_size=128*1024)
        client.barrier()
        client.send(33)
        overload(client, 33)
        assert calls == ['/user/blocked']
        release.set()
        assert [client.reply()['req_id'] for _ in range(32)] == list(range(1, 33))
        client.send(34, wire_size=512*1024)
        assert client.reply()['kind'] == 'deployed'


def test_disconnect_keeps_accepted_management_owned_and_reconnect_cannot_bypass(monkeypatch):
    with fixture(monkeypatch) as (connect, entered, release, calls, threads):
        state = gateway.RayAdminState()
        first = connect(state)
        first.send(1, name='blocked')
        assert entered.wait(3)
        for req_id in range(2, 257): first.send(req_id)
        first.barrier()
        first.close()
        second = connect(state)
        second.send(300)
        overload(second, 300)
        assert calls == ['/user/blocked']
        # Independent gateways do not consume the old owner's capacity.
        other = connect(gateway.RayAdminState())
        other.send(400)
        assert other.reply()['kind'] == 'deployed'
        release.set()
        # A queued completion sentinel waits for all accepted old work, not an
        # arbitrary delay; it observes the registry only after the worker joins.
        for thread in threads:
            if thread.name.startswith('parrot-management-'):
                thread.join(5)
        second.send(301, kind='metrics')
        reply = second.reply()
        assert reply['kind'] == 'metrics' and reply['snapshot']['components'] == 256
        second.send(302)
        assert second.reply()['kind'] == 'deployed'


def test_worker_start_failure_returns_overload_and_restores_full_capacity(monkeypatch):
    with fixture(monkeypatch) as (connect, entered, release, calls, _):
        state = gateway.RayAdminState()
        client = connect(state)
        start = threading.Thread.start
        attempts = []
        def fail_once(thread):
            if thread.name.startswith('parrot-management-') and not attempts:
                attempts.append(True)
                raise RuntimeError('scheduler refused worker')
            return start(thread)
        monkeypatch.setattr(threading.Thread, 'start', fail_once)
        client.send(1, name='must-not-run')
        overload(client, 1)
        assert calls == []
        client.send(2, name='blocked')
        assert entered.wait(3)
        for req_id in range(3, 258): client.send(req_id)
        client.barrier()
        client.send(258)
        overload(client, 258)
        release.set()
        assert [client.reply()['req_id'] for _ in range(256)] == list(range(2, 258))


def test_broken_reply_sink_cannot_strand_accepted_followup(monkeypatch):
    import queue
    out = queue.Queue()
    class Broken:
        def put(self, _frame): raise RuntimeError('reply connection unavailable')
    with fixture(monkeypatch) as (_, entered, release, calls, threads):
        owner = gateway.RayAdminState()
        first = gateway.RayAdminExecutor(Broken(), types.SimpleNamespace(), state=owner)
        second = gateway.RayAdminExecutor(out, types.SimpleNamespace(), state=owner)
        def deploy(req_id, name):
            return admin_v2.encode_admin_cmd_v2(dict(kind='deploy', req_id=req_id, component=dict(
                name=name, version='1', artifact=dict(kind='pymodule', module='management_contract'),
                instances=dict(kind='singleton'), config=None)))
        assert first.on_frame(1, '/reply', deploy(1, 'blocked'))
        assert entered.wait(3)
        assert second.on_frame(2, '/reply', deploy(2, 'next'))
        release.set()
        decoder = wire.FrameDecoder()
        decoder.feed(out.get(timeout=5))
        frame = decoder.next_frame()
        assert admin_v2.decode_admin_reply_v2(frame.payload)['kind'] == 'deployed'
        assert calls == ['/user/blocked', '/user/next']
        for thread in threads: thread.join(5)
        assert owner.management._count == owner.management._bytes == 0
        assert owner.management._thread is None


def test_user_exit_is_failure_with_cleanup_and_queue_keeps_progress(monkeypatch):
    with fixture(monkeypatch) as (connect, _, _, calls, _):
        module = sys.modules['management_contract']
        original = module.parrot_entry
        def entry(ctx):
            if ctx['path'] == '/user/failing': raise SystemExit('factory tried to exit')
            return original(ctx)
        module.parrot_entry = entry
        client = connect(gateway.RayAdminState())
        client.send(1, name='failing')
        failed = client.reply()
        assert failed['kind'] == 'failed' and 'factory tried to exit' in failed['detail']
        client.send(2, kind='metrics')
        assert client.reply()['snapshot']['components'] == 0
        client.send(3, name='next')
        assert client.reply()['kind'] == 'deployed'
        assert calls == ['/user/next']


def test_unprintable_user_exception_reports_failure_and_keeps_queue_healthy(monkeypatch):
    class Unprintable(Exception):
        def __str__(self): raise RuntimeError('cannot format error')
    with fixture(monkeypatch) as (connect, _, _, calls, _):
        module = sys.modules['management_contract']
        original = module.parrot_entry
        def entry(ctx):
            if ctx['path'] == '/user/failing': raise Unprintable()
            return original(ctx)
        module.parrot_entry = entry
        client = connect(gateway.RayAdminState())
        client.send(1, name='failing')
        failed = client.reply()
        assert failed['kind'] == 'failed' and failed['detail'] == 'unprintable Unprintable'
        client.send(2, name='healthy')
        assert client.reply()['kind'] == 'deployed'
        assert calls == ['/user/healthy']


def test_mismatched_management_ids_never_execute_and_reply_on_frame_id(monkeypatch):
    with fixture(monkeypatch) as (connect, _, _, calls, _):
        state = gateway.RayAdminState()
        client = connect(state)
        client.send(1, name='kept')
        assert client.reply()['kind'] == 'deployed'
        commands = [dict(kind='deploy', req_id=99, component=dict(name='forged', version='1',
                    artifact=dict(kind='pymodule', module='management_contract'),
                    instances=dict(kind='singleton'), config=None)),
                    dict(kind='stop', req_id=99, path_prefix='/user/kept'),
                    dict(kind='drain', req_id=99, path_prefix='/user/kept', timeout_ms=1000),
                    dict(kind='status', req_id=99, path_prefix='/user/kept'),
                    dict(kind='metrics', req_id=99), dict(kind='measured_metrics', req_id=99)]
        for cid, command in enumerate(commands, 2):
            client.peer.sendall(wire.build_frame(wire.FT_SYSTEM_EVENT, cid, '/reply', '',
                                                 admin_v2.encode_admin_cmd_v2(command)))
            reply = client.reply()
            assert reply['req_id'] == cid and reply['kind'] == 'failed'
            assert reply['code'] == admin_v2.ERR_DIALECT_MISMATCH
            assert calls == ['/user/kept']
            assert state.path_states == {'/user/kept': 'running'}
        client.peer.sendall(wire.build_frame(wire.FT_SYSTEM_EVENT, 10, '/reply', '',
            admin_v2.encode_admin_cmd_v2(dict(kind='stop', req_id=10, path_prefix='/user/kept'))))
        assert client.reply()['kind'] == 'stopped'
        assert state.path_states == {}


@pytest.mark.parametrize('failure', [RuntimeError, SystemExit])
@pytest.mark.parametrize('stage', ['encode', 'sink', 'diagnostic', 'rejection'])
def test_reply_failure_retires_only_its_connection_and_preserves_owner(monkeypatch, failure, stage):
    with fixture(monkeypatch) as (connect, _, _, calls, threads):
        state = gateway.RayAdminState()
        failed, healthy = connect(state), connect(state)
        completed = threading.Event()
        original_encode = admin_v2.encode_admin_reply_v2
        original_put = gateway._ConnectionOutbox.put
        original_entry = sys.modules['management_contract'].parrot_entry

        def encode(reply):
            if stage in ('encode', 'rejection') and reply['req_id'] == 1:
                completed.set()
                raise failure('injected admin reply encoding failure')
            return original_encode(reply)

        def put(out, frame):
            decoder = wire.FrameDecoder()
            decoder.feed(frame)
            parsed = decoder.next_frame()
            if stage == 'sink' and parsed.ft == wire.FT_SYSTEM_EVENT and parsed.cid == 1:
                completed.set()
                raise failure('injected reply sink failure')
            return original_put(out, frame)

        def entry(ctx):
            if stage == 'diagnostic' and ctx['path'] == '/user/committed':
                raise ValueError('injected factory failure')
            return original_entry(ctx)

        def detail(_):
            completed.set()
            raise failure('injected diagnostic failure')

        monkeypatch.setattr(admin_v2, 'encode_admin_reply_v2', encode)
        monkeypatch.setattr(gateway._ConnectionOutbox, 'put', put)
        monkeypatch.setattr(sys.modules['management_contract'], 'parrot_entry', entry)
        if stage == 'diagnostic':
            monkeypatch.setattr(gateway, '_detail', detail)
        if stage == 'rejection':
            failed.peer.sendall(wire.build_frame(wire.FT_SYSTEM_EVENT, 1, '/reply', '', b'\x03'))
        else:
            failed.send(1, name='committed')
        assert completed.wait(3), 'failure must occur before checking the wire outcome'
        if stage in ('encode', 'sink'):
            assert calls == ['/user/committed']
            assert state.path_states == {'/user/committed': 'running'}
        assert failed.peer.recv(1) == b'', 'failed response must retire the actual socket'
        # An unrelated connection can still query and retire committed state.
        if stage in ('encode', 'sink'):
            for cid, kind in [(2, 'status'), (3, 'stop')]:
                healthy.peer.sendall(wire.build_frame(wire.FT_SYSTEM_EVENT, cid, '/reply', '',
                    admin_v2.encode_admin_cmd_v2(dict(kind=kind, req_id=cid, path_prefix='/user/committed'))))
                reply = healthy.reply()
                if kind == 'status':
                    assert reply['kind'] == 'status' and reply['states'][0]['state'] == 'running'
                else:
                    assert reply['kind'] == 'stopped'
            assert state.path_states == {}
        healthy.send(4, name='followup')
        assert healthy.reply()['kind'] == 'deployed'
        healthy.barrier()
        for thread in threads:
            if thread.name.startswith('parrot-management-'):
                thread.join(3)
                assert not thread.is_alive()
        assert state.management._count == state.management._bytes == 0
