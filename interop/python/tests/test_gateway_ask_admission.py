"""Gateway-wide ASK retention, exercised through actual socket drivers.

The local Ray adapter holds requests at ray.get and records the executor threads
so each test can join its own running work after connection cancellation.
"""
import socket
import struct
import threading
import types
from contextlib import contextmanager

import pytest
from parrot_protocol import ray_gw as gateway, wire


class Connection:
    def __init__(self, owner):
        self.local, self.peer = socket.socketpair()
        self.peer.settimeout(2)
        self.release = threading.Event()
        self.entered = threading.Event()
        self.calls = []
        self.workers = set()
        self.errors = []
        self.decoder = wire.FrameDecoder()

        def ask(path, key, payload):
            self.workers.add(threading.current_thread())
            self.calls.append(payload)
            if key == 'bin:block':
                self.entered.set()
                assert self.release.wait(10), 'test failed to release a running ASK'
            if key == 'bin:fail':
                raise ValueError('business failure')
            return dict(ok=True, key=key, payload=b'ok')

        worker = types.SimpleNamespace(ask=types.SimpleNamespace(remote=lambda *args: lambda: ask(*args)))
        ray = types.SimpleNamespace(get=lambda ref: ref())

        def run():
            try:
                gateway._serve_connection_guarded(self.local, gateway.ParrotDispatcher(), worker, ray, owner)
            except BaseException as error:
                self.errors.append(error)

        self.thread = threading.Thread(target=run)
        self.thread.start()
        self.peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 0, '', '', wire.handshake_body('ask-budget')))
        assert self.read().ft == wire.FT_HANDSHAKE_ACK

    def read(self):
        while True:
            frame = self.decoder.next_frame()
            if frame is not None:
                return frame
            data = self.peer.recv(65536)
            assert data, 'gateway closed before a terminal reply'
            self.decoder.feed(data)

    def ask(self, cid, payload=b'x', key='bin:block', wire_size=None):
        if wire_size is not None:
            # 32 fixed wire bytes + 10 path + 9 key + 6 reply prefix.
            payload = b'x' * (wire_size - 57)
        frame = wire.build_frame(wire.FT_ASK, cid, '/user/test', key, struct.pack('<I', 2) + b'/r' + payload)
        if wire_size is not None:
            assert len(frame) == wire_size
        self.peer.sendall(frame)

    def barrier(self):
        self.peer.sendall(wire.build_frame(wire.FT_HEARTBEAT, 9999, '', '', b''))
        reply = self.read()
        assert reply.ft == wire.FT_HEARTBEAT_ACK and reply.cid == 9999

    def close_connection(self):
        self.peer.close()
        self.thread.join(3)
        if self.thread.is_alive():
            self.local.shutdown(socket.SHUT_RDWR)
            self.thread.join(3)
        assert not self.thread.is_alive()
        assert not self.errors, self.errors

    def cleanup(self):
        self.close_connection()  # Cancel unstarted work before releasing running work.
        self.release.set()
        for worker in tuple(self.workers):
            worker.join(3)
            assert not worker.is_alive(), 'owned ASK executor thread failed to retire'


@contextmanager
def clients(monkeypatch, count=1, owner=None):
    monkeypatch.setattr(gateway, 'RAY_GW_ASK_WORKERS', 1)
    owner = owner or gateway.RayAdminState()
    live = []
    try:
        for _ in range(count):
            live.append(Connection(owner))
        yield live
    finally:
        for conn in live:
            conn.cleanup()


def overloaded(conn, cid):
    reply = conn.read()
    assert (reply.ft, reply.cid, reply.path) == (wire.FT_REPLY_ERR, cid, '/r')
    assert wire.decode_err_payload(reply.payload)[0] == wire.ERR_OVERLOADED


def test_ask_count_budget_is_shared_across_connections_and_heartbeats_progress(monkeypatch):
    with clients(monkeypatch, 2) as (first, second):
        for conn in (first, second):
            conn.ask(1)
            assert conn.entered.wait(2)
            for cid in range(2, 129):
                conn.ask(cid)
            conn.barrier()
        second.ask(129)
        overloaded(second, 129)
        assert len(first.calls) == len(second.calls) == 1
        first.barrier(); second.barrier()


def test_ask_encoded_byte_budget_includes_queued_and_running_requests(monkeypatch):
    with clients(monkeypatch) as (conn,):
        conn.ask(1, wire_size=512 * 1024)
        assert conn.entered.wait(2)
        for cid in range(2, 33):
            conn.ask(cid, wire_size=512 * 1024)
        conn.barrier()
        conn.ask(33)
        overloaded(conn, 33)
        assert len(conn.calls) == 1
        conn.barrier()


def test_disconnect_cancels_unstarted_work_but_running_request_keeps_its_charge(monkeypatch):
    with clients(monkeypatch, 2) as (first, second):
        first.ask(1, b'first')
        assert first.entered.wait(2)
        for cid in range(2, 257):
            first.ask(cid, b'must-not-run')
        first.barrier()
        first.close_connection()
        second.ask(1)
        assert second.entered.wait(2)
        for cid in range(2, 256):
            second.ask(cid)
        second.barrier()
        second.ask(256)
        overloaded(second, 256)
        assert first.calls == [b'first']
        assert len(second.calls) == 1
        first.release.set(); second.release.set()
        replies = [second.read() for _ in range(255)]
        assert {reply.cid for reply in replies} == set(range(1, 256))
        assert all(reply.ft == wire.FT_REPLY for reply in replies)
        second.ask(300, key='bin:fail')
        reply = second.read()
        assert reply.ft == wire.FT_REPLY_ERR and reply.cid == 300
        second.ask(301, key='bin:ok')
        assert second.read().ft == wire.FT_REPLY


def test_distinct_gateway_owners_do_not_consume_each_others_capacity(monkeypatch):
    with clients(monkeypatch, owner=gateway.RayAdminState()) as (busy,):
        busy.ask(1)
        assert busy.entered.wait(2)
        for cid in range(2, 257):
            busy.ask(cid)
        busy.barrier()
        with clients(monkeypatch, owner=gateway.RayAdminState()) as (other,):
            other.ask(1, key='bin:ok')
            assert other.read().ft == wire.FT_REPLY
            assert len(other.calls) == 1
        assert len(busy.calls) == 1


def test_scheduler_rejection_replies_and_returns_reserved_capacity(monkeypatch):
    import concurrent.futures
    original = concurrent.futures.ThreadPoolExecutor

    class RejectOnce(original):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, **kwargs)
            self.rejected = False

        def submit(self, *args, **kwargs):
            if not self.rejected:
                self.rejected = True
                raise RuntimeError('injected scheduling failure')
            return super().submit(*args, **kwargs)

    monkeypatch.setattr(concurrent.futures, 'ThreadPoolExecutor', RejectOnce)
    with clients(monkeypatch) as (conn,):
        conn.ask(999)
        overloaded(conn, 999)
        assert conn.calls == []
        conn.ask(1)
        assert conn.entered.wait(2)
        for cid in range(2, 257):
            conn.ask(cid)
        conn.barrier()
        conn.ask(257)
        overloaded(conn, 257)
        assert len(conn.calls) == 1
