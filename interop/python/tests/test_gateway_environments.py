"""Runtime environment preflight must reject before deployment side effects."""
import queue
import types

import pytest
from parrot_protocol.ray_gw import RayAdminExecutor


def component(env):
    return dict(name='app',version='1',artifact=dict(kind='pymodule',module='environment_contract',runtime_env=env),instances=dict(kind='singleton'))


@pytest.fixture
def remote_executor():
    calls = []
    def mount(*args):
        calls.append(args)
        return dict(ok=True,paths=args[4])
    worker = types.SimpleNamespace(mount_module=types.SimpleNamespace(remote=mount))
    ex = RayAdminExecutor(queue.Queue(),types.SimpleNamespace(get=lambda value:value),worker_ref=worker)
    return ex,calls


def test_valid_toml_is_parsed_not_wrapped_as_raw(remote_executor):
    ex,calls = remote_executor
    assert ex.deploy(component('[env_vars]\nPARROT_ENV_CASE = "alpha"\n'))['kind']=='deployed'
    assert calls[0][3] == {'env_vars':{'PARROT_ENV_CASE':'alpha'}}


@pytest.mark.parametrize('env', ['[env_vars', 'unknown_option = "unused"', 'env_vars = "wrong"',
    '[env_vars]\nPARROT_ENV_CASE = 42', '[env_vars]\n"BAD=NAME" = "value"',
    '[env_vars]\nVALUE = "bad\\u0000value"', 'pip = 42', 'pip = ["one", 2]',
    'pip = ["one"]\nconda = "other"', '[config]\nsetup_timeout_seconds = -1',
    '[config]\nsetup_timeout_seconds = 0', '[config]\nunknown = 1'])
def test_invalid_environment_fails_before_remote_call(remote_executor,env):
    ex,calls = remote_executor
    reply = ex.deploy(component(env))
    assert reply['kind']=='failed',reply
    assert calls == [] and ex._components == {}


def test_local_executor_must_not_pretend_to_install_runtime_environment(monkeypatch):
    from parrot_protocol.ray_gw import ParrotDispatcher
    import sys
    calls=[]
    module=types.ModuleType('environment_contract')
    module.parrot_entry=lambda ctx:(calls.append(ctx),ParrotDispatcher())[1]
    monkeypatch.setitem(sys.modules,'environment_contract',module)
    ex=RayAdminExecutor(queue.Queue(),types.SimpleNamespace())
    assert ex.deploy(component('[env_vars]\nPARROT_ENV_CASE="alpha"'))['kind']=='failed'
    assert calls==[] and ex._components=={}


def test_limits_count_encoded_toml_bytes_and_variables(remote_executor):
    ex,calls=remote_executor
    assert ex.deploy(component('[env_vars]\nVALUE="'+'界'*22000+'"'))['kind']=='failed'
    assert ex.deploy(component('[env_vars]\n'+'\n'.join(f'K{i}="v"' for i in range(257))))['kind']=='failed'
    assert calls==[]
    assert ex.deploy(component('[env_vars]\n'+'\n'.join(f'K{i}="v"' for i in range(256))))['kind']=='deployed'
    assert len(calls[0][3]['env_vars'])==256


def test_host_environment_merges_without_changing_request_or_gateway(monkeypatch):
    from copy import deepcopy
    import os
    from parrot_protocol.ray_environments import host_environment
    monkeypatch.setenv('PYTHONPATH','/gateway')
    original={'env_vars':{'PYTHONPATH':'/user/extra','PARROT_ENV_CASE':'A'},'config':{'setup_timeout_seconds':30}}
    saved=deepcopy(original)
    result=host_environment(original,'/snapshot','/protocol')
    assert result['env_vars']['PYTHONPATH'].split(os.pathsep)==['/snapshot','/protocol','/user/extra','/gateway']
    assert result['env_vars']['PARROT_ENV_CASE']=='A' and result['config']==saved['config']
    result['config']['setup_timeout_seconds']=60
    assert original==saved and os.environ['PYTHONPATH']=='/gateway'


def test_dependency_environment_is_forwarded_intact(remote_executor):
    ex,calls=remote_executor
    raw='pip=["local-package==1"]\n[config]\nsetup_timeout_seconds=30\neager_install=false\n'
    assert ex.deploy(component(raw))['kind']=='deployed'
    assert calls[0][3]=={'pip':['local-package==1'],'config':{'setup_timeout_seconds':30,'eager_install':False}}


def test_new_job_preserves_parent_dependencies_and_variable_merge_without_old_ownership():
    from copy import deepcopy
    from parrot_protocol.ray_environments import host_environment
    parent = {'pip': ['parent-package==1'], 'working_dir': 'gcs://parent.zip',
              'env_vars': {'PARENT': 'yes', 'OVERRIDE': 'old', 'PARROT_DEPLOYMENT_JOB': '01000000',
                           'PARROT_SNAPSHOT_CONTROL': '/parent-control'}}
    child = {'env_vars': {'OVERRIDE': 'new'}}
    saved = deepcopy(parent), deepcopy(child)
    result = host_environment(child, '/payload', '/protocol', lease='/control', inherited_env=parent)
    assert result['pip'] == ['parent-package==1'] and result['working_dir'] == 'gcs://parent.zip'
    assert result['env_vars']['PARENT'] == 'yes' and result['env_vars']['OVERRIDE'] == 'new'
    assert result['env_vars']['PARROT_SNAPSHOT_CONTROL'] == '/control'
    assert 'PARROT_DEPLOYMENT_JOB' not in result['env_vars']
    result['pip'].append('other')
    assert (parent, child) == saved


@pytest.mark.parametrize('key', ['PARROT_SNAPSHOT_CONTROL', 'PARROT_SNAPSHOT_USER_HOOK', 'PARROT_DEPLOYMENT_JOB',
    '__RAY_WORKER_PROCESS_SETUP_HOOK_ENV_VAR'])
def test_reserved_lease_variables_fail_before_remote_side_effects(remote_executor, key):
    ex, calls = remote_executor
    assert ex.deploy(component('[env_vars]\n'+key+'="override"'))['kind'] == 'failed'
    assert calls == [] and ex._components == {}


def test_leased_environment_preserves_user_hook_without_mutating_request():
    from copy import deepcopy
    from ray._private.ray_constants import WORKER_PROCESS_SETUP_HOOK_ENV_VAR
    from parrot_protocol.ray_environments import host_environment
    original = dict(worker_process_setup_hook='component.setup',env_vars={'APP': 'value'})
    saved = deepcopy(original)
    result = host_environment(original, '/snapshot/payload', '/protocol', lease='/snapshot')
    assert result['env_vars']['PARROT_SNAPSHOT_CONTROL'] == '/snapshot'
    assert result['env_vars']['PARROT_SNAPSHOT_USER_HOOK'] == 'component.setup'
    assert result['worker_process_setup_hook'] == result['env_vars'][WORKER_PROCESS_SETUP_HOOK_ENV_VAR]
    assert original == saved
