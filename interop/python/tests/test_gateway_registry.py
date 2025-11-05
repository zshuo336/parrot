"""Gateway registry ownership through real Wire connections with a local Ray adapter.

The adapter only replaces actor scheduling; encoding, connection drivers,
component factories, mounting, admin dispatch and replies are production code.
"""
from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack, contextmanager
import queue
import socket
import sys
import threading
import types

import pytest

from parrot_protocol import admin_v2, wire
from parrot_protocol.ray_gw import (
    ParrotDispatcher, RayAdminExecutor, RayAdminState, _serve_connection_guarded,
)


def component(name="shared"):
    return {"name": name, "version": "1", "artifact": {
        "kind": "pymodule", "module": "registry_component"},
        "instances": {"kind": "singleton"}}


@pytest.fixture
def factory(monkeypatch):
    created = []
    module = types.ModuleType("registry_component")

    def entry(ctx):
        dispatcher = ParrotDispatcher()
        dispatcher.handler("bin:t:Echo")(lambda key, payload: (key, payload))
        created.append(dispatcher)
        return dispatcher

    module.parrot_entry = entry
    monkeypatch.setitem(sys.modules, module.__name__, module)
    return module, created


class ActorDied(Exception):
    pass


class LocalRay:
    exceptions = types.SimpleNamespace(ActorDiedError=ActorDied)
    def get(self, value):
        return value() if callable(value) else value

    def kill(self, handle, no_restart=False):
        # Component dispatchers are unmounted, never Ray actor handles.
        raise TypeError("not an actor handle")


@contextmanager
def connection(owner, dispatcher):
    peer, server = socket.socketpair()
    peer.settimeout(5)
    errors = []

    def run():
        try:
            _serve_connection_guarded(server, dispatcher, None, LocalRay(), owner)
        except BaseException as error:
            errors.append(error)

    thread = threading.Thread(target=run)
    thread.start()
    decoder = wire.FrameDecoder()

    def receive():
        while True:
            frame = decoder.next_frame()
            if frame is not None:
                return frame
            chunk = peer.recv(65536)
            assert chunk, "connection closed before reply"
            decoder.feed(chunk)

    def admin(kind, req_id, **fields):
        peer.sendall(wire.build_frame(wire.FT_SYSTEM_EVENT, req_id, "admin", "",
            admin_v2.encode_admin_cmd_v2({"kind": kind, "req_id": req_id, **fields})))
        result = admin_v2.decode_admin_reply_v2(receive().payload)
        assert result["req_id"] == req_id
        return result

    try:
        peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 0, "", "__handshake__",
                                     wire.handshake_body("registry-test")))
        assert receive().ft == wire.FT_HANDSHAKE_ACK
        yield admin
    finally:
        peer.close()
        thread.join(6)
        if thread.is_alive():
            server.shutdown(socket.SHUT_RDWR)
            thread.join(6)
        assert not thread.is_alive(), "connection thread leaked"
        assert not errors, errors


def test_deploy_disconnect_reconnect_observe_and_stop(factory):
    owner, dispatcher = RayAdminState(), ParrotDispatcher()
    with connection(owner, dispatcher) as first:
        assert first("deploy", 1, component=component())["kind"] == "deployed"
    with connection(owner, dispatcher) as second:
        status = second("status", 2, path_prefix="/user/shared")
        assert status["states"] == [{"path": "/user/shared", "state": "running", "version": "1"}]
        metrics = second("metrics", 3)["snapshot"]
        assert metrics["components"] == 1
        assert metrics["component_states"] == status["states"]
        assert dispatcher.dispatch_path("/user/shared", "bin:t:Echo", b"alive") == ("bin:t:Echo", b"alive")
        assert second("stop", 4, path_prefix="/user/shared")["kind"] == "stopped"
        assert second("metrics", 5)["snapshot"]["components"] == 0
    with connection(owner, dispatcher) as third:
        assert third("status", 6, path_prefix="/user/shared")["code"] == admin_v2.ERR_COMPONENT_NOT_FOUND
    with pytest.raises(KeyError):
        dispatcher.dispatch_path("/user/shared", "bin:t:Echo", b"gone")


def test_distinct_gateway_owners_do_not_share_components(factory):
    with connection(RayAdminState(), ParrotDispatcher()) as first, connection(RayAdminState(), ParrotDispatcher()) as second:
        assert first("deploy", 1, component=component())["kind"] == "deployed"
        assert second("metrics", 2)["snapshot"]["components"] == 0
        assert second("stop", 3, path_prefix="/user/shared")["code"] == admin_v2.ERR_COMPONENT_NOT_FOUND
        assert first("stop", 4, path_prefix="/user/shared")["kind"] == "stopped"


def test_concurrent_connections_cannot_overwrite_live_deployment(factory):
    _, created = factory
    owner, dispatcher = RayAdminState(), ParrotDispatcher()
    with ExitStack() as stack, ThreadPoolExecutor(max_workers=2) as pool:
        first = stack.enter_context(connection(owner, dispatcher))
        second = stack.enter_context(connection(owner, dispatcher))
        barrier = threading.Barrier(2)

        def deploy(client, req_id):
            barrier.wait(timeout=5)
            return client("deploy", req_id, component=component())

        a, b = pool.submit(deploy, first, 1), pool.submit(deploy, second, 2)
        replies = [a.result(timeout=6), b.result(timeout=6)]
        assert sorted(r["kind"] for r in replies) == ["deployed", "failed"]
        assert len(created) == 1, "the losing request must not launch an orphan"
        assert first("stop", 3, path_prefix="/user/shared")["kind"] == "stopped"
        assert second("deploy", 4, component=component())["kind"] == "deployed"
        assert len(created) == 2
        assert first("stop", 5, path_prefix="/user/shared")["kind"] == "stopped"


def test_stop_on_another_connection_waits_for_deployment_receipt(factory):
    module, _ = factory
    original = module.parrot_entry
    entered, release, stop_started = threading.Event(), threading.Event(), threading.Event()

    def blocked(ctx):
        entered.set()
        assert release.wait(5)
        return original(ctx)

    module.parrot_entry = blocked
    state, dispatcher = RayAdminState(), ParrotDispatcher()
    first = RayAdminExecutor(queue.Queue(), LocalRay(), dispatcher, state=state)
    second = RayAdminExecutor(queue.Queue(), LocalRay(), dispatcher, state=state)
    with ThreadPoolExecutor(max_workers=2) as pool:
        deploy = pool.submit(first.deploy, component())
        assert entered.wait(5)

        def stop():
            stop_started.set()
            return second.stop("/user/shared")

        stopped = pool.submit(stop)
        try:
            assert stop_started.wait(5)
            assert not stopped.done()
        finally:
            release.set()
        assert deploy.result(timeout=5)["kind"] == "deployed"
        assert stopped.result(timeout=5)["kind"] == "stopped"
    assert second.collect_metrics()["components"] == 0
    with pytest.raises(KeyError):
        dispatcher.dispatch_path("/user/shared", "bin:t:Echo", b"gone")


class RemoteMethod:
    def __init__(self, function):
        self.function = function
        self.calls = 0

    def remote(self, *args):
        self.calls += 1
        return lambda: self.function(*args)


def test_stop_waits_for_worker_unmount_receipt(factory):
    entered, release = threading.Event(), threading.Event()

    def unmount(name, paths, timeout_ms):
        entered.set()
        assert release.wait(5)
        return {path: None for path in paths}

    worker = types.SimpleNamespace(
        mount_module=RemoteMethod(lambda *args: {'ok': True, 'paths': args[4]}),
        retire_paths=RemoteMethod(unmount))
    executor = RayAdminExecutor(queue.Queue(), LocalRay(), worker_ref=worker)
    assert executor.deploy(component())["kind"] == "deployed"
    with ThreadPoolExecutor(max_workers=1) as pool:
        stopped = pool.submit(executor.stop, "/user/shared")
        try:
            assert entered.wait(2), "stop must await the remote unmount operation"
            assert not stopped.done()
        finally:
            release.set()
        assert stopped.result(timeout=5)["kind"] == "stopped"
    assert executor.collect_metrics()["components"] == 0


def test_failed_worker_unmount_retains_registry_for_retry(factory):
    def fail(_name, _paths, _timeout):
        raise RuntimeError("unmount rejected")

    worker = types.SimpleNamespace(
        mount_module=RemoteMethod(lambda *args: {'ok': True, 'paths': args[4]}),
        retire_paths=RemoteMethod(fail))
    executor = RayAdminExecutor(queue.Queue(), LocalRay(), worker_ref=worker)
    assert executor.deploy(component())["kind"] == "deployed"
    with pytest.raises(RuntimeError, match="unmount rejected"):
        executor.stop("/user/shared")
    assert executor.status("/user/shared")["states"][0]["path"] == "/user/shared"
    worker.retire_paths = RemoteMethod(lambda name, paths, timeout: {p:None for p in paths})
    assert executor.stop("/user/shared")["kind"] == "stopped"


def test_drain_hook_runs_once_with_remaining_deadline(factory):
    module, _ = factory
    drained, killed, timeouts = [], [], []
    ready_ref = object()
    handle = types.SimpleNamespace(__ray_ready__=types.SimpleNamespace(remote=lambda: ready_ref), parrot_drain=RemoteMethod(lambda: drained.append(True)))
    module.parrot_entry = lambda ctx: handle

    class Adapter(LocalRay):
        def get(self, value, *, timeout=None):
            if value is ready_ref:
                if killed:
                    raise ActorDied()
                return True
            timeouts.append(timeout)
            return super().get(value)

        def kill(self, value, no_restart=False):
            assert value is handle and no_restart
            killed.append(value)

    executor = RayAdminExecutor(queue.Queue(), Adapter())
    assert executor.deploy(component())["kind"] == "deployed"
    assert executor.drain("/user/shared", 2000) == (1, 0)
    assert handle.parrot_drain.calls == 1
    assert drained == [True] and killed == [handle]
    assert len(timeouts) == 1 and 0 < timeouts[0] <= 2


def test_failed_drain_retains_component_for_explicit_stop(factory):
    module, _ = factory
    killed = []
    ready_ref = object()
    handle = types.SimpleNamespace(__ray_ready__=types.SimpleNamespace(remote=lambda: ready_ref), parrot_drain=RemoteMethod(lambda: None))
    module.parrot_entry = lambda ctx: handle

    class Adapter(LocalRay):
        def get(self, value, *, timeout=None):
            if value is ready_ref:
                if killed:
                    raise ActorDied()
                return True
            raise TimeoutError("drain has not completed")

        def kill(self, value, no_restart=False):
            killed.append(value)

    executor = RayAdminExecutor(queue.Queue(), Adapter())
    assert executor.deploy(component())["kind"] == "deployed"
    assert executor.drain("/user/shared", 10) == (0, 1)
    assert killed == []
    assert executor.collect_metrics()["components"] == 1
    assert executor.stop("/user/shared")["kind"] == "stopped"
    assert killed == [handle]
