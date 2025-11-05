"""Local dispatch is dependency-free; native actors keep explicit Ray ownership."""
import builtins
import queue
import types

import pytest

from parrot_protocol import ray_gw as gateway


@pytest.fixture
def ray_imports(monkeypatch):
    original = builtins.__import__
    attempts = []

    def without_ray(name, *args, **kwargs):
        if name == 'ray' or name.startswith('ray.'):
            attempts.append(name)
            raise ModuleNotFoundError('blocked optional Ray import: ' + name)
        return original(name, *args, **kwargs)

    monkeypatch.setattr(builtins, '__import__', without_ray)
    return attempts


def executor(dispatcher, ray=None):
    return gateway.RayAdminExecutor(queue.Queue(), ray_module=ray, gateway_dispatcher=dispatcher)


def test_local_dispatch_and_missing_actor_need_no_ray(ray_imports):
    dispatcher = gateway.ParrotDispatcher()
    calls = []

    @dispatcher.handler('bin:local')
    def local(key, payload):
        calls.append((key, payload))
        return 'bin:reply', payload + b'-reply'

    owner = executor(dispatcher)
    runtime = owner._runtime()
    assert owner._runtime() is runtime
    assert dispatcher._component_runtime is runtime
    assert runtime.ask('/user/local', 'bin:local', b'ask') == dict(ok=True, key='bin:reply', payload=b'ask-reply')
    assert runtime.deliver('/user/local', 'bin:local', b'tell') is None
    assert calls == [('bin:local', b'ask'), ('bin:local', b'tell')]
    assert runtime.ask('/user/missing', 'bin:unknown', b'')['code'] == 1
    assert ray_imports == []


def test_native_dependency_is_still_resolved_lazily(ray_imports):
    owner = executor(gateway.ParrotDispatcher())
    runtime = owner._runtime()
    assert ray_imports == []
    with pytest.raises(ModuleNotFoundError, match='blocked optional Ray import'):
        _ = runtime.ray
    with pytest.raises(ModuleNotFoundError, match='blocked optional Ray import'):
        _ = owner.ray
    assert ray_imports == ['ray', 'ray']


def test_native_calls_use_the_injected_ray_instance(ray_imports):
    calls = []
    reference = object()

    def remote(key, payload):
        calls.append(('remote', key, payload))
        return reference

    def get(value):
        assert value is reference
        calls.append(('get', value))
        return 'bin:native-reply', b'native-result'

    ray = types.SimpleNamespace(get=get)
    owner = executor(gateway.ParrotDispatcher(), ray)
    runtime = owner._runtime()
    actor = types.SimpleNamespace(ask=types.SimpleNamespace(remote=remote))
    runtime._entries['/user/native'] = dict(owner='native', state='running', handle=actor)
    assert runtime.ask('/user/native', 'bin:request', b'body') == dict(
        ok=True, key='bin:native-reply', payload=b'native-result')
    assert calls == [('remote', 'bin:request', b'body'), ('get', reference)]
    assert owner.ray is runtime.ray is ray
    assert ray_imports == []
