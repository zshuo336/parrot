"""Preserve Ray hook inheritance without losing per-worker snapshot leases."""
from copy import deepcopy
import os
import sys
import types

import pytest
import ray
import ray.actor
import ray.remote_function
from ray._private.ray_constants import WORKER_PROCESS_SETUP_HOOK_ENV_VAR as HOOK_KEY
from ray._private.runtime_env import setup_hook as ray_hooks
from parrot_protocol.ray_environments import host_environment, validate_environment
from parrot_protocol import ray_snapshot_lease as leases


@pytest.mark.parametrize('parent,expected', [
    ({}, None),
    ({'worker_process_setup_hook': 'app.setup'}, 'app.setup'),
    ({'worker_process_setup_hook': 'display_name', 'env_vars': {HOOK_KEY: 'ray_runtime_env_func::opaque'}}, 'ray_runtime_env_func::opaque'),
    ({'worker_process_setup_hook': leases.SETUP_HOOK, 'env_vars': {HOOK_KEY: leases.SETUP_HOOK, 'PARROT_SNAPSHOT_USER_HOOK': 'app.setup'}}, 'app.setup'),
    ({'worker_process_setup_hook': 'child.setup', 'env_vars': {HOOK_KEY: leases.SETUP_HOOK, 'PARROT_SNAPSHOT_USER_HOOK': 'app.setup'}}, 'child.setup'),
    ({'worker_process_setup_hook': '', 'env_vars': {HOOK_KEY: leases.SETUP_HOOK, 'PARROT_SNAPSHOT_USER_HOOK': 'app.setup'}}, ''),
], ids=['none','import','exported-callable','wrapped-parent','child-override','child-disables'])
def test_host_carries_effective_parent_hook_without_mutating_environment(parent, expected):
    saved = deepcopy(parent)
    result = host_environment({}, '/payload', '/protocol', lease='/control', inherited_env=parent)
    assert result['env_vars']['PARROT_SNAPSHOT_USER_HOOK'] == (expected or '')
    assert result['env_vars'][HOOK_KEY] == leases.SETUP_HOOK
    assert result['worker_process_setup_hook'] == leases.SETUP_HOOK
    assert parent == saved


@pytest.mark.parametrize('override', ['app.override', ''])
def test_explicit_deployment_hook_replaces_inherited_hook_instead_of_chaining(override):
    result = host_environment({'worker_process_setup_hook': override}, '/payload', '/protocol',
        lease='/control', inherited_env={'env_vars': {HOOK_KEY: 'ray_runtime_env_func::opaque'}})
    assert result['env_vars']['PARROT_SNAPSHOT_USER_HOOK'] == override


def test_users_cannot_configure_recursive_internal_hook():
    with pytest.raises(ValueError, match='internal'):
        validate_environment({'worker_process_setup_hook': leases.SETUP_HOOK})


@pytest.mark.parametrize('kind', ['import','callable','child','disabled','import-failure','callable-failure'])
def test_worker_runs_effective_hook_once_after_lease_and_propagates_failure(tmp_path, monkeypatch, kind):
    monkeypatch.setattr(leases.tempfile, 'tempdir', str(tmp_path))
    snapshot = leases.SnapshotLease()
    control = str(snapshot.path)
    snapshot._close_descriptors()
    calls = []
    module = types.ModuleType('hook_contract')
    def record(name):
        assert leases.collect_orphaned_snapshots(tmp_path)['busy'] == [control]
        calls.append(name)
    def imported():
        record('import')
        if kind == 'import-failure':raise ValueError('user setup failed')
    def child():record('child')
    module.imported = imported;module.child = child
    monkeypatch.setitem(sys.modules, 'hook_contract', module)
    opaque = 'ray_runtime_env_func::opaque'
    def loaded(reference):
        assert reference == opaque
        record('callable')
        return 'user setup failed' if kind == 'callable-failure' else None
    monkeypatch.setattr(ray_hooks, 'load_and_execute_setup_hook', loaded)
    user = opaque if kind.startswith('callable') else 'hook_contract.imported'
    selected = {'child':'hook_contract.child','disabled':''}.get(kind, leases.SETUP_HOOK)
    def no_context():
        raise AssertionError('runtime context is not safe during worker setup')
    monkeypatch.setattr(ray, 'get_runtime_context', no_context)
    manager = ray._private.worker.global_worker.function_actor_manager
    monkeypatch.setattr(manager, 'get_execution_info', manager.get_execution_info)
    for module in (ray.actor, ray.remote_function):
        monkeypatch.setattr(module, 'parse_runtime_env_for_task_or_actor', module.parse_runtime_env_for_task_or_actor)
    monkeypatch.setenv('PARROT_SNAPSHOT_USER_HOOK', user if selected == leases.SETUP_HOOK else selected)
    monkeypatch.setenv('PARROT_SNAPSHOT_CONTROL', control)
    try:
        if kind.endswith('failure'):
            with pytest.raises((RuntimeError, ValueError), match='user setup failed'):
                leases.worker_setup()
        else:
            leases.worker_setup()
        expected = [] if kind == 'disabled' else [kind.split('-')[0]]
        assert calls == expected
        assert leases.collect_orphaned_snapshots(tmp_path)['busy'] == [control]
    finally:
        owned = leases._WORKER_LEASES.pop(control, None)
        if owned is not None:
            directory, descriptor = owned
            os.close(descriptor);os.close(directory)
        assert leases.collect_orphaned_snapshots(tmp_path)['removed'] == [control]


@pytest.mark.parametrize('module', [ray.actor, ray.remote_function], ids=['actor','task'])
def test_submission_adapter_is_idempotent_and_preserves_requests(monkeypatch, module):
    from parrot_protocol.ray_environments import install_child_hook_adapter
    for target in (ray.actor, ray.remote_function):
        monkeypatch.setattr(target, 'parse_runtime_env_for_task_or_actor', target.parse_runtime_env_for_task_or_actor)
    original = module.parse_runtime_env_for_task_or_actor
    monkeypatch.setenv('PARROT_SNAPSHOT_CONTROL', '/control')
    request={'worker_process_setup_hook':'child.setup','env_vars':{'EXTRA':'value'},'pip':['local-demo==1']}
    saved=deepcopy(request)
    install_child_hook_adapter()
    adapter=module.parse_runtime_env_for_task_or_actor
    install_child_hook_adapter()
    assert module.parse_runtime_env_for_task_or_actor is adapter
    result=adapter(request)
    assert result['worker_process_setup_hook']==leases.SETUP_HOOK
    assert result['env_vars']['PARROT_SNAPSHOT_USER_HOOK']=='child.setup'
    assert result['env_vars']['PARROT_SNAPSHOT_CONTROL']=='/control'
    assert result['env_vars'][HOOK_KEY]==leases.SETUP_HOOK
    assert result['env_vars']['EXTRA']=='value' and result['pip']==original({'pip':['local-demo==1']})['pip']
    assert request==saved
    assert adapter(None) is None
    # Without a lease, preserve Ray's ordinary parsing contract.
    monkeypatch.delenv('PARROT_SNAPSHOT_CONTROL')
    assert adapter(request)==original(request)


@pytest.mark.parametrize('key', ['PARROT_SNAPSHOT_CONTROL','PARROT_SNAPSHOT_USER_HOOK','PARROT_DEPLOYMENT_JOB',HOOK_KEY])
def test_child_requests_cannot_override_lease_metadata_even_without_user_hook(key):
    from parrot_protocol.ray_environments import child_environment
    with pytest.raises(ValueError,match='reserved'):
        child_environment({'env_vars':{key:'replacement'}},'/control')


def test_child_hook_rejects_nonportable_callable_before_submission():
    from parrot_protocol.ray_environments import child_environment
    with pytest.raises(ValueError,match='importable'):
        child_environment({'worker_process_setup_hook':lambda:None},'/control')
