#!/usr/bin/env python3
"""Exercise the public daemon lifecycle against actual Erlang/JVM/Ray gateways."""
import concurrent.futures
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile

ROOT=Path(__file__).resolve().parents[3]
WRAPPER=ROOT/'tools/parrot-obs/gw.sh'
BINARY=ROOT/'target/debug/parrot-obs'


def run():
    with tempfile.TemporaryDirectory(prefix='pg-owned-real-',dir='/tmp') as temporary:
        env=dict(os.environ,GW_RUN_DIR=temporary,GW_ERL_PORT='0',GW_JVM_PORT='0',GW_RAY_PORT='0',GW_JVM_NODE='search-custom',PYTHON=sys.executable)
        def gateway(*args,environment=env,success=True):
            result=subprocess.run(['bash',str(WRAPPER),*args],cwd='/tmp',env=environment,text=True,capture_output=True,timeout=150)
            if (result.returncode==0)!=success:
                raise AssertionError(f'{args} exit={result.returncode}\n{result.stdout}\n{result.stderr}')
            return result
        try:
            first=gateway('up')
            line=next(line for line in first.stdout.splitlines() if line.startswith('NODES='))
            nodes=json.loads(line[len('NODES='):])
            assert 'jvm@search-custom=' in nodes
            second=gateway('up')
            assert line in second.stdout
            snapshot=json.loads(gateway('status').stdout)
            assert snapshot['phase']=='running' and len(snapshot['ports'])==3
            for name,marker in [('erl','PARROT_ERL_PORT='),('jvm','PARROT_JVM_PORT='),('ray','RAY_GW_PORT=')]:
                assert marker in gateway('log',name).stdout
            for args in [['status'],['ask','--rounds=5'],['load','--seconds=1','--conc=2']]:
                result=subprocess.run([str(BINARY),*args,nodes,'--insecure-development'],cwd='/tmp',text=True,capture_output=True,timeout=30)
                if result.returncode:raise AssertionError(result.stdout+'\n'+result.stderr)
                print(f'OWNED_GATEWAY_COMMAND={args!r} exit=0',flush=True)
                print(result.stdout,end='',flush=True)
            from gateway_metrics_contract import check_gateway_metrics
            check_gateway_metrics(snapshot['ports']['erl'], 'erlang_processes', 'ERLANG')
            from jvm_metrics_contract import check_jvm_metrics
            check_jvm_metrics(snapshot['ports']['jvm'])
            from ray_registry_contract import check_ray_registry
            check_ray_registry(snapshot['ports']['ray'])
            gateway('up',environment=dict(env,GW_JVM_NODE='different-node'),success=False)
            assert json.loads(gateway('status').stdout)['ports']==snapshot['ports']
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                results=list(pool.map(lambda _:gateway('down'),range(2)))
            assert all(json.loads(result.stdout)['phase']=='stopped' for result in results)
            assert json.loads(gateway('status').stdout)['phase']=='stopped'
            for port in snapshot['ports'].values():
                with socket.socket() as listener:
                    listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
                    listener.bind(('127.0.0.1',port))
            print('OWNED_GATEWAYS_OK: actual three gateways, custom identity, idempotent up, conflict, logs, concurrent down and released ports',flush=True)
        finally:
            gateway('down')


if __name__=='__main__':run()
