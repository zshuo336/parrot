#!/usr/bin/env python3
"""Actual TCP -> gateway -> Ray worker -> per-path components and native actors."""
from contextlib import ExitStack
from pathlib import Path
import json
import os
import re
import signal
import socket
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'interop/python'))
sys.path.insert(0, str(ROOT / 'apps/websearch/tests'))
from parrot_protocol import admin_v2, wire
from run_integration import Suite, until


class Client:
    def __init__(self, port):
        self.sock = socket.create_connection(('127.0.0.1', port), timeout=20)
        self.decoder = wire.FrameDecoder()
        self.sequence = 0
        assert self.exchange(wire.FT_HANDSHAKE, payload=wire.handshake_body('ray-lifecycle')).ft == wire.FT_HANDSHAKE_ACK

    def send(self, kind, path='', key='', payload=b''):
        self.sequence += 1
        self.sock.sendall(wire.build_frame(kind, self.sequence, path, key, payload))

    def exchange(self, kind, path='', key='', payload=b''):
        self.send(kind, path, key, payload)
        while True:
            frame = self.decoder.next_frame()
            if frame is not None:
                assert frame.cid == self.sequence
                return frame
            data = self.sock.recv(65536)
            assert data, 'gateway closed before reply'
            self.decoder.feed(data)

    def admin(self, kind, **fields):
        request_id = self.sequence + 1
        reply = self.exchange(wire.FT_SYSTEM_EVENT, payload=admin_v2.encode_admin_cmd_v2(
            dict(kind=kind, req_id=request_id, **fields)))
        assert reply.ft == wire.FT_SYSTEM_EVENT
        result = admin_v2.decode_admin_reply_v2(reply.payload)
        assert result['req_id'] == request_id
        return result

    def ask(self, path, operation='Read', payload=b''):
        return self.exchange(wire.FT_ASK, path, 'bin:contract/' + operation,
                             wire.with_reply_to_prefix('/test/replies', payload))

    def read(self, path):
        reply = self.ask(path)
        assert reply.ft == wire.FT_REPLY, (path, reply)
        return json.loads(reply.payload)

    def missing(self, path):
        reply = self.ask(path)
        assert reply.ft == wire.FT_REPLY_ERR and wire.decode_err_payload(reply.payload)[0] == wire.ERR_ACTOR_NOT_FOUND, reply


MODULE = '''from parrot_protocol.ray_gw import ParrotDispatcher
from pathlib import Path
import json
import ray
import sys
ray.cloudpickle.register_pickle_by_value(sys.modules[__name__])

class Native:
    def __init__(self, ctx):
        if 'initfail' in ctx['path']: raise RuntimeError('native initialization failed')
        self.value = {'path': ctx['path'], 'version': ctx.get('version'), 'config': (ctx.get('config') or b'').decode(), 'count': 0}
    def ask(self, key, payload):
        if key.endswith('/Add'): self.value['count'] += 1
        return key, json.dumps(self.value).encode()
    def deliver(self, key, payload): self.value['count'] += 10
    def parrot_drain(self): return True

def parrot_entry(ctx):
    path = ctx['path']
    with Path(CALLS_PATH).open('a') as log: log.write(path + '\\n')
    if 'rollback' in path and path.endswith('-1'): raise RuntimeError('second factory failed')
    if path.startswith('/user/native-'):
        return ray.remote(Native).options(name=path.removeprefix('/user/'), lifetime='detached').remote({k:v for k,v in ctx.items() if k != 'ray'})
    dispatcher = ParrotDispatcher()
    value = {'path': path, 'version': ctx.get('version'), 'config': (ctx.get('config') or b'').decode(), 'count': 0}
    @dispatcher.handler('bin:contract/Read')
    def read(key, payload): return key, json.dumps(value).encode()
    @dispatcher.handler('bin:contract/Add')
    def add(key, payload):
        value['count'] += 1
        return key, json.dumps(value).encode()
    @dispatcher.handler('bin:contract/FailLookup')
    def fail_lookup(key, payload):
        value['count'] += 1
        raise KeyError('business lookup failed after mutation')
    @dispatcher.handler('bin:contract/BadReply')
    def bad_reply(key, payload):
        value['count'] += 1
        return chr(0xd800), payload
    @dispatcher.handler('bin:contract/Alive')
    def alive(key, payload):
        try:
            ray.get_actor(payload.decode())
            result = True
        except Exception: result = False
        return key, json.dumps(result).encode()
    return dispatcher
'''


def run():
    root = Path(tempfile.mkdtemp(prefix='parrot-ray-lifecycle-', dir='/tmp'))
    print(f'RAY_LIFECYCLE_ARTIFACTS={root}', flush=True)
    report = dict(passed=[], failed=[])
    artifact = root / 'artifact'
    artifact.mkdir()
    module = artifact / 'lifecycle_component.py'
    calls = root / 'factory.calls'
    module.write_text('CALLS_PATH = ' + repr(str(calls)) + '\n' + MODULE)
    try:
        with ExitStack() as stack:
            suite = Suite(root, ROOT / 'target/debug/websearch', stack)
            env = dict(suite.env, RAY_USAGE_STATS_ENABLED='0')
            for key in ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy'):
                env[key] = suite.runtime_proxy.base
            process = suite.process('ray', [sys.executable, ROOT/'apps/websearch/tests/ray_gateway.py', '0', root/'ray'], env)
            port = int(until(lambda: re.search(r'RAY_GW_PORT=(\d+)', process.text()), [process]).group(1))
            client = Client(port); stack.callback(client.sock.close)

            def deploy(name, count=None):
                return client.admin('deploy', component=dict(name=name, version='v7',
                    artifact=dict(kind='pymodule', module='lifecycle_component', uri=artifact.as_uri()),
                    instances=dict(kind='singleton') if count is None else dict(kind='pool', count=count),
                    config=b'mode = "per-instance"'))

            def case(name, names, run):
                try:
                    run(); report['passed'].append(name); print('RAY_LIFECYCLE_OK ' + name, flush=True)
                except Exception as error:
                    report['failed'].append(dict(name=name, error=str(error)))
                    print('RAY_LIFECYCLE_FAIL ' + name + ': ' + str(error), flush=True)
                finally:
                    for component in names:
                        client.admin('stop', path_prefix='/user/' + component)

            def pool():
                assert deploy('pool', 2)['kind'] == 'deployed'
                assert client.read('/user/pool-0')['path'] == '/user/pool-0'
                assert client.read('/user/pool-1')['path'] == '/user/pool-1'
                client.send(wire.FT_TELL, '/user/pool-0', 'bin:contract/Add', b'')
                assert client.read('/user/pool-0')['count'] == 1
                assert client.read('/user/pool-1')['count'] == 0
            case('pool paths and TELL state isolation', ['pool'], pool)

            def separate():
                assert deploy('first')['kind'] == deploy('second')['kind'] == 'deployed'
                assert client.read('/user/first')['path'] == '/user/first'
                assert client.read('parrot://ray-gw-1/ray/user/second')['path'] == '/user/second'
            case('same type key in different components', ['first','second'], separate)

            def missing():
                assert deploy('known')['kind'] == 'deployed'
                for path in ['/user/unknown', '', '/user/known/../known', '/user/known/child']:
                    client.missing(path)
                assert client.read('/user/known')['count'] == 0
            case('unknown paths cannot borrow another component', ['known'], missing)

            def reply_failures():
                assert deploy('reply-failures')['kind'] == 'deployed'
                for operation, expected in [('Missing', wire.ERR_UNKNOWN_TYPE_KEY),
                                            ('FailLookup', wire.ERR_PROTOCOL_VIOLATION),
                                            ('BadReply', wire.ERR_PROTOCOL_VIOLATION)]:
                    reply = client.ask('/user/reply-failures', operation)
                    assert reply.ft == wire.FT_REPLY_ERR
                    code, detail = wire.decode_err_payload(reply.payload)
                    assert code == expected and detail, (operation, code, detail)
                assert client.read('/user/reply-failures')['count'] == 2
                assert json.loads(client.ask('/user/reply-failures', 'Add').payload)['count'] == 3
                assert client.read('/user/reply-failures')['count'] == 3
            case('actual Ray failures preserve classification and connection without undoing effects', ['reply-failures'], reply_failures)

            def metadata():
                assert deploy('metadata')['kind'] == 'deployed'
                value = client.read('/user/metadata')
                assert value['version'] == 'v7' and value['config'] == 'mode = "per-instance"', value
            case('instance context receives version and config', ['metadata'], metadata)

            def individual(operation):
                name = 'individual-' + operation
                assert deploy(name, 2)['kind'] == 'deployed'
                args = dict(path_prefix='/user/' + name + '-0')
                if operation == 'drain': args['timeout_ms'] = 5000
                reply = client.admin(operation, **args)
                assert reply['kind'] == ('drained' if operation == 'drain' else 'stopped')
                if operation == 'drain': assert (reply['drained'],reply['aborted']) == (1,0)
                assert client.read('/user/' + name + '-1')['path'].endswith('-1')
                client.missing('/user/' + name + '-0')
                states = client.admin('status', path_prefix='/user/' + name)['states']
                assert [s['path'] for s in states] == ['/user/' + name + '-1']
            for operation in ['stop','drain']:
                case('individual pool ' + operation, ['individual-' + operation], lambda op=operation: individual(op))

            def collision():
                assert deploy('collision', 2)['kind'] == 'deployed'
                assert deploy('collision-0')['kind'] == 'failed'
                assert client.read('/user/collision-0')['path'] == '/user/collision-0'
            case('logical path collisions rejected', ['collision','collision-0'], collision)

            def invalid():
                before = calls.read_bytes()
                for count in [0, 257]: assert deploy('invalid-count', count)['kind'] == 'failed'
                for name in ['', '..', 'bad/name']: assert deploy(name)['kind'] == 'failed'
                assert calls.read_bytes() == before
            case('invalid policy rejected before factory effects', ['invalid-count','','..','bad/name'], invalid)

            def native():
                assert deploy('native-live', 2)['kind'] == 'deployed'
                assert client.read('/user/native-live-0')['path'] == '/user/native-live-0'
                assert client.admin('stop', path_prefix='/user/native-live-0')['kind'] == 'stopped'
                assert client.read('/user/native-live-1')['path'] == '/user/native-live-1'
                reply = client.admin('drain', path_prefix='/user/native-live-1', timeout_ms=5000)
                assert (reply['drained'], reply['aborted']) == (1,0)
            case('native Ray handles route and retire independently', ['native-live'], native)

            def native_tell():
                assert deploy('native-tell', 2)['kind'] == 'deployed'
                client.send(wire.FT_TELL, '/user/native-tell-0', 'bin:contract/Add', b'')
                assert client.read('/user/native-tell-0')['count'] == 10
                assert client.read('/user/native-tell-1')['count'] == 0
            case('native TELL uses deliver and exact path', ['native-tell'], native_tell)

            def native_failure():
                assert deploy('native-initfail')['kind'] == 'failed'
                client.missing('/user/native-initfail')
            case('native constructor failure is not Deployed', ['native-initfail'], native_failure)

            def rollback():
                assert deploy('rollback', 2)['kind'] == 'failed'
                client.missing('/user/rollback-0')
                assert client.admin('status', path_prefix='/user/rollback')['kind'] == 'failed'
            case('failed dispatcher pool leaves no route', ['rollback'], rollback)

            def native_rollback():
                assert deploy('audit')['kind'] == 'deployed'
                assert deploy('native-rollback', 2)['kind'] == 'failed'
                deadline = time.monotonic() + 5
                while True:
                    result = client.ask('/user/audit', 'Alive', b'native-rollback-0')
                    assert result.ft == wire.FT_REPLY
                    if json.loads(result.payload) is False: break
                    assert time.monotonic() < deadline, 'prefix native actor survived failed pool deployment'
                    time.sleep(0.01)
                client.missing('/user/native-rollback-0')
            case('failed native pool retires created prefix', ['audit','native-rollback'], native_rollback)
        report['status'] = 'failed' if report['failed'] else 'passed'
        assert not report['failed'], report['failed']
    finally:
        (root/'result.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n')


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda signum, frame: (_ for _ in ()).throw(KeyboardInterrupt(signum)))
    run()
