"""Bounded gateway retirement retains ownership across uncertain outcomes."""
import threading
import time
import types

import pytest

from parrot_protocol import ray_gw as gateway


class ActorDied(Exception):
    pass


class Worker:
    def __init__(self):
        self.submissions = 0
        self.dead = False
        self.receipt = {'ok': True, 'pending': []}
        self.close = types.SimpleNamespace(remote=self.submit)
        self.ready = types.SimpleNamespace(remote=self.probe)
    def submit(self):
        self.submissions += 1
        return 'retirement'
    def probe(self):
        if self.dead:
            raise ActorDied()
        return True


class Ray:
    exceptions = types.SimpleNamespace(ActorDiedError=ActorDied, GetTimeoutError=TimeoutError)
    def __init__(self, worker):
        self.worker = worker
        self.failure = None
        self.kills = 0
        self.kill_failure = False
    def get(self, ref, timeout):
        if self.failure:
            raise self.failure
        return self.worker.receipt if ref == 'retirement' else ref
    def kill(self, worker, no_restart):
        assert worker is self.worker and no_restart
        if self.kill_failure:
            raise OSError('death report unavailable')
        self.kills += 1
        worker.dead = True


def fixture():
    worker = Worker()
    ray = Ray(worker)
    state = gateway.RayAdminState()
    owner = gateway._GatewayResources(ray, state)
    owner.worker = worker
    return owner, state, worker, ray


def test_management_close_discards_queued_effects_but_retains_running_slot_until_return():
    queue = gateway._ManagementQueue()
    entered, release = threading.Event(), threading.Event()
    effects = []
    def blocked():
        entered.set()
        assert release.wait(5)
        effects.append('running')
    try:
        assert queue.submit(blocked, (), 100)
        assert entered.wait(2)
        assert queue.submit(lambda: effects.append('queued'), (), 200)
        with pytest.raises(TimeoutError, match='still running'):
            queue.close(time.monotonic() + .01)
        assert queue._count == 1 and queue._bytes == 100
        assert not queue.submit(lambda: effects.append('late'), (), 1)
    finally:
        release.set()
        queue.close(time.monotonic() + 3)
    assert effects == ['running']
    assert queue._count == queue._bytes == 0


def test_active_request_timeout_seals_admission_without_killing_runtime_then_retry_completes():
    owner, state, worker, ray = fixture()
    permit = state.ask_admission.acquire(100)
    with pytest.raises(gateway.GatewayCleanupError) as error:
        owner.close(.01)
    assert error.value.owner is owner and isinstance(error.value.__cause__, TimeoutError)
    assert owner.worker is worker and worker.submissions == ray.kills == 0
    assert state.ask_admission.acquire(1) is None
    assert state.route_error('/user/not-yet-published')['code'] == 3
    permit.release()
    error.value.owner.close()
    assert ray.kills == 1 and worker.dead and owner.worker is None
    owner.close()
    assert ray.kills == 1


@pytest.mark.parametrize('failure,submissions', [(TimeoutError('RPC pending'), 1), (ValueError('RPC completed with error'), 2)])
def test_retry_reuses_pending_rpc_but_resubmits_after_terminal_error(failure, submissions):
    owner, state, worker, ray = fixture()
    ray.failure = failure
    with pytest.raises(gateway.GatewayCleanupError) as error:
        owner.close()
    assert error.value.owner is owner and not worker.dead
    ray.failure = None
    owner.close()
    assert worker.submissions == submissions and ray.kills == 1


@pytest.mark.parametrize('receipt', [{'ok': False, 'pending': ['app']}, {'ok': 1, 'pending': []}, None])
def test_incomplete_or_invalid_retirement_never_kills_the_coordinator(receipt):
    owner, state, worker, ray = fixture()
    worker.receipt = receipt
    with pytest.raises(gateway.GatewayCleanupError) as error:
        owner.close()
    assert error.value.owner is owner and ray.kills == 0 and owner.worker is worker
    worker.receipt = {'ok': True, 'pending': []}
    owner.close()
    assert worker.submissions == 2 and worker.dead


def test_uncertain_coordinator_death_retries_confirmation_without_repeating_deployment_close():
    owner, state, worker, ray = fixture()
    ray.kill_failure = True
    with pytest.raises(gateway.GatewayCleanupError):
        owner.close()
    assert owner.worker is worker
    ray.kill_failure = False
    owner.close()
    assert worker.submissions == 1 and worker.dead


@pytest.mark.parametrize('timeout', [0, -1, float('nan'), float('inf')])
def test_invalid_deadline_has_no_side_effects(timeout):
    owner, state, worker, ray = fixture()
    with pytest.raises(ValueError):
        owner.close(timeout)
    assert not state.closing and worker.submissions == ray.kills == 0


@pytest.mark.parametrize('already_initialized', [False, True])
def test_bind_failure_retires_coordinator_without_shutting_down_the_ray_job(monkeypatch, already_initialized):
    import sys
    import socket
    from parrot_protocol import ray_startup
    owner, state, worker, ray = fixture()
    ray.is_initialized = lambda: already_initialized
    remote = types.SimpleNamespace(options=lambda **kw: remote, remote=lambda *args: worker)
    ray.remote = lambda cls: remote
    monkeypatch.setitem(sys.modules, 'ray', ray)
    initializations = []
    def initialize(*args):
        assert not already_initialized, 'reinitialized caller runtime'
        initializations.append(True)
    monkeypatch.setattr(ray_startup, 'initialize', initialize)
    with socket.socket() as occupied:
        occupied.bind(('127.0.0.1', 0))
        occupied.listen()
        with pytest.raises(OSError):
            gateway.serve(occupied.getsockname()[1])
    assert worker.dead and ray.kills == 1
    assert initializations == ([] if already_initialized else [True])
    if already_initialized:
        with pytest.raises(ValueError, match='cannot reconfigure'):
            gateway.serve(0, ray_kwargs={'address': 'local'})
