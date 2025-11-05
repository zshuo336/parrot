#!/usr/bin/env python3
"""Strict business payloads and redeploy state in an owned, actual Ray worker."""
from contextlib import ExitStack
from pathlib import Path
import json
import os
import re
import signal
import socket
import struct
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'interop/python'))
sys.path.insert(0, str(ROOT / 'apps/websearch/tests'))
from parrot_protocol import admin_v2, wire
from run_integration import Suite, require, until


def run():
    root = Path(tempfile.mkdtemp(prefix='parrot-python-contract-', dir='/tmp'))
    print(f'PYTHON_COMPONENT_ARTIFACTS={root}', flush=True)
    report = {'passed': []}
    try:
        with ExitStack() as stack:
            suite = Suite(root, ROOT / 'target/debug/websearch', stack)
            env = dict(suite.env, RAY_USAGE_STATS_ENABLED='0')
            for name in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'):
                env[name] = suite.runtime_proxy.base
            ray = suite.process('ray', [sys.executable, ROOT / 'apps/websearch/tests/ray_gateway.py', '0', root / 'ray'], env)
            port = int(until(lambda: re.search(r'RAY_GW_PORT=(\d+)', ray.text()), [ray]).group(1))
            with socket.create_connection(('127.0.0.1', port), timeout=20) as sock:
                decoder = wire.FrameDecoder()
                sequence = 0
                def exchange(kind, key='', payload=b'', path=''):
                    nonlocal sequence
                    sequence += 1
                    sock.sendall(wire.build_frame(kind, sequence, path, key, payload))
                    while True:
                        frame = decoder.next_frame()
                        if frame is not None:
                            require(frame.cid == sequence, 'wrong reply correlation')
                            return frame
                        chunk = sock.recv(65536)
                        require(chunk, 'gateway closed before reply')
                        decoder.feed(chunk)
                require(exchange(wire.FT_HANDSHAKE, '__handshake__', wire.handshake_body('python-components')).ft == wire.FT_HANDSHAKE_ACK, 'handshake failed')
                def admin(kind, **fields):
                    request = sequence + 1
                    frame = exchange(wire.FT_SYSTEM_EVENT, payload=admin_v2.encode_admin_cmd_v2({'kind':kind, 'req_id':request, **fields}))
                    require(frame.ft == wire.FT_SYSTEM_EVENT, 'expected admin reply')
                    reply = admin_v2.decode_admin_reply_v2(frame.payload)
                    require(reply['req_id'] == request, 'wrong admin request id')
                    return reply
                def ask(key, payload=b''):
                    return exchange(wire.FT_ASK, key, wire.with_reply_to_prefix('', payload), path='/user/' + name)
                for app, module, prefix, operation in [
                    ('crawler-lab', 'index_builder', 'crawl', 'IndexPage'),
                    ('websearch', 'tokenizer', 'ws', 'Tokenize'),
                ]:
                    name = prefix + '-contract'
                    component = {'name':name, 'version':'1', 'artifact':{'kind':'pymodule', 'module':module, 'uri':(ROOT / 'apps' / app / 'python').as_uri()}, 'instances':{'kind':'singleton'}, 'config':None}
                    def stats():
                        response = ask(f'bin:{prefix}/IndexStats')
                        require(response.ft == wire.FT_REPLY, 'stats did not reply')
                        require(response.type_key == f'bin:{prefix}/IndexStatsR', 'wrong stats type')
                        require(len(response.payload) == 12, 'wrong stats width')
                        return struct.unpack('<IQ', response.payload)
                    require(admin('deploy', component=component)['kind'] == 'deployed', 'component deploy failed')
                    require(stats() == (0, 0), 'new component inherited state')
                    valid = struct.pack('<IQI', 1, 17, 5) + b'alpha'
                    for malformed in (valid[:-1], valid+b'\0', struct.pack('<I', 0)+valid[4:]):
                        response = ask(f'bin:{prefix}/{operation}', malformed)
                        require(response.ft == wire.FT_REPLY_ERR, 'malformed business batch was acknowledged')
                        require(stats() == (0, 0), 'malformed batch partially indexed')
                    require(ask(f'bin:{prefix}/IndexStats', b'\0').ft == wire.FT_REPLY_ERR, 'bad stats request accepted')
                    response = ask(f'bin:{prefix}/{operation}', valid)
                    require(response.ft == wire.FT_REPLY, 'valid batch failed after malformed requests')
                    require(stats() == (1, 1), 'exact legal business effect missing')
                    if prefix == 'ws':
                        replacement = struct.pack('<IQI', 1, 17, 4) + b'beta'
                        expected = struct.pack('<II', 1, 4) + b'beta' + struct.pack('<QI', 17, 1)
                        for _ in range(2):
                            response = ask('bin:ws/Tokenize', replacement)
                            require(response.ft == wire.FT_REPLY and response.payload == expected, 'wrong replacement terms')
                            require(stats() == (1, 1), 'replay retained obsolete terms')
                        duplicate = struct.pack('<I', 2) + replacement[4:] + replacement[4:]
                        require(ask('bin:ws/Tokenize', duplicate).ft == wire.FT_REPLY_ERR, 'duplicate documents accepted')
                        require(stats() == (1, 1), 'failed replay changed statistics')
                        require(ask('bin:ws/Tokenize', struct.pack('<IQI', 1, 17, 0)).ft == wire.FT_REPLY, 'empty replacement failed')
                        require(stats() == (0, 0), 'empty document retained old postings')
                        report['websearch_replay'] = dict(repetitions=2, duplicate_rejected=True, empty_stats=[0, 0])
                    require(admin('stop', path_prefix='/user/'+name)['kind'] == 'stopped', 'component stop failed')
                    require(admin('deploy', component=component)['kind'] == 'deployed', 'component redeploy failed')
                    require(stats() == (0, 0), 'redeployment reused retired index')
                    require(admin('stop', path_prefix='/user/'+name)['kind'] == 'stopped', 'final stop failed')
                    report['passed'].append(app)
                    print(f'PYTHON_COMPONENT_OK {app}: strict payloads, no partial mutation, same-connection recovery, fresh redeploy', flush=True)
            require(not suite.corpus.blocked, 'unexpected crawler traffic')
        report['status'] = 'passed'
    except BaseException as error:
        report['status'] = 'failed'
        report['error'] = str(error)
        raise
    finally:
        (root / 'result.json').write_text(json.dumps(report, indent=2)+'\n')


if __name__ == '__main__':
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f'signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    run()
