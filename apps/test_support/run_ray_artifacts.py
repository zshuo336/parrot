#!/usr/bin/env python3
"""Actual Wire -> coordinator -> isolated hosts -> native actors/imports."""
from contextlib import ExitStack
from pathlib import Path
import json
import os
import re
import shutil
import signal
import sys
import tempfile

from run_ray_lifecycle import Client, ROOT, Suite, until, wire

SOURCE = '''from parrot_protocol.ray_gw import ParrotDispatcher
from pathlib import Path
import helper
from pkg import part
import importlib
import json
import os
import ray

def value():
    import lazy
    return dict(values=[helper.VALUE, part.VALUE, lazy.VALUE, importlib.import_module('dynamic').VALUE],
                resource=Path(__file__).with_name('data.txt').read_text(), pid=os.getpid(), root=str(Path(__file__).parent))

class Native:
    def ask(self, key, payload): return key, json.dumps(value()).encode()
    def parrot_drain(self): return True

def parrot_entry(ctx):
    if ctx['path'].startswith('/user/native'):
        return ray.remote(Native).remote()
    d = ParrotDispatcher()
    @d.handler('bin:contract/Read')
    def read(key, payload): return key, json.dumps(value()).encode()
    return d
'''


def artifact(root, value, suffix):
    root.mkdir(parents=True, exist_ok=True)
    # Distinct case names prevent one old-version cache failure contaminating
    # subsequent cases; each case still reuses names across its deployments.
    source = re.sub(r'\b(entry|helper|pkg|part|lazy|dynamic)\b', lambda m: m[0]+suffix, SOURCE)
    (root/('entry'+suffix+'.py')).write_text(source)
    package = root/('pkg'+suffix)
    package.mkdir(exist_ok=True)
    (package/'__init__.py').write_text('from . import part'+suffix+'\n')
    for path in [root/('helper'+suffix+'.py'),package/('part'+suffix+'.py'),
                 root/('lazy'+suffix+'.py'),root/('dynamic'+suffix+'.py')]:
        path.write_text('VALUE = ' + repr(value) + '\n')
    (root/'data.txt').write_text(value)
    return root


def run():
    root = Path(tempfile.mkdtemp(prefix='parrot-ray-artifacts-', dir='/tmp'))
    print(f'RAY_ARTIFACTS_RESULTS={root}', flush=True)
    report = dict(passed=[], failed=[])
    try:
        with ExitStack() as stack:
            suite = Suite(root, ROOT/'target/debug/websearch', stack)
            env = dict(suite.env, RAY_USAGE_STATS_ENABLED='0')
            for key in ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy'):
                env[key] = suite.runtime_proxy.base
            process = suite.process('ray', [sys.executable, ROOT/'apps/websearch/tests/ray_gateway.py','0',root/'r'], env)
            port = int(until(lambda: re.search(r'RAY_GW_PORT=(\d+)',process.text()),[process]).group(1))
            client = Client(port); stack.callback(client.sock.close)
            active = set()
            case_number = 0

            def make_artifact(root, value):
                return artifact(root,value,str(case_number))
            snapshots = {}

            def deploy(name, source):
                reply = client.admin('deploy', component=dict(name=name, version=name,
                    artifact=dict(kind='pymodule',module='entry'+str(case_number),uri=source.as_uri()),instances=dict(kind='singleton')))
                assert reply['kind'] == 'deployed', reply
                active.add(name)

            def check(name, value):
                result = client.read('/user/' + name)
                assert result['values'] == [value]*4 and result['resource'] == value, result
                snapshots[name] = Path(result['root'])
                return result

            def stop(name):
                reply = client.admin('stop', path_prefix='/user/' + name)
                assert reply['kind'] == 'stopped', reply
                active.remove(name)
                if name in snapshots: assert not snapshots[name].exists(), 'retired source snapshot leaked'

            def case(name, action):
                nonlocal case_number
                case_number += 1
                try:
                    action()
                    for component in list(active): stop(component)
                    report['passed'].append(name)
                    print('RAY_ARTIFACTS_OK ' + name, flush=True)
                except Exception as error:
                    report['failed'].append(dict(name=name,error=str(error)))
                    print('RAY_ARTIFACTS_FAIL ' + name + ': ' + str(error), flush=True)
                finally:
                    for component in list(active):
                        client.admin('stop',path_prefix='/user/' + component)
                        active.remove(component)

            def independent():
                a, b = make_artifact(root/'A','A'), make_artifact(root/'B','B')
                deploy('first',a); deploy('second',b)
                first, second = check('first','A'), check('second','B')
                assert first['pid'] != second['pid'] and first['root'] != second['root']
                check('first','A'); check('second','B')
            case('same names isolated in distinct host processes', independent)

            def replacement():
                source = make_artifact(root/'replace','A')
                deploy('old',source)
                stats = {p:p.stat() for p in source.rglob('*.py')}
                make_artifact(source,'B')
                for p, st in stats.items(): os.utime(p, ns=(st.st_atime_ns,st.st_mtime_ns))
                deploy('new',source)
                check('old','A'); check('new','B')
                stop('new'); make_artifact(source,'C'); deploy('new',source)
                check('new','C'); check('old','A')
            case('same source replacement preserves old lazy imports and refreshes redeploy', replacement)

            def deleted():
                source = make_artifact(root/'deleted','D')
                deploy('deleted',source); shutil.rmtree(source)
                check('deleted','D')
            case('deleted source still supports deferred imports and resources', deleted)

            def unicode():
                deploy('encoded',make_artifact(root/'源码 space # %','U')); check('encoded','U')
            case('percent encoded Unicode file URI', unicode)

            def native():
                source = make_artifact(root/'native','N')
                deploy('native-old',source)
                make_artifact(source,'M'); deploy('native-new',source)
                shutil.rmtree(source)
                old, new = check('native-old','N'), check('native-new','M')
                assert old['pid'] != new['pid'] and old['root'] != new['root']
                check('native-old','N')
            case('native child actors inherit isolated imports without pickle registration', native)
        report['status'] = 'failed' if report['failed'] else 'passed'
        assert not report['failed'], report['failed']
    finally:
        (root/'result.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')


if __name__ == '__main__':
    signal.signal(signal.SIGTERM, lambda signum, frame: (_ for _ in ()).throw(KeyboardInterrupt(signum)))
    run()
