"""Fault-controlled RPC boundaries around real snapshots and owner decisions."""
from pathlib import Path
import types

import pytest

from parrot_protocol.ray_gw import ParrotDispatcher
from parrot_protocol.ray_hosts import IsolatedRuntime


class ActorDied(Exception): pass


class Method:
    def __init__(self, fn): self.fn = fn
    def remote(self, *args): return lambda: self.fn(*args)


class Host:
    def __init__(self, ray):
        self.ray, self.paths, self.dead = ray, set(), False
        self.mount_module = Method(self.mount)
        self.retire_paths = Method(self.retire)
        self.ready = Method(self.check)
    def check(self):
        if self.dead: raise ActorDied()
        return True
    def mount(self, name, module, root, env, paths, version, config):
        self.paths.update(paths)
        if self.ray.lost_mount: raise RuntimeError('lost receipt after commit')
        if self.ray.failed_mount:
            self.paths.clear()
            return dict(ok=False,paths=[],detail='user factory failed')
        return dict(ok=True,paths=paths)
    def retire(self, name, paths, timeout):
        result = {}
        for path in paths:
            if path in self.ray.failed_paths: result[path] = 'stop_failed'
            else:
                self.paths.discard(path)
                result[path] = None
        return result


class Ray:
    exceptions = types.SimpleNamespace(ActorDiedError=ActorDied)
    def __init__(self):
        self.fail_create = self.fail_kill = self.lost_mount = self.failed_mount = False
        self.fail_job_close = False
        self.hosts, self.failed_paths, self.jobs = [], set(), []
    def get(self, ref, timeout=None): return ref()
    def kill(self, host, no_restart):
        assert not host.paths, 'never discard an actor whose path retirement is uncertain'
        if self.fail_kill: raise RuntimeError('kill submission failed')
        host.dead = True
    def get_runtime_context(self): return types.SimpleNamespace(get_node_id=lambda: '0'*56, runtime_env={})
    def remote(self, cls): return self
    def options(self, **kwargs):
        assert kwargs['num_cpus'] == 0
        assert kwargs['runtime_env']['env_vars']['PYTHONPATH'].split(':')[0].startswith('/tmp/') or 'parrot-artifact-' in kwargs['runtime_env']['env_vars']['PYTHONPATH']
        return types.SimpleNamespace(remote=self.create)
    def create(self, root):
        if self.fail_create: raise RuntimeError('host scheduler rejected creation')
        host = Host(self); self.hosts.append(host)
        return host


class Job:
    def __init__(self, ray, context, root, env):
        self.ray, self.root, self.env = ray, root, env
        self.closed, self.close_calls = False, 0
        ray.jobs.append(self)
    def start(self):
        return self.ray.options(num_cpus=0,runtime_env=self.env).remote(self.root)
    def close(self, deadline=None):
        self.close_calls += 1
        if self.ray.fail_job_close:raise TimeoutError('job driver still running')
        self.closed = True


@pytest.fixture
def setup(tmp_path,monkeypatch):
    from parrot_protocol import ray_hosts
    monkeypatch.setattr(ray_hosts,'DeploymentJob',Job)
    source = tmp_path/'source'; source.mkdir(); (source/'entry.py').write_text('value = 1\n')
    ray = Ray(); runtime = IsolatedRuntime(ParrotDispatcher(),ray)
    yield runtime, ray, source
    ray.fail_kill = False; ray.fail_job_close = False; ray.failed_paths.clear()
    for name in list(runtime._deployments):
        paths = [p for p,e in runtime._entries.items() if e['owner'] == name]
        assert all(v is None for v in runtime.retire_paths(name,paths).values())


def mount(runtime, source, name='app'):
    return runtime.mount_module(name,'entry',str(source),{},['/user/'+name+'-0','/user/'+name+'-1'],'1',None)


def test_runtime_close_retires_every_deployment_and_seals_future_effects(setup):
    runtime, ray, source = setup
    assert mount(runtime, source)['ok']
    assert mount(runtime, source, 'second')['ok']
    roots = [Path(d['snapshot'].root) for d in runtime._deployments.values()]
    assert runtime.close() == {'ok': True, 'pending': []}
    assert all(host.dead for host in ray.hosts)
    assert not any(root.exists() for root in roots)
    assert not runtime._entries and not runtime._deployments
    assert mount(runtime, source, 'late')['paths'] == []
    assert len(ray.hosts) == 2
    assert runtime.close() == {'ok': True, 'pending': []}


def test_job_close_timeout_retains_snapshot_owner_and_retries_without_killing_host_again(setup, monkeypatch):
    runtime, ray, source = setup
    assert mount(runtime, source)['ok']
    deployment = runtime._deployments['app']
    snapshot = Path(deployment['snapshot'].root)
    paths = ['/user/app-0', '/user/app-1']
    ray.fail_job_close = True
    assert set(runtime.retire_paths('app', paths).values()) == {'stop_failed'}
    assert snapshot.is_dir() and runtime._deployments['app'] is deployment
    assert deployment['dead'] and not deployment['job'].closed
    assert set(runtime._entries) == set(paths)
    assert not mount(runtime, source)['ok']
    assert len(ray.jobs) == 1
    def forbidden(*a, **k): raise AssertionError('confirmed dead host killed again')
    monkeypatch.setattr(ray, 'kill', forbidden)
    ray.fail_job_close = False
    assert runtime.retire_paths('app', paths) == dict.fromkeys(paths)
    assert deployment['job'].closed and deployment['job'].close_calls == 2
    assert not snapshot.exists() and not runtime._deployments and not runtime._entries


def test_runtime_close_preserves_uncertain_owner_and_retries_only_remaining_work(setup):
    runtime, ray, source = setup
    assert mount(runtime, source)['ok']
    assert mount(runtime, source, 'second')['ok']
    root = Path(runtime._deployments['app']['snapshot'].root)
    ray.failed_paths.add('/user/app-1')
    assert runtime.close() == {'ok': False, 'pending': ['app']}
    assert root.exists() and not ray.hosts[0].dead and ray.hosts[1].dead
    assert set(runtime._entries) == {'/user/app-1'}
    assert mount(runtime, source, 'late')['paths'] == []
    ray.failed_paths.clear()
    assert runtime.close() == {'ok': True, 'pending': []}
    assert not root.exists() and ray.hosts[0].dead


@pytest.mark.parametrize('fault', ['lost_mount','failed_mount'])
def test_failed_mount_preserves_uncertain_cleanup_for_stop(setup, fault):
    runtime, ray, source = setup
    setattr(ray,fault,True); ray.fail_kill = True
    reply = mount(runtime,source)
    assert not reply['ok'] and len(reply['paths']) == 2
    snapshot = Path(runtime._deployments['app']['snapshot'].root)
    assert snapshot.exists() and runtime.ask('/user/app-0','read',b'')['code'] == 3
    result = runtime.retire_paths('app',reply['paths'])
    assert set(result.values()) == {'stop_failed'} and snapshot.exists()
    ray.fail_kill = False
    assert set(runtime.retire_paths('app',reply['paths']).values()) == {None}
    assert not snapshot.exists() and runtime._deployments == {} and ray.hosts[0].dead


def test_partial_retirement_keeps_snapshot_and_only_pending_path(setup):
    runtime, ray, source = setup
    assert mount(runtime,source)['ok']
    snapshot = Path(runtime._deployments['app']['snapshot'].root)
    ray.failed_paths.add('/user/app-1')
    assert runtime.retire_paths('app',['/user/app-0','/user/app-1']) == {'/user/app-0':None,'/user/app-1':'stop_failed'}
    assert snapshot.exists() and not ray.hosts[0].dead
    assert runtime.ask('/user/app-0','read',b'')['code'] == 1
    ray.failed_paths.clear()
    assert runtime.retire_paths('app',['/user/app-1']) == {'/user/app-1':None}
    assert not snapshot.exists() and ray.hosts[0].dead


def test_host_creation_failure_releases_snapshot_and_capacity(setup, tmp_path, monkeypatch):
    from parrot_protocol import ray_artifacts
    runtime, ray, source = setup
    temp = tmp_path/'snapshots'; temp.mkdir()
    monkeypatch.setattr(ray_artifacts.tempfile,'tempdir',str(temp))
    ray.fail_create = True
    assert mount(runtime,source)['paths'] == []
    assert not runtime._deployments and not runtime._entries and not list(temp.iterdir())
    ray.fail_create = False
    assert mount(runtime,source)['ok']


@pytest.mark.parametrize('budget', ['bytes','hosts'])
def test_owner_capacity_retained_until_retirement_and_reused(setup, monkeypatch, budget):
    from parrot_protocol import ray_hosts
    runtime, ray, source = setup
    if budget == 'bytes': monkeypatch.setattr(ray_hosts,'MAX_OWNED_BYTES',(source/'entry.py').stat().st_size)
    else: monkeypatch.setattr(ray_hosts,'MAX_HOSTS',1)
    assert mount(runtime,source)['ok']
    assert mount(runtime,source,'second')['paths'] == []
    assert len(ray.hosts) == 1
    assert set(runtime.retire_paths('app',['/user/app-0','/user/app-1']).values()) == {None}
    assert mount(runtime,source,'second')['ok']


def test_alive_probe_after_kill_keeps_snapshot_until_actual_death(setup):
    runtime, ray, source = setup
    assert mount(runtime,source)['ok']
    snapshot = Path(runtime._deployments['app']['snapshot'].root)
    host = ray.hosts[0]
    probes = []
    def delayed_death():
        assert snapshot.exists(), 'source released before death confirmation'
        probes.append(True)
        if len(probes) == 3: raise ActorDied()
        return True
    host.ready = Method(delayed_death)
    assert set(runtime.retire_paths('app',['/user/app-0','/user/app-1']).values()) == {None}
    assert len(probes) == 3 and not snapshot.exists()


def test_unconfirmed_death_retains_snapshot_and_retry_owns_cleanup(setup, monkeypatch):
    from parrot_protocol import ray_gw
    runtime, ray, source = setup
    assert mount(runtime,source)['ok']
    snapshot = Path(runtime._deployments['app']['snapshot'].root)
    host = ray.hosts[0]
    original = host.ready
    host.ready = Method(lambda: True)
    clock = [100.0]
    def advance(_): clock[0] += 6
    # Drive the actual deadline path without spending five seconds on a fake RPC.
    with monkeypatch.context() as patch:
        patch.setattr(ray_gw.time,'monotonic',lambda:clock[0])
        patch.setattr(ray_gw.time,'sleep',advance)
        result = runtime.retire_paths('app',['/user/app-0','/user/app-1'])
    assert set(result.values()) == {'stop_failed'} and snapshot.exists()
    host.ready = original
    assert set(runtime.retire_paths('app',['/user/app-0','/user/app-1']).values()) == {None}
    assert not snapshot.exists()


def test_failed_environment_setup_kill_failure_retains_snapshot_for_retry(setup,monkeypatch):
    runtime,ray,source=setup
    class SetupError(Exception): pass
    monkeypatch.setattr(ray.exceptions,'RuntimeEnvSetupError',SetupError,raising=False)
    create=ray.create
    def create_failed(root):
        host=create(root)
        def failed(*args): raise SetupError('installer failed before actor execution')
        host.mount_module=Method(failed);host.ready=Method(failed)
        def forbidden(*args): raise AssertionError('failed setup must not submit user retirement hooks')
        host.retire_paths=Method(forbidden)
        return host
    monkeypatch.setattr(ray,'create',create_failed)
    ray.fail_kill=True
    result=mount(runtime,source)
    assert not result['ok'] and len(result['paths'])==2
    snapshot=Path(runtime._deployments['app']['snapshot'].root)
    assert snapshot.exists() and not ray.hosts[0].paths
    ray.fail_kill=False
    assert set(runtime.retire_paths('app',result['paths']).values())=={None}
    assert not snapshot.exists() and not runtime._deployments


def test_task_error_wrapping_setup_error_preserves_uncertain_commit(setup,monkeypatch):
    from ray.exceptions import RuntimeEnvSetupError,RayTaskError
    runtime,ray,source=setup
    monkeypatch.setattr(ray.exceptions,'RuntimeEnvSetupError',RuntimeEnvSetupError,raising=False)
    error=RayTaskError('mount','task failed after commit',RuntimeEnvSetupError('user exception')).as_instanceof_cause()
    assert isinstance(error,RuntimeEnvSetupError) and isinstance(error,RayTaskError)
    create=ray.create
    def uncertain(root):
        host=create(root)
        def lost(*args):
            host.mount(*args)
            raise error
        host.mount_module=Method(lost)
        return host
    monkeypatch.setattr(ray,'create',uncertain)
    reply=mount(runtime,source)
    assert not reply['ok'] and len(reply['paths'])==2
    snapshot=Path(runtime._deployments['app']['snapshot'].root)
    # Existing paths prove that treating this as 'factory never ran' is wrong.
    assert len(ray.hosts[0].paths)==2
    assert set(runtime.retire_paths('app',reply['paths']).values())=={None}
    assert not snapshot.exists() and not ray.hosts[0].paths


def test_user_task_exception_is_not_actor_death_confirmation(monkeypatch):
    from ray.exceptions import RuntimeEnvSetupError,RayTaskError
    from parrot_protocol.ray_gw import _kill_and_confirm
    ray=Ray()
    monkeypatch.setattr(ray.exceptions,'RuntimeEnvSetupError',RuntimeEnvSetupError,raising=False)
    host=Host(ray)
    error=RayTaskError('ready','user probe failure',RuntimeEnvSetupError('user exception')).as_instanceof_cause()
    def failure(): raise error
    with pytest.raises(RayTaskError):
        _kill_and_confirm(ray,host,Method(failure))


def test_confirmed_dead_host_can_retire_paths_and_snapshot_without_another_rpc(setup):
    runtime, ray, source = setup
    assert mount(runtime, source)['ok']
    host=ray.hosts[0]
    root=Path(runtime._deployments['app']['snapshot'].root)
    calls=[]
    def died(*args):
        calls.append(args)
        raise ActorDied('host worker already exited')
    host.dead=True;host.paths.clear();host.retire_paths=Method(died)
    try:
        assert runtime.retire_paths('app',['/user/app-0'])=={'/user/app-0':None}
        assert root.is_dir(), 'other logical paths still own the snapshot'
        assert runtime.retire_paths('app',['/user/app-1'])=={'/user/app-1':None}
        assert len(calls)==1 and not root.exists()
        assert not runtime._deployments and not runtime._entries
    finally:
        host.retire_paths=Method(host.retire)


def test_user_exception_subclass_does_not_prove_host_death(setup):
    runtime, ray, source = setup
    assert mount(runtime, source)['ok']
    host=ray.hosts[0]
    class WrappedUserError(ActorDied):pass
    def user_error(*args):raise WrappedUserError('user cause wrapped by RayTaskError')
    host.retire_paths=Method(user_error)
    try:
        assert runtime.retire_paths('app',['/user/app-0'])=={'/user/app-0':'stop_failed'}
        assert runtime._deployments['app']['dead'] is False
        assert Path(runtime._deployments['app']['snapshot'].root).is_dir()
    finally:
        host.retire_paths=Method(host.retire)


def test_dead_host_cannot_remove_snapshot_still_leased_by_other_worker(setup):
    import fcntl, os
    runtime, ray, source = setup
    assert mount(runtime, source)['ok']
    host=ray.hosts[0];snapshot=runtime._deployments['app']['snapshot']
    root=Path(snapshot.root)
    fd=os.open(snapshot._lease.path/'lease',os.O_RDONLY)
    fcntl.flock(fd,fcntl.LOCK_SH)
    calls=[]
    def died(*args):
        calls.append(args)
        raise ActorDied('host already exited')
    host.dead=True;host.paths.clear();host.retire_paths=Method(died)
    paths=['/user/app-0','/user/app-1']
    try:
        assert runtime.retire_paths('app',paths)==dict.fromkeys(paths,'stop_failed')
        assert root.is_dir() and runtime._deployments['app']['dead']
        assert set(runtime._entries)==set(paths)
    finally:
        os.close(fd)
        host.retire_paths=Method(host.retire)
    assert runtime.retire_paths('app',paths)==dict.fromkeys(paths)
    assert len(calls)==1 and not root.exists()
