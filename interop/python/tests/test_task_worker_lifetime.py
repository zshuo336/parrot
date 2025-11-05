"""The Ray adapter changes worker lifetime, never user task execution."""
from collections import namedtuple
import types

import pytest

from parrot_protocol.ray_tasks import install_task_worker_lifetime

Info = namedtuple('Info', 'function function_name max_calls')


class Manager:
    def __init__(self, info):
        self.info, self.requests = info, []
    def get_execution_info(self, job, descriptor):
        self.requests.append((job, descriptor))
        return self.info


@pytest.mark.parametrize('actor', [False, True], ids=['task','actor'])
@pytest.mark.parametrize('limit', [0,1,5])
def test_native_lifetime_boundary_preserves_function_identity_and_actor_policy(actor, limit):
    function=lambda:object()
    info=Info(function, 'component.work', limit)
    manager=Manager(info)
    descriptor=types.SimpleNamespace(is_actor_method=lambda:actor)
    install_task_worker_lifetime(manager)
    actual=manager.get_execution_info('job',descriptor)
    assert actual.function is function and actual.function_name=='component.work'
    assert actual.max_calls==(limit if actor else 1)
    assert manager.requests==[('job',descriptor)]
    assert info.max_calls==limit, 'shared Ray metadata must not be changed in place'
    if actor:assert actual is info


def test_installation_is_idempotent_and_does_not_patch_unrelated_managers():
    info=Info(lambda:None, 'component.work', 0)
    owned=Manager(info);unrelated=Manager(info)
    descriptor=types.SimpleNamespace(is_actor_method=lambda:False)
    install_task_worker_lifetime(owned)
    first=owned.get_execution_info
    install_task_worker_lifetime(owned)
    assert owned.get_execution_info is first
    assert unrelated.get_execution_info('job',descriptor) is info
    assert unrelated.info.max_calls==0


def test_function_loading_failure_is_not_converted_to_success():
    def failed(*args):raise LookupError('missing exported function')
    manager=types.SimpleNamespace(get_execution_info=failed)
    install_task_worker_lifetime(manager)
    with pytest.raises(LookupError, match='missing exported function'):
        manager.get_execution_info('job',object())


@pytest.mark.parametrize('owned_job', ['01000000', '02000000', ''])
def test_deployment_job_identity_controls_worker_reuse(monkeypatch, owned_job):
    import ray._private.worker
    manager = Manager(Info(lambda: None, 'work', 5))
    original = manager.get_execution_info
    worker = types.SimpleNamespace(current_job_id=types.SimpleNamespace(hex=lambda:'01000000'),
                                   function_actor_manager=manager)
    monkeypatch.setattr(ray._private.worker, 'global_worker', worker)
    monkeypatch.setenv('PARROT_DEPLOYMENT_JOB', owned_job)
    if owned_job == '01000000':
        install_task_worker_lifetime()
        assert manager.get_execution_info == original
        assert manager.get_execution_info('01000000', object()).max_calls == 5
    else:
        with pytest.raises(RuntimeError, match='different Ray job'): install_task_worker_lifetime()
        assert manager.get_execution_info == original
