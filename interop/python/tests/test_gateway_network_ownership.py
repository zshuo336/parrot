"""Real sockets survive traceback retention unless the gateway retires them."""
import socket
import sys
import threading
import types

import pytest

from parrot_protocol import ray_gw as gateway, ray_startup, wire


class DriverStopped(Exception):
    pass


@pytest.fixture
def no_cluster(monkeypatch):
    class ActorDied(Exception):
        pass
    worker = types.SimpleNamespace(dead=False, close=types.SimpleNamespace(remote=lambda: {'ok': True, 'pending': []}))
    def ready():
        if worker.dead:
            raise ActorDied()
        return True
    worker.ready = types.SimpleNamespace(remote=ready)
    remote = types.SimpleNamespace(options=lambda **kw: remote, remote=lambda *args: worker)
    monkeypatch.setitem(sys.modules, 'ray', types.SimpleNamespace(
        remote=lambda cls: remote, get=lambda value, **kw: value, is_initialized=lambda: False,
        kill=lambda handle, **kw: setattr(handle, 'dead', True),
        exceptions=types.SimpleNamespace(ActorDiedError=ActorDied)))
    monkeypatch.setattr(ray_startup, 'initialize', lambda *args: None)


def test_bind_failure_closes_listener_while_exception_traceback_is_retained(monkeypatch, no_cluster):
    constructor = socket.socket
    made = []
    def create(*args, **kwargs):
        sock = constructor(*args, **kwargs)
        made.append(sock)
        return sock
    with constructor() as occupied:
        occupied.bind(('127.0.0.1', 0))
        occupied.listen()
        api = types.SimpleNamespace(**{name: getattr(socket, name) for name in dir(socket)})
        api.socket = create
        monkeypatch.setattr(gateway, 'socket', api)
        try:
            with pytest.raises(OSError) as error:
                gateway.serve(occupied.getsockname()[1])
            assert error.traceback is not None
            assert made and all(sock.fileno() == -1 for sock in made)
        finally:
            for sock in made:
                sock.close()


def test_driver_failure_closes_live_connections_and_joins_network_threads(monkeypatch, no_cluster):
    constructor = socket.socket
    listener = constructor()
    peer = constructor()
    peer.settimeout(3)
    accepted, threads = [], []
    read_hello = threading.Event()
    connected = threading.Event()
    errors = []
    class Listener:
        def __getattr__(self, name):
            return getattr(listener, name)
        def accept(self):
            if accepted:
                assert read_hello.wait(3)
                raise DriverStopped('accept interrupted')
            sock, address = listener.accept()
            accepted.append(sock)
            return sock, address
        def listen(self, backlog):
            listener.listen(backlog)
            peer.connect(listener.getsockname())
            peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '', wire.handshake_body('owner-test')))
            connected.set()
    api = types.SimpleNamespace(**{name: getattr(socket, name) for name in dir(socket)})
    api.socket = lambda *args, **kwargs: Listener()
    monkeypatch.setattr(gateway, 'socket', api)
    start = threading.Thread.start
    def track(thread):
        threads.append(thread)
        return start(thread)
    monkeypatch.setattr(threading.Thread, 'start', track)
    def read():
        try:
            assert connected.wait(3), 'fixture client never connected'
            decoder = wire.FrameDecoder()
            while True:
                data = peer.recv(65536)
                assert data
                decoder.feed(data)
                frame = decoder.next_frame()
                if frame is not None:
                    assert frame.ft == wire.FT_HANDSHAKE_ACK
                    read_hello.set()
                    return
        except BaseException as error:
            errors.append(error)
            read_hello.set()
    reader = threading.Thread(target=read)
    reader.start()
    try:
        with pytest.raises(DriverStopped) as error:
            gateway.serve(0)
        reader.join(3)
        assert not errors, errors
        assert error.traceback is not None
        assert listener.fileno() == -1, 'listener survives serve failure'
        assert all(sock.fileno() == -1 for sock in accepted), 'accepted socket survives serve failure'
        assert all(not thread.is_alive() for thread in threads), 'owned network thread survives serve failure'
        assert peer.recv(1) == b''
    finally:
        peer.close()
        listener.close()
        for sock in accepted:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            sock.close()
        for thread in threads:
            thread.join(3)


@pytest.mark.parametrize('failure', ['socket-option', 'thread-start'])
def test_accepted_socket_is_closed_if_handoff_fails(monkeypatch, no_cluster, failure):
    listener = socket.socket()
    local, peer = socket.socketpair()
    class Accepted:
        def __getattr__(self, name):
            return getattr(local, name)
        def setsockopt(self, *args):
            if failure == 'socket-option':
                raise DriverStopped('socket configuration failed')
    class Listener:
        def __getattr__(self, name):
            return getattr(listener, name)
        def accept(self):
            return Accepted(), ('127.0.0.1', 1)
    api = types.SimpleNamespace(**{name: getattr(socket, name) for name in dir(socket)})
    api.socket = lambda *args, **kwargs: Listener()
    monkeypatch.setattr(gateway, 'socket', api)
    def fail_start(thread):
        raise DriverStopped('thread creation failed')
    monkeypatch.setattr(threading.Thread, 'start', fail_start)
    try:
        with pytest.raises(DriverStopped) as error:
            gateway.serve(0)
        assert error.traceback is not None
        assert local.fileno() == -1
        assert listener.fileno() == -1
    finally:
        local.close()
        peer.close()
        listener.close()


def test_registration_closes_established_socket_if_direct_owner_setup_fails(monkeypatch, no_cluster):
    local, peer = socket.socketpair()
    api = types.SimpleNamespace(**{name: getattr(socket, name) for name in dir(socket)})
    api.create_connection = lambda *args, **kwargs: local
    monkeypatch.setattr(gateway, 'socket', api)
    def fail(*args, **kwargs):
        raise DriverStopped('direct owner setup failed')
    monkeypatch.setattr(gateway, '_DirectLinks', fail)
    try:
        with pytest.raises(DriverStopped) as error:
            gateway.serve(0, parrot_addr='127.0.0.1:1234')
        assert error.traceback is not None
        assert local.fileno() == -1
    finally:
        local.close()
        peer.close()


def test_owner_shutdown_retires_every_connection_and_is_idempotent():
    owner = gateway._ConnectionDrivers()
    pairs = [socket.socketpair() for _ in range(4)]
    state = gateway.RayAdminState()
    try:
        for index, (local, peer) in enumerate(pairs):
            owner.start(local, gateway.ParrotDispatcher(), None, None, state)
            peer.settimeout(3)
            peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, index, '', '', wire.handshake_body('multi')))
            decoder = wire.FrameDecoder()
            while True:
                data = peer.recv(65536)
                assert data
                decoder.feed(data)
                frame = decoder.next_frame()
                if frame is not None:
                    assert frame.ft == wire.FT_HANDSHAKE_ACK
                    break
        owner.close()
        owner.close()
        for local, peer in pairs:
            assert local.fileno() == -1
            assert peer.recv(1) == b''
    finally:
        owner.close()
        for local, peer in pairs:
            local.close()
            peer.close()


@pytest.mark.parametrize('fails_at', ['executor', 'admin', 'decoder'])
def test_connection_setup_failure_joins_writer(monkeypatch, fails_at):
    import concurrent.futures
    writers = []
    original = gateway._ConnectionOutbox
    def outbox(sock):
        out = original(sock)
        writers.append(out)
        return out
    monkeypatch.setattr(gateway, '_ConnectionOutbox', outbox)
    def fail(*args, **kwargs):
        raise DriverStopped('setup failed')
    target, name = {'executor': (concurrent.futures, 'ThreadPoolExecutor'),
                    'admin': (gateway, 'RayAdminExecutor'),
                    'decoder': (gateway, 'FrameDecoder')}[fails_at]
    monkeypatch.setattr(target, name, fail)
    local, peer = socket.socketpair()
    try:
        with pytest.raises(DriverStopped) as error:
            gateway._serve_connection_guarded(local, gateway.ParrotDispatcher(), None, None)
        assert error.traceback is not None
        assert all(not out._thread.is_alive() for out in writers), 'setup failure leaked writer'
        assert local.fileno() == -1
        peer.settimeout(3)
        assert peer.recv(1) == b''
    finally:
        for out in writers:
            out.close()
        local.close()
        peer.close()
