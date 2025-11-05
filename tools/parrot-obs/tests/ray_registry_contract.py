"""Actual Ray worker deployment survives client retirement and stops by receipt."""
from contextlib import contextmanager
from pathlib import Path
import socket
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'interop/python'))
from parrot_protocol import admin_v2, wire


@contextmanager
def client(port):
    with socket.create_connection(('127.0.0.1', port), timeout=20) as sock:
        decoder = wire.FrameDecoder()
        sequence = 0

        def exchange(kind, path='', key='', payload=b''):
            nonlocal sequence
            sequence += 1
            sock.sendall(wire.build_frame(kind, sequence, path, key, payload))
            while True:
                frame = decoder.next_frame()
                if frame is not None:
                    assert frame.cid == sequence, (frame.cid, sequence)
                    return frame
                chunk = sock.recv(65536)
                assert chunk, 'peer closed before reply'
                decoder.feed(chunk)

        assert exchange(wire.FT_HANDSHAKE, key='__handshake__',
                        payload=wire.handshake_body('registry-contract')).ft == wire.FT_HANDSHAKE_ACK

        def admin(kind, **fields):
            request = sequence + 1
            frame = exchange(wire.FT_SYSTEM_EVENT, path='admin', payload=
                admin_v2.encode_admin_cmd_v2({'kind':kind, 'req_id':request, **fields}))
            assert frame.ft == wire.FT_SYSTEM_EVENT
            reply = admin_v2.decode_admin_reply_v2(frame.payload)
            assert reply['req_id'] == request
            return reply

        yield admin, exchange


def check_ray_registry(port):
    with tempfile.TemporaryDirectory(prefix='parrot-registry-component-') as temporary:
        Path(temporary, 'registry_contract_component.py').write_text('''from parrot_protocol.ray_gw import ParrotDispatcher

def parrot_entry(ctx):
    dispatcher = ParrotDispatcher()
    @dispatcher.handler("bin:registry:Echo")
    def echo(key, payload):
        return key, payload + b"-worker"
    return dispatcher
''')
        component = {'name':'registry-contract', 'version':'1', 'artifact': {
            'kind':'pymodule', 'module':'registry_contract_component',
            'uri':Path(temporary).as_uri()}, 'instances':{'kind':'singleton'}, 'config':None}
        with client(port) as (admin, _):
            assert admin('deploy', component=component)['kind'] == 'deployed'
            measured = admin('measured_metrics')['metrics']
            epoch = measured['epoch']
            assert epoch and measured['process_kind'] == 'python_threads'
            assert measured['snapshot']['components'] == 1
        with client(port) as (admin, exchange):
            assert admin('status', path_prefix='/user/registry-contract')['states'] == [
                {'path':'/user/registry-contract', 'state':'running', 'version':'1'}]
            measured = admin('measured_metrics')['metrics']
            assert measured['epoch'] == epoch
            deadline = time.monotonic() + 5
            while measured['snapshot']['connections'] != 1 and time.monotonic() < deadline:
                time.sleep(0.01)
                measured = admin('measured_metrics')['metrics']
            assert measured['snapshot']['connections'] == 1
            if sys.platform == 'linux':
                assert measured['available'] & (1 << 15), 'Linux RSS must be actually measured'
            else:
                assert not measured['available'] & (1 << 15)
            sample = admin('metrics')['snapshot']
            assert sample['components'] == 1 and len(sample['component_states']) == 1
            frame = exchange(wire.FT_ASK, path='/user/registry-contract', key='bin:registry:Echo', payload=wire.with_reply_to_prefix('/test/replies', b'alive'))
            assert frame.ft == wire.FT_REPLY and frame.payload == b'alive-worker' and frame.path == '/test/replies'
            assert admin('deploy', component=component)['kind'] == 'failed'
            assert admin('stop', path_prefix='/user/registry-contract')['kind'] == 'stopped'
            # The stop receipt must precede this request after remote unmount.
            frame = exchange(wire.FT_ASK, path='/user/registry-contract', key='bin:registry:Echo', payload=wire.with_reply_to_prefix('/test/replies', b'stopped'))
            assert frame.ft == wire.FT_REPLY_ERR and frame.path == '/test/replies'
            assert wire.decode_err_payload(frame.payload)[0] == wire.ERR_ACTOR_NOT_FOUND
            assert admin('metrics')['snapshot']['components'] == 0
        with client(port) as (admin, _):
            assert admin('status', path_prefix='/user/registry-contract')['code'] == admin_v2.ERR_COMPONENT_NOT_FOUND
            assert admin('deploy', component=component)['kind'] == 'deployed'
            assert admin('drain', path_prefix='/user/registry-contract', timeout_ms=5000) == {
                'kind':'drained', 'req_id':4, 'drained':1, 'aborted':0}
        print('RAY_REGISTRY_OK: actual worker, reconnect, metrics, business ASK, duplicate rejection, stop receipt and redeploy/drain', flush=True)
