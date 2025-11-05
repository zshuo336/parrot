#!/usr/bin/env python3
"""Run the real crawler application against owned gateways, including a corrupt reply.

Prerequisites: built crawler binary, generic JVM gateway and crawler JVM/Beam
artifacts. All ports are allocated by their listeners; logs are retained.
"""
from contextlib import ExitStack
import importlib.util
import json
import os
import re
import signal
import struct
import sys
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT/'apps/test_support'))
from reply_proxy import CorruptFirstReply as CorruptFirstBatch
spec = importlib.util.spec_from_file_location('crawler_owned_gateway_fixture', ROOT / 'tools/parrot-obs/tests/run_gateways.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def run_cli(command, env=None):
    child = subprocess.Popen(command, cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    try:
        stdout, stderr = child.communicate(timeout=120)
        return subprocess.CompletedProcess(command, child.returncode, stdout, stderr)
    finally:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
        child.stdout.close()
        child.stderr.close()



def run():
    root = Path(tempfile.mkdtemp(prefix='crawler-contract-', dir='/tmp'))
    print(f'CRAWLER_ARTIFACTS={root}', flush=True)
    cases, failures = [], []
    # A success response with the wrong shape/value/type must not count as a
    # committed application batch. The proxy touches only business REPLY frames.
    scenarios = [
        ('valid', None, None, None, None, True),
        ('valid_hostname', None, None, None, None, True),
        ('valid_zero', None, None, None, None, True),
        ('corrupt', 'erl', b'bin:crawl/FrontierBatch', struct.pack('<I', 1), None, False),
        ('frontier_ack_short', 'erl', b'bin:crawl/FrontierAck', b'', None, False),
        ('frontier_ack_false', 'erl', b'bin:crawl/FrontierAck', struct.pack('<I', 0), None, False),
        ('ray_ack_short', 'ray', b'bin:crawl/IndexAck', b'\x00', None, False),
        ('ray_ack_jvm_shape', 'ray', b'bin:crawl/IndexAck', struct.pack('<I', 0), None, False),
        ('ray_ack_wrong_count', 'ray', b'bin:crawl/IndexAck', struct.pack('<II', 1, 0), None, False),
        ('ray_ack_wrong_key', 'ray', b'bin:crawl/IndexAck', struct.pack('<I', 1), b'bin:crawl/FrontierAck', False),
        ('jvm_ack_ray_shape', 'jvm', b'bin:crawl/IndexAck', struct.pack('<II', 0, 0), None, False),
        ('jvm_ack_wrong_count', 'jvm', b'bin:crawl/IndexAck', struct.pack('<I', 0), None, False),
        ('stats_short', 'ray', b'bin:crawl/IndexStatsR', b'', None, False),
        ('health_invalid_json', 'jvm', b'bin:crawl/HealthzR', b'{broken', None, False),
        ('health_whitespace', 'jvm', b'bin:crawl/HealthzR', b'{ "queries" : 0, "postings" : 0, "terms" : 0 }', None, True),
        ('search_invalid_json', 'jvm', b'bin:crawl/SearchResult', b'not json', None, False),
        ('search_false_empty', 'jvm', b'bin:crawl/SearchResult', b'[]', None, False),
    ]
    try:
        for name, gateway, key, payload, replacement, succeeds in scenarios:
            try:
                run_case(root, name, gateway, key, payload, replacement, succeeds)
                cases.append(name)
            except Exception as error:
                failures.append(dict(name=name, error=str(error)))
                print(f'CRAWLER_CONTRACT_FAIL {name}: {error}', flush=True)
        try:
            run_registry_case(root)
            cases.append('valid_registry')
        except Exception as error:
            failures.append(dict(name='valid_registry', error=str(error)))
            print(f'CRAWLER_CONTRACT_FAIL valid_registry: {error}', flush=True)
    finally:
        (root/'result.json').write_text(json.dumps({'passed': cases, 'failed': failures}, indent=2)+'\n')
    require(not failures and len(cases) == len(scenarios) + 1, 'application reply contracts failed: '+str(failures))


def run_case(root, name, gateway, key, payload, replacement, succeeds):
    with ExitStack() as stack:
        directory = root / name
        directory.mkdir()
        environment = dict(os.environ, GW_ERL_PORT='0', GW_JVM_PORT='0', GW_RAY_PORT='0', GW_JVM_NODE='jvm-search-1')
        config = fixture.manager.config_from_environment(directory/'gateways', environment)
        gateways = stack.enter_context(fixture.OwnedGateways(config))
        ports = dict(gateways.snapshot['ports'])
        proxy = None
        if gateway is not None:
            proxy = CorruptFirstBatch(ports[gateway], key=key, payload=payload, replacement_key=replacement)
            stack.callback(proxy.close)
            ports[gateway] = proxy.port
        command = [str(ROOT/'target/debug/crawler-lab'), '--insecure-development',
            '--pages','0' if name == 'valid_zero' else '12','--depth','2','--fanout','2','--batch','4']
        host = 'localhost' if name == 'valid_hostname' else '127.0.0.1'
        command.extend(f'{kind}={host}:{port}' for kind, port in sorted(ports.items()))
        completed = run_cli(command)
        (directory/'stdout.log').write_text(completed.stdout)
        (directory/'stderr.log').write_text(completed.stderr)
        output = completed.stdout + completed.stderr
        if proxy is not None:
            require(proxy.corrupted == 1, 'did not intercept the expected real reply')
        require('panicked' not in output, 'gateway reply panicked the application: '+output[-2000:])
        if succeeds:
            require(completed.returncode == 0, output[-4000:])
            require('crawler-lab PASS' in output, output[-4000:])
        else:
            require(completed.returncode != 0, 'bad gateway reply still ended in CLI success')
            require('crawler-lab PASS' not in output, 'false PASS marker after bad reply')
            require('crawler-lab:' in output, output[-2000:])
        if name == 'valid':
            # The user-facing TS client must query the same live, populated JVM
            # component after the actual Rust crawl/index operation completes.
            ts = run_cli(['node', '--test', str(ROOT/'interop/typescript-lite/tests/integration/crawler_client.test.mjs')],
                         env=dict(os.environ, LAB_JVM=f'127.0.0.1:{ports["jvm"]}'))
            (directory/'typescript.log').write_text(ts.stdout + ts.stderr)
            require(ts.returncode == 0, 'TS user query failed: '+(ts.stdout+ts.stderr)[-4000:])
            require('[ts-user] search parrot+actor' in ts.stdout, 'TS query never completed')
            print('CRAWLER_TS_QUERY_OK', flush=True)
        print(f'CRAWLER_CONTRACT_OK {name} exit={completed.returncode}', flush=True)


def run_registry_case(root):
    # Reuse the existing owner of process groups, Ray runtime and runtime HTTP
    # probes; the app listener itself chooses port 0 and reports its bound port.
    sys.path.insert(0, str(ROOT/'apps/websearch/tests'))
    from run_integration import Suite, until
    directory = root/'r'  # Ray adds a long session/socket suffix; stay under AF_UNIX limits.
    directory.mkdir()
    with ExitStack() as stack:
        binary = ROOT/'target/debug/crawler-lab'
        suite = Suite(directory, binary, stack)
        app = suite.process('crawler-registry', [binary, '--insecure-development', '--bind', '127.0.0.1:0',
            '--wait', '30', '--pages', '12', '--depth', '2', '--fanout', '2', '--batch', '4'])
        match = until(lambda: re.search(r'应用监听 (127\.0\.0\.1:\d+)', app.text()), [app])
        gateways, _ = suite.gateways(directory/'data', target=match.group(1))
        until(lambda: app.child.poll() is not None, gateways, seconds=60)
        output = app.text()
        require(app.child.returncode == 0 and 'crawler-lab PASS' in output and 'panicked' not in output, output[-4000:])
        require('三网关已全部注册' in output, 'did not observe all registered gateway links')
        print('CRAWLER_CONTRACT_OK valid_registry exit=0', flush=True)


if __name__ == '__main__':
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f'signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    run()
