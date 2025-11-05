#!/usr/bin/env python3
"""Real per-deployment environment variables, offline pip and child inheritance."""
from contextlib import ExitStack
from pathlib import Path
import base64
import hashlib
import importlib.util
import json
import os
import re
import signal
import sys
import tempfile
import zipfile

from run_ray_lifecycle import Client, ROOT, Suite, until, wire

SOURCE = '''from parrot_protocol.ray_gw import ParrotDispatcher
from pathlib import Path
import json
import os
import sys
import ray

def read_value(env, payload):
    version = None
    if payload == b'package':
        import parrot_env_fixture
        version = parrot_env_fixture.VERSION
    return dict(value=os.environ.get('PARROT_ENV_CASE'), package=version,
                prefix=sys.prefix, env=env, root=str(Path(__file__).parent))

class Native:
    def __init__(self, env): self.env=env
    def ask(self,key,payload): return key,json.dumps(read_value(self.env,payload)).encode()

def parrot_entry(ctx):
    with Path(ctx['config'].decode()).open('a') as out: out.write(ctx['path']+'\\n')
    if ctx['path'].startswith('/user/native'):
        return ray.remote(Native).remote(ctx['env'])
    d=ParrotDispatcher()
    @d.handler('bin:contract/Read')
    def read(key,payload): return key,json.dumps(read_value(ctx['env'],payload)).encode()
    return d
'''


def wheel(directory, version):
    name = 'parrot_env_fixture'
    metadata = f'{name}-{version}.dist-info'
    files = {
        name+'/__init__.py': ('VERSION = '+repr(version)+'\n').encode(),
        metadata+'/METADATA': f'Metadata-Version: 2.1\nName: parrot-env-fixture\nVersion: {version}\n'.encode(),
        metadata+'/WHEEL': b'Wheel-Version: 1.0\nGenerator: parrot-contract\nRoot-Is-Purelib: true\nTag: py3-none-any\n',
    }
    rows=[]
    for path,data in files.items():
        digest=base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b'=').decode()
        rows.append(f'{path},sha256={digest},{len(data)}')
    files[metadata+'/RECORD']=('\n'.join(rows+[metadata+'/RECORD,,'])+'\n').encode()
    target=directory/f'{name}-{version}-py3-none-any.whl'
    with zipfile.ZipFile(target,'w') as archive:
        for path,data in files.items(): archive.writestr(path,data)
    return target


def run():
    assert importlib.util.find_spec('parrot_env_fixture') is None, 'fixture package must not exist in runner environment'
    root=Path(tempfile.mkdtemp(prefix='parrot-ray-env-',dir='/tmp'))
    print(f'RAY_ENV_RESULTS={root}',flush=True)
    report=dict(passed=[],failed=[])
    source=root/'source';source.mkdir();(source/'environment_component.py').write_text(SOURCE)
    calls=root/'factory.calls'; calls.write_text('')
    wheels=[wheel(root,'1.0'),wheel(root,'2.0')]
    try:
        with ExitStack() as stack:
            suite=Suite(root,ROOT/'target/debug/websearch',stack)
            env=dict(suite.env,RAY_USAGE_STATS_ENABLED='0',VIRTUALENV_NO_PERIODIC_UPDATE='1',
                     PIP_NO_INDEX='1',PIP_DISABLE_PIP_VERSION_CHECK='1')
            env.pop('PARROT_ENV_CASE',None)
            for key in ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy'):
                env[key]=suite.runtime_proxy.base
            process=suite.process('ray',[sys.executable,ROOT/'apps/websearch/tests/ray_gateway.py','0',root/'r'],env)
            port=int(until(lambda:re.search(r'RAY_GW_PORT=(\d+)',process.text()),[process]).group(1))
            client=Client(port);stack.callback(client.sock.close)
            client.sock.settimeout(120)
            active=set()

            def deploy(name,environment):
                reply=client.admin('deploy',component=dict(name=name,version=name,
                    artifact=dict(kind='pymodule',module='environment_component',uri=source.as_uri(),runtime_env=environment),
                    instances=dict(kind='singleton'),config=str(calls).encode()))
                active.add(name)
                return reply

            def value(name,package=False):
                reply=client.ask('/user/'+name,payload=b'package' if package else b'')
                assert reply.ft==wire.FT_REPLY,reply
                return json.loads(reply.payload)

            def case(name,action):
                try:
                    action()
                    for component in list(active):
                        assert client.admin('stop',path_prefix='/user/'+component)['kind']=='stopped'
                        active.remove(component)
                    report['passed'].append(name);print('RAY_ENV_OK '+name,flush=True)
                except Exception as error:
                    report['failed'].append(dict(name=name,error=str(error)));print('RAY_ENV_FAIL '+name+': '+str(error),flush=True)
                finally:
                    for component in list(active):
                        client.admin('stop',path_prefix='/user/'+component);active.remove(component)

            def variables():
                for name,label in [('first','A'),('second','B'),('native-vars','N')]:
                    toml='[env_vars]\nPARROT_ENV_CASE='+json.dumps(label)+'\n'
                    assert deploy(name,toml)['kind']=='deployed'
                assert value('first')['value']=='A'
                assert value('second')['value']=='B'
                native=value('native-vars');assert native['value']=='N'
                assert native['env']=={'env_vars':{'PARROT_ENV_CASE':'N'}},native
                assert deploy('baseline',None)['kind']=='deployed'
                assert value('baseline')['value'] is None
            case('variables isolated across hosts and inherited by native children',variables)

            def packages():
                for name,path in [('packages-old',wheels[0]),('native-packages-new',wheels[1])]:
                    config='pip=['+json.dumps(str(path))+']\n[env_vars]\nPIP_NO_INDEX="1"\nPIP_DISABLE_PIP_VERSION_CHECK="1"\n'
                    reply=deploy(name,config);assert reply['kind']=='deployed',reply
                old,new=value('packages-old',True),value('native-packages-new',True)
                assert old['package']=='1.0' and new['package']=='2.0',(old,new)
                assert old['prefix']!=new['prefix']
                assert value('packages-old',True)['package']=='1.0'
            case('offline wheels give different dependency versions to host and native child',packages)

            def setup_failure():
                before=calls.read_bytes()
                # Existing invalid wheel causes a deterministic local installer failure.
                broken=root/'parrot_env_fixture-3.0-py3-none-any.whl';broken.write_bytes(b'not a wheel')
                config='pip=['+json.dumps(str(broken))+']\n[env_vars]\nPIP_NO_INDEX="1"\n'
                for _ in range(2):
                    reply=deploy('recover',config);assert reply['kind']=='failed',reply
                    assert client.admin('status',path_prefix='/user/recover')['kind']=='failed', 'failed setup retained phantom registration'
                    active.remove('recover')
                assert calls.read_bytes()==before,'factory ran before its environment was ready'
                assert deploy('recover','[env_vars]\nPARROT_ENV_CASE="recovered"')['kind']=='deployed'
                assert value('recover')['value']=='recovered'
            case('failed environment setup has no factory effects and permits immediate name reuse',setup_failure)
        assert importlib.util.find_spec('parrot_env_fixture') is None
        report['status']='failed' if report['failed'] else 'passed'
        assert not report['failed'],report['failed']
    finally:
        (root/'result.json').write_text(json.dumps(report,indent=2)+'\n')


if __name__=='__main__':
    signal.signal(signal.SIGTERM,lambda signum,frame:(_ for _ in ()).throw(KeyboardInterrupt(signum)))
    run()
