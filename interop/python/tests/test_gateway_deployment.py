"""Failure receipts, retained ownership and capacity contracts for Ray deployment.

Factories and executor/runtime are production code. Ray RPC completion and kill
are controlled here; actual actors are covered by run_ray_lifecycle.py.
"""
import queue
import sys
import time
import types
import threading
from concurrent.futures import ThreadPoolExecutor

import pytest

from parrot_protocol.ray_gw import ParrotDispatcher, RayAdminExecutor


class Remote:
    def __init__(self, fn):
        self.fn, self.calls = fn, []

    def remote(self, *args):
        self.calls.append(args)
        return lambda: self.fn(*args)


class ActorDied(Exception):
    pass


class Ray:
    exceptions = types.SimpleNamespace(ActorDiedError=ActorDied)
    def __init__(self):
        self.fail_kill, self.killed = set(), []

    def get(self, value, timeout=None):
        return value()

    def kill(self, handle, no_restart=False):
        assert no_restart is True
        if handle in self.fail_kill:
            raise RuntimeError('kill unavailable')
        self.killed.append(handle)
        handle.dead = True


class Native:
    def __init__(self):
        self.dead = False
        self.__ray_ready__ = Remote(self.ready)
        self.ask = Remote(lambda key, data: (key, data))
        self.parrot_drain = Remote(lambda: True)

    def ready(self):
        if self.dead:
            raise ActorDied()
        return True


def comp(name='pool', count=2):
    return dict(name=name, version='7', artifact=dict(kind='pymodule', module='deployment_contract'),
                instances=dict(kind='pool', count=count))


@pytest.fixture
def setup(monkeypatch):
    module = types.ModuleType('deployment_contract')
    created = []
    def entry(ctx):
        handle = Native()
        created.append(handle)
        return handle
    module.parrot_entry = entry
    monkeypatch.setitem(sys.modules, module.__name__, module)
    ray = Ray()
    executor = RayAdminExecutor(queue.Queue(), ray, ParrotDispatcher())
    return executor, ray, module, created


def test_factory_failure_retains_failed_kill_and_retry_owns_handle(setup):
    ex, ray, module, created = setup
    original = module.parrot_entry
    def fail_second(ctx):
        if created:
            raise RuntimeError('second factory failed')
        handle = original(ctx)
        ray.fail_kill.add(handle)
        return handle
    module.parrot_entry = fail_second
    assert ex.deploy(comp())['kind'] == 'failed'
    assert ex.status('/user/pool')['states'] == [dict(path='/user/pool-0', version='7', state='stop_failed')]
    assert ex._components['pool']['handles'] == created
    assert ex._runtime().ask('/user/pool-0', 'echo', b'')['code'] == 3
    ray.fail_kill.clear()
    assert ex.stop('/user/pool')['kind'] == 'stopped'
    assert ray.killed == created and ex.collect_metrics()['components'] == 0


@pytest.mark.parametrize('operation', ['stop', 'drain'])
def test_partial_retirement_retains_only_failure_and_keeps_sibling(setup, operation):
    ex, ray, _, created = setup
    assert ex.deploy(comp(count=3))['kind'] == 'deployed'
    ray.fail_kill.add(created[1])
    if operation == 'stop':
        with pytest.raises(RuntimeError, match='incomplete'):
            ex.stop('/user/pool')
    else:
        assert ex.drain('/user/pool', 5000) == (2, 1)
    assert ex._components['pool']['handles'] == [created[1]]
    assert ex.status('/user/pool')['states'] == [dict(path='/user/pool-1', version='7', state=operation+'_failed')]
    assert ray.killed == [created[0], created[2]]
    assert ex._runtime().ask('/user/pool-1', 'echo', b'')['code'] == 3
    assert ex.deploy(comp('sibling', 1))['kind'] == 'deployed'
    ray.fail_kill.clear()
    assert ex.stop('/user/pool')['kind'] == 'stopped'
    assert ex.status('/user/sibling')['states'][0]['state'] == 'running'
    assert ex.stop('/user/sibling')['kind'] == 'stopped'


def test_uncertain_mount_rpc_keeps_all_paths_until_explicit_retirement(setup):
    ex, ray, _, _ = setup
    runtime = ex._runtime()
    def lost_receipt(*args):
        assert runtime.mount_module(*args)['ok']
        raise ConnectionError('reply lost after mount')
    worker = types.SimpleNamespace(mount_module=Remote(lost_receipt), retire_paths=Remote(runtime.retire_paths))
    remote = RayAdminExecutor(queue.Queue(), ray, worker_ref=worker)
    assert remote.deploy(comp())['kind'] == 'failed'
    assert {s['state'] for s in remote.status('/user/pool')['states']} == {'deploy_failed'}
    assert remote.stop('/user/pool')['kind'] == 'stopped'
    assert len(ray.killed) == 2 and runtime.handles(['/user/pool-0', '/user/pool-1']) == []


@pytest.mark.parametrize('receipt', [None, {'ok': 1, 'paths': ['/user/pool-0', '/user/pool-1']},
    {'ok': True, 'paths': ['/user/pool-0']}, {'ok': False, 'paths': ['/user/other']},
    {'ok': False, 'paths': ['/user/pool-0', '/user/pool-0']}, {'ok': False, 'paths': ''}])
def test_bad_mount_receipt_cannot_erase_requested_ownership(receipt):
    worker = types.SimpleNamespace(mount_module=Remote(lambda *args: receipt),
        retire_paths=Remote(lambda name, paths, timeout: dict.fromkeys(paths)))
    ex = RayAdminExecutor(queue.Queue(), Ray(), worker_ref=worker)
    assert ex.deploy(comp())['kind'] == 'failed'
    assert ex.status('/user/pool')['states'] == [dict(path=f'/user/pool-{i}', state='deploy_failed', version='7') for i in range(2)]
    assert ex.stop('/user/pool')['kind'] == 'stopped'


@pytest.mark.parametrize('receipt', [None, {}, {'/user/pool-0': None},
    {'/user/pool-0': None, '/user/pool-1': 'running'}])
def test_bad_retirement_receipt_retains_all_selected_paths(receipt):
    worker = types.SimpleNamespace(mount_module=Remote(lambda *args: dict(ok=True, paths=args[4])),
        retire_paths=Remote(lambda *args: receipt))
    ex = RayAdminExecutor(queue.Queue(), Ray(), worker_ref=worker)
    assert ex.deploy(comp())['kind'] == 'deployed'
    with pytest.raises(Exception):
        ex.stop('/user/pool')
    assert len(ex.status('/user/pool')['states']) == 2
    worker.retire_paths = Remote(lambda name, paths, timeout: dict.fromkeys(paths))
    assert ex.stop('/user/pool')['kind'] == 'stopped'


def test_expired_deadline_does_not_submit_worker_operation():
    worker = types.SimpleNamespace(retire_paths=Remote(lambda *args: None))
    ex = RayAdminExecutor(queue.Queue(), Ray(), worker_ref=worker)
    with pytest.raises(TimeoutError):
        ex._call('retire_paths', 'pool', ['/user/pool-0'], 0, deadline=time.monotonic()-1)
    assert worker.retire_paths.calls == []


def test_longest_pool_member_is_routable(setup):
    ex, _, module, _ = setup
    module.parrot_entry = lambda ctx: ParrotDispatcher()
    name = 'n' * 256
    assert ex.deploy(comp(name, 256))['kind'] == 'deployed'
    handle = ex._components[name]['handles'][-1]
    handle.handler('echo')(lambda key, data: (key, data))
    assert ex._runtime().ask('/user/'+name+'-255', 'echo', b'ok')['payload'] == b'ok'
    assert ex.stop('/user/'+name)['kind'] == 'stopped'


def test_gateway_capacity_precedes_factory_and_reuses_retired_slots(setup):
    ex, _, module, created = setup
    def entry(ctx):
        handle = ParrotDispatcher()
        created.append(handle)
        return handle
    module.parrot_entry = entry
    for i in range(16):
        assert ex.deploy(comp('group'+str(i), 256))['kind'] == 'deployed'
    assert len(created) == 4096
    assert ex.deploy(comp('overflow', 1))['kind'] == 'failed'
    assert len(created) == 4096
    assert ex.stop('/user/group0-0')['kind'] == 'stopped'
    assert ex.deploy(comp('overflow', 1))['kind'] == 'deployed'
    assert len(created) == 4097


@pytest.mark.parametrize('bad_handle', [None, object(), 42])
def test_invalid_factory_handle_never_becomes_running(setup, bad_handle):
    ex, _, module, _ = setup
    module.parrot_entry = lambda ctx: bad_handle
    assert ex.deploy(comp(count=1))['kind'] == 'failed'
    assert ex.collect_metrics()['components'] == 0


def test_native_readiness_failure_retires_handle_before_reply(setup):
    ex, ray, module, created = setup
    original = module.parrot_entry
    def not_ready(ctx):
        handle = original(ctx)
        def timeout():
            if handle.dead:
                raise ActorDied()
            raise TimeoutError('constructor never became ready')
        handle.__ray_ready__ = Remote(timeout)
        return handle
    module.parrot_entry = not_ready
    assert ex.deploy(comp())['kind'] == 'failed'
    assert ray.killed == created and len(created) == 1
    assert ex.collect_metrics()['components'] == 0


def test_inline_native_stop_waits_for_death_before_success_receipt(setup):
    ex, ray, _, created = setup
    assert ex.deploy(comp(count=1))['kind'] == 'deployed'
    handle = created[0]
    requested = threading.Event()
    probing = threading.Event()
    dead = threading.Event()
    def request_kill(value, no_restart=False):
        assert value is handle and no_restart
        requested.set()
    def readiness():
        if requested.is_set():
            probing.set()
            if not dead.wait(5):
                raise TimeoutError('test did not confirm actor death')
            raise ActorDied()
        return True
    ray.kill = request_kill
    handle.__ray_ready__ = Remote(readiness)
    with ThreadPoolExecutor(max_workers=1) as executor:
        result = executor.submit(ex.stop, '/user/pool-0')
        try:
            assert probing.wait(2), 'Stop returned without asking for a death receipt'
            assert not result.done()
        finally:
            dead.set()
        assert result.result(timeout=5)['kind'] == 'stopped'
    assert ex.collect_metrics()['components'] == 0


def test_inline_native_uncertain_death_retains_ownership_until_confirmed_retry(setup):
    ex, ray, _, created = setup
    assert ex.deploy(comp(count=1))['kind'] == 'deployed'
    handle = created[0]
    state = dict(requested=False, confirmed=False)
    def request_kill(value, no_restart=False):
        assert value is handle and no_restart
        state['requested'] = True
    def readiness():
        if state['confirmed']:
            raise ActorDied()
        if state['requested']:
            raise TimeoutError('death receipt unavailable')
        return True
    ray.kill = request_kill
    handle.__ray_ready__ = Remote(readiness)
    with pytest.raises(RuntimeError, match='incomplete'):
        ex.stop('/user/pool-0')
    assert ex.collect_metrics()['components'] == 1
    assert ex._runtime().handles(['/user/pool-0']) == [handle]
    state['confirmed'] = True
    assert ex.stop('/user/pool-0')['kind'] == 'stopped'
    assert ex.collect_metrics()['components'] == 0


@pytest.mark.parametrize('kind', ['ask', 'tell'])
def test_lost_mount_receipt_blocks_wire_business_until_cleanup(setup, kind):
    import socket
    import threading
    from parrot_protocol import wire
    from parrot_protocol.ray_gw import RayAdminState, _serve_connection_guarded

    ex, ray, module, _ = setup
    effects = []
    def entry(ctx):
        dispatcher = ParrotDispatcher()
        def mutate(key, payload):
            effects.append(ctx['path'])
            return key, payload
        dispatcher.handler('mutate')(mutate)
        return dispatcher
    module.parrot_entry = entry
    runtime = ex._runtime()
    def lost(*args):
        assert runtime.mount_module(*args)['ok']
        raise ConnectionError('mount reply lost')
    worker = types.SimpleNamespace(mount_module=Remote(lost), retire_paths=Remote(runtime.retire_paths),
        ask=Remote(runtime.ask), deliver=Remote(runtime.deliver))
    state = RayAdminState()
    admin = RayAdminExecutor(queue.Queue(), ray, worker_ref=worker, state=state)
    assert admin.deploy(comp())['kind'] == 'failed'
    peer, local = socket.socketpair()
    peer.settimeout(3)
    failures = []
    def serve():
        try:
            _serve_connection_guarded(local, ParrotDispatcher(), worker, ray, state)
        except BaseException as error:
            failures.append(error)
    thread = threading.Thread(target=serve)
    thread.start()
    decoder = wire.FrameDecoder()
    def exchange(ft, cid, path='', key='', payload=b''):
        peer.sendall(wire.build_frame(ft, cid, path, key, payload))
        while True:
            frame = decoder.next_frame()
            if frame is not None:
                assert frame.cid == cid
                return frame
            data = peer.recv(65536)
            assert data
            decoder.feed(data)
    try:
        assert exchange(wire.FT_HANDSHAKE, 1, payload=wire.handshake_body('uncertain')).ft == wire.FT_HANDSHAKE_ACK
        if kind == 'ask':
            reply = exchange(wire.FT_ASK, 2, '/user/pool-0', 'mutate', wire.with_reply_to_prefix('/reply', b'effect'))
            assert reply.ft == wire.FT_REPLY_ERR and wire.decode_err_payload(reply.payload)[0] == wire.ERR_STOPPED
            assert reply.path == '/reply'
        else:
            peer.sendall(wire.build_frame(wire.FT_TELL, 2, '/user/pool-0', 'mutate', b'effect'))
            assert exchange(wire.FT_HEARTBEAT, 3).ft == wire.FT_HEARTBEAT_ACK
        assert worker.ask.calls == worker.deliver.calls == [] and effects == []
        assert admin.stop('/user/pool')['kind'] == 'stopped'
        worker.mount_module = Remote(runtime.mount_module)
        assert admin.deploy(comp())['kind'] == 'deployed'
        reply = exchange(wire.FT_ASK, 4, '/user/pool-0', 'mutate', wire.with_reply_to_prefix('/reply', b'effect'))
        assert reply.ft == wire.FT_REPLY and effects == ['/user/pool-0']
        assert admin.stop('/user/pool')['kind'] == 'stopped'
    finally:
        peer.close()
        thread.join(5)
        if thread.is_alive():
            local.shutdown(socket.SHUT_RDWR)
            thread.join(5)
        assert not thread.is_alive() and failures == []
