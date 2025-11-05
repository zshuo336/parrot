#!/usr/bin/env python3
"""Real standard assembly, declared routing, query phase, and process retirement.

Requires build-crawler-business, build-crawler-components, build-ts and Ray.
Every run samples actual process identities; no skipped runtime counts as pass.
"""
import argparse
import json
import os
import signal
import shlex
import shutil
from pathlib import Path
import sys
import tempfile
import time

from run_launcher_integration import ROOT, Run, require
sys.path.insert(0, str(ROOT / 'scripts'))
from crawler_business_artifacts import verify as verify_business_artifacts
sys.path.insert(0, str(ROOT / 'apps/crawler-lab/deploy'))
from regression import compare, GOLDEN


def verify_ray_allocator(output):
    # Run the installed Ray algorithm at its exact upper-port fallback boundary.
    # Only its initial socket and random choices are controlled; the repaired
    # fallback obtains a real kernel socket and must exclude both logical ports.
    from unittest.mock import patch
    import socket
    import ray
    from ray._private.node import Node
    from parrot_protocol.ray_startup import guarded_allocator
    real_socket = socket.socket
    class Initial:
        def bind(self, address): pass
        def getsockname(self): return '', 65534
        def close(self): pass
    with patch('ray._private.node.socket.socket', return_value=Initial()), patch('ray._private.node.random.randint', return_value=65534):
        before = Node._get_unused_port(object(), {65534, 65535})
    calls = []
    def create(*args):
        calls.append(1)
        return Initial() if len(calls) == 1 else real_socket(*args)
    with patch('ray._private.node.socket.socket', side_effect=create), patch('ray._private.node.random.randint', return_value=65534):
        after = guarded_allocator(Node._get_unused_port)(object(), {65534, 65535})
    require(after not in {65534, 65535}, 'allocator returned an already allocated internal port')
    (output / 'ray-port-proof.json').write_text(json.dumps(dict(ray_version=ray.__version__, original=before, guarded=after, excluded=[65534,65535]), indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    output = args.output or Path(tempfile.mkdtemp(prefix='crawler-manifest-', dir='/tmp'))
    if args.output:
        output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    print('CRAWLER_MANIFEST_ARTIFACTS=' + str(output), flush=True)
    sys.path.insert(0, str(ROOT / 'crates/parrot-app/tests'))
    from run_gateway_ownership import verify_port_probe
    verify_port_probe(output)
    sys.path.insert(0, str(ROOT / 'interop/python'))
    verify_ray_allocator(output)
    source = (ROOT / 'apps/crawler-lab/crawler.app.toml').read_text()
    # Moving the manifest is deliberate: absolute file URIs must work too.
    for relative in ['erlang/', 'python/', 'jvm/target/crawler-lab-jvm-1.0.0.jar']:
        source = source.replace('"' + relative + '"', '"' + (ROOT / 'apps/crawler-lab' / relative).as_uri() + '"')
    renamed = source
    for old, new in [('frontier', 'queue'), ('index', 'catalog'), ('search', 'finder'), ('crawler', 'driver')]:
        # Preserve artifact module/class/factory and source aliases. Only names,
        # dependency references, target paths and source owner change.
        renamed = renamed.replace('name = "' + old + '"', 'name = "' + new + '"')
        renamed = renamed.replace('"' + old + '"', '"' + new + '"') if old == 'crawler' else renamed
        renamed = renamed.replace('"' + old + ':/user/', '"' + new + ':/user/')
        renamed = renamed.replace('to = "' + new + ':/user/' + old + '"', 'to = "' + new + ':/user/' + new + '"')
    renamed = renamed.replace('["frontier"]', '["queue"]').replace('["index"]', '["catalog"]').replace('["frontier", "index", "search"]', '["queue", "catalog", "finder"]')
    renamed = renamed.replace('version = "2.0.0"', 'version = "7.8.9"').replace('"pages":60', '"pages":17').replace('"batch":32', '"batch":7')
    custom = output / 'renamed.app.toml'
    custom.write_text(renamed)
    broken = output / 'broken.app.toml'
    broken.write_text(source.replace('to = "search:/user/search"', 'to = "index:/user/index"'))
    # Structurally valid but semantically wrong: the wrong target must fail a
    # real request; rejecting just the TOML parser cannot make this case pass.
    artifacts = verify_business_artifacts()
    (output / 'artifacts.json').write_text(json.dumps(artifacts, indent=2) + '\n')
    cases = []
    for form in ['props', 'dylib', 'wasm']:
        flags = [] if form == 'props' else ['--hub-' + form, artifacts[form]['path'], '--trust-native-sha256' if form == 'dylib' else '--hub-sha256', artifacts[form]['sha256']]
        cases.append((form, dict(pages=None, hub_options=flags), True, form))
    cases.extend([
        ('renamed-config-version', dict(pages=None, hub_options=['--manifest', str(custom)]), False, 'props'),
        ('zero-explicit-override', dict(pages=0, hub_options=['--manifest', str(custom)]), False, 'props'),
        ('wrong-business-wire', dict(pages=None, hub_options=['--manifest', str(broken)], expected_code=1), False, None),
    ])
    for name, signum in [('query-timeout', None), ('app-term', signal.SIGTERM), ('app-kill', signal.SIGKILL), ('query-invalid-ack', None)]:
        directory = output / name
        directory.mkdir()
        command = [artifacts['application']['path'], '--insecure-development',
                   '--manifest', str(ROOT / 'apps/crawler-lab/crawler.app.toml'),
                   '--query-ready', str(directory / 'query-ready.json'),
                   '--query-done', str(directory / 'query-done.txt'), '--wait', '2']
        options = dict(pages=None, launch_command=command, interrupt=signum,
                       signal_marker='PARROT_CRAWLER_QUERY_READY=',
                       expected_code=-signal.SIGKILL if signum == signal.SIGKILL else 1)
        cases.append((name, options, False, None))
    shim = output / 'query-failure-bin'
    shim.mkdir()
    node = shutil.which('node')
    require(node is not None, 'Node is required')
    wrapper = shim / 'node'
    wrapper.write_text('#!/bin/sh\nif [ "$1" = "--test" ]; then\n  export LAB_EXPECTED_HITS=\'[]\'\nfi\nexec ' + shlex.quote(node) + ' "$@"\n')
    wrapper.chmod(0o700)
    cases.append(('ts-oracle-failure', dict(pages=None, expected_code=1,
                  extra_environment={'PATH': str(shim) + os.pathsep + os.environ['PATH']}), False, None))
    passed, failed = {}, []
    for name, options, golden, form in cases:
        run = None
        try:
            run = Run(output / name, mode='manifest', application=artifacts['application']['path'], **options)
            while not run.sample():
                if name == 'query-invalid-ack' and (run.directory / 'query-ready.json').exists() and not (run.directory / 'query-done.txt').exists():
                    temporary = run.directory / 'invalid.tmp'
                    temporary.write_text('no\n')
                    temporary.replace(run.directory / 'query-done.txt')
                time.sleep(.025)
            record = run.finish()
            text = run.stdout.read_text()
            if golden:
                compare(GOLDEN.read_text(), text)
            terminal = json.loads((run.directory / 'result.json').read_text()) if not run.raw_application else None
            if form:
                require('[lab] hub business: form=' + form in text, 'selected business did not execute')
                require(terminal['ts'] == 'passed', 'actual TS query missing')
                state = json.loads((run.directory / 'query-ready.json').read_text())
                require('[ts-user] search parrot+actor → ' + json.dumps(state['expected_hits'], separators=(',', ':')) in text, 'TS did not return independent oracle results')
            if name == 'renamed-config-version':
                for target in ['queue', 'catalog', 'finder']:
                    require('[lab] verified deployment ' + target + ' version=7.8.9' in text, 'gateway deployment version was not verified')
                require('version=7.8.9 components=["queue", "catalog", "finder", "driver"]' in text, 'manifest identity ignored')
                require('fetched=17' in text and 'ticks=3' in text, 'manifest settings ignored')
                require(state['path'] == '/user/finder', 'declared search target ignored')
            if name == 'zero-explicit-override':
                require('fetched=0' in text and state['expected_hits'] == [], 'explicit zero override ignored')
            if name == 'wrong-business-wire':
                require(not (run.directory / 'query-ready.json').exists(), 'invalid business reached query phase')
                require('JVM health' in (run.directory / 'app.log').read_text(), 'misroute did not reach real protocol failure')
            if name == 'ts-oracle-failure':
                query_output = (run.directory / 'typescript-query.log').read_text()
                require('independent term-frequency oracle' in query_output, 'real TS oracle assertion did not fail')
                require(not (run.directory / 'query-done.txt').exists(), 'failed TS query acknowledged success')
            if name.startswith('query-'):
                require((run.directory / 'query-ready.json').exists(), 'query phase was not reached')
                expected = 'external query completion timed out' if name == 'query-timeout' else 'external query failed'
                require(expected in text, 'expected query failure absent')
            require(record['listeners'], 'no actual nested listeners observed')
            passed[name] = record
            print('CRAWLER_MANIFEST_PASS ' + name, flush=True)
        except Exception as error:
            failed.append(dict(name=name, error=str(error)))
            print('CRAWLER_MANIFEST_FAIL ' + name + ': ' + str(error), flush=True)
        finally:
            if run is not None:
                if run.process.poll() is None:
                    run.process.kill()
                    run.process.wait(timeout=15)
                    try:
                        run.finish()
                    except Exception as error:
                        failed.append(dict(name=name + '-retirement', error=str(error)))
                run.stream.close()
    (output / 'result.json').write_text(json.dumps(dict(passed=passed, failed=failed), indent=2) + '\n')
    require(not failed and len(passed) == len(cases), 'manifest matrix failed: ' + str(output))


if __name__ == '__main__':
    main()
