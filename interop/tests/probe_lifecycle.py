#!/usr/bin/env python3
"""Actual CLI failure/cancellation contracts. No Ray/Erlang install needed.

The real guardian runs unchanged; only its business entrypoint is a controlled
fixture. All failure logs are retained when PARROT_PROBE_EVIDENCE is supplied.
"""
from pathlib import Path
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'apps/test_support'))
from owned_process import OwnedProcess, process_exited

FIXTURE = r"""import os,sys,json,socket,subprocess,time
from pathlib import Path
listener=socket.socket();listener.bind(('127.0.0.1',0));listener.listen()
child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)'],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
port=listener.getsockname()[1]
record=Path(os.environ['PROBE_PID_FILE']);temporary=record.with_suffix('.tmp');temporary.write_text(json.dumps(dict(pid=os.getpid(),child=child.pid,group=os.getpgrp(),port=port,ray_address=os.environ.get('RAY_ADDRESS'),auth_token_present='RAY_AUTH_TOKEN' in os.environ,auth_mode=os.environ.get('RAY_AUTH_MODE'),token_path=os.environ.get('RAY_AUTH_TOKEN_PATH'),node=os.environ.get('PARROT_NODE_ID'),cwd=os.getcwd())));temporary.replace(record)
mode=os.environ['PROBE_MODE'];marker=os.environ['PROBE_MARKER']
if mode=='invalid':print(marker+'invalid',flush=True)
elif mode=='zero':print(marker+'0',flush=True)
elif mode=='eof':sys.exit(23)
elif mode=='oversized':print('x'*300000,flush=True)
elif mode in ('business','flood','interrupt-business','success','wrong-ping','wrong-add','wrong-type'):
 print(marker+str(port),flush=True)
 if mode=='flood':
  for i in range(4000):print('x'*1024,flush=True)
  Path(os.environ['PROBE_DRAINED']).write_text('drained')
if mode in ('success','wrong-ping','wrong-add','wrong-type'):
 import struct
 sys.path.insert(0,os.environ['PROBE_WIRE_ROOT'])
 from parrot_protocol.wire import FrameDecoder,build_frame,decode_handshake,handshake_ack_body,split_reply_to,FT_HANDSHAKE,FT_HANDSHAKE_ACK,FT_HEARTBEAT,FT_HEARTBEAT_ACK,FT_ASK,FT_REPLY
 peer,_=listener.accept();decoder=FrameDecoder();requests=[]
 reason='eof'
 try:
  while True:
   chunk=peer.recv(65536)
   if not chunk:break
   decoder.feed(chunk)
   while (frame:=decoder.next_frame()) is not None:
    if frame.ft==FT_HANDSHAKE:
     hello=decode_handshake(frame.payload)
     peer.sendall(build_frame(FT_HANDSHAKE_ACK,frame.cid,'','__handshake__',handshake_ack_body(os.environ['PARROT_NODE_ID'],chosen_codec=hello['chosen_codec'])))
    elif frame.ft==FT_HEARTBEAT:peer.sendall(build_frame(FT_HEARTBEAT_ACK,frame.cid,'','',b''))
    elif frame.ft==FT_ASK:
     reply_to,payload=split_reply_to(frame.payload)
     requests.append(dict(key=frame.type_key,payload=payload.hex()))
     Path(os.environ['PROBE_REQUESTS']).write_text(json.dumps(requests))
     ray=os.environ['PARROT_NODE_ID']=='ray-gw-1'
     if frame.type_key=='bin:u:Ping':
      value=0 if mode=='wrong-ping' else (102 if ray else 103)
      key='bin:u:AddR' if mode=='wrong-type' else 'bin:u:Pong'
     elif frame.type_key=='bin:u:Add':
      value=0 if mode=='wrong-add' else (1007 if ray else 10007);key='bin:u:AddR'
     else:raise AssertionError('unexpected request type')
     peer.sendall(build_frame(FT_REPLY,frame.cid,reply_to,key,struct.pack('<Q',value)))
 except (ConnectionResetError,BrokenPipeError):
  reason='reset'
 finally:
  peer.close()
 if os.environ.get('PROBE_PEER_CLOSED'):
  Path(os.environ['PROBE_PEER_CLOSED']).write_text(json.dumps(dict(reason=reason,requests=len(requests))))
while True:time.sleep(.02)
"""


def until(predicate, seconds=8):
    deadline=time.monotonic()+seconds
    while not predicate():
        if time.monotonic()>=deadline:raise AssertionError('condition did not become true within deadline')
        time.sleep(.01)


def closed(port):
    with socket.socket() as peer:
        peer.settimeout(.1)
        return peer.connect_ex(('127.0.0.1',port))!=0


def run(out):
    from test_probe_peer_retirement import run_contracts
    run_contracts(out / 'peer-contracts')
    root=out/'fixture-root';(root/'tools/parrot-obs').mkdir(parents=True)
    (root/'interop/erlang').mkdir(parents=True);(root/'interop/erlang/parrot_gw.es').write_text('fixture')
    shutil.copy2(ROOT/'tools/parrot-obs/gateway_worker.py',root/'tools/parrot-obs/gateway_worker.py')
    (root/'tools/parrot-obs/ray_gateway.py').write_text(FIXTURE)
    tools=out/'bin';tools.mkdir()
    escript=tools/'escript';escript.write_text('#!'+sys.executable+'\n'+FIXTURE);escript.chmod(0o755)
    rows=[]
    argument_rows=[]
    with OwnedProcess([sys.executable,'-c','import time;time.sleep(180)']) as sentinel:
        for name,marker in [('ray','RAY_GW_PORT='),('erl','PARROT_ERL_PORT=')]:
            if os.environ.get('PARROT_PROBE_LANGUAGE') and name!=os.environ['PARROT_PROBE_LANGUAGE']:continue
            binary=Path(os.environ['PARROT_TEST_'+name.upper()+'_BINARY']).resolve()
            if not os.environ.get('PARROT_PROBE_CASE'):
                bad_args=[['-1'],['65536'],['1','2'],['--startup-timeout-ms','0'],['--startup-timeout-ms'],['--business-timeout-ms','600001'],['--business-timeout-ms','not-a-number'],['--startup-timeout-ms','1','--startup-timeout-ms','2'],['--unknown']]
                for index,args in enumerate(bad_args):
                    result=subprocess.run([str(binary),*args],cwd=out,env=dict(os.environ,PARROT_SOURCE_ROOT=str(out/'missing-source')),capture_output=True,text=True,timeout=5)
                    assert result.returncode==2,(name,args,result.returncode)
                    assert 'gateway logs:' not in result.stderr and ' PASS' not in result.stdout
                    assert 'source root:' not in result.stderr,'argument validation occurred after accessing gateway resources'
                    argument_rows.append(dict(name=name,args=args,exit=result.returncode,stderr=result.stderr))
                (out/'argument-results.json').write_text(json.dumps(argument_rows,indent=2)+'\n')
            for mode in ['silent','invalid','zero','eof','oversized','business','flood','interrupt','interrupt-business','kill-launcher','success','wrong-ping','wrong-add','wrong-type']:
                if os.environ.get('PARROT_PROBE_CASE') and mode!=os.environ['PARROT_PROBE_CASE']:continue
                label=name+'-'+mode;record=out/(label+'-pids.json');drained=out/(label+'-drained')
                env=dict(os.environ,PATH=str(tools)+os.pathsep+os.environ['PATH'],PARROT_SOURCE_ROOT=str(root),PROBE_PID_FILE=str(record),PROBE_DRAINED=str(drained),PROBE_REQUESTS=str(out/(label+'-requests.json')),PROBE_WIRE_ROOT=str(ROOT/'interop/python'),PROBE_MODE=mode,PROBE_MARKER=marker,RAY_ADDRESS='ray://external.invalid:10001',RAY_AUTH_TOKEN='ambient-not-for-this-cluster',PARROT_NODE_ID='ambient-wrong')
                path=out/(label+'.log');started=time.monotonic()
                with path.open('wb') as log,OwnedProcess([str(binary),'--startup-timeout-ms','1500','--business-timeout-ms','2000'],cwd=out,env=env,stdout=log,stderr=log) as process:
                    until(record.exists)
                    state=json.loads(record.read_text())
                    if mode=='flood':until(drained.exists)
                    if mode in ['interrupt','interrupt-business','flood']:os.kill(process.pid,signal.SIGTERM)
                    if mode=='kill-launcher':os.kill(process.pid,signal.SIGKILL)
                    code=process.wait(timeout=15)
                    assert code==(-signal.SIGKILL if mode=='kill-launcher' else (0 if mode=='success' else 2)),(label,code,path.read_text())
                    if mode=='kill-launcher':
                        until(lambda:all(process_exited(pid) for pid in [state['pid'],state['child']]))
                    else:
                        assert all(process_exited(pid) for pid in [state['pid'],state['child']]),(label,'returned before descendants retired')
                    assert closed(state['port']),(label,'listener retained')
                    assert sentinel.poll() is None
                text=path.read_text();assert (' PASS' in text)==(mode=='success')
                assert state['ray_address']=='local' and not state['auth_token_present']
                assert state['node']==name+'-gw-1'
                if mode!='kill-launcher':
                    assert not Path(state['token_path']).exists(),'credential remains after confirmed cleanup'
                if mode=='silent':assert 'startup deadline' in text
                if mode=='oversized':assert 'startup output exceeds limit' in text
                if mode=='business':assert 'business deadline' in text
                if mode in ['invalid','zero']:assert 'invalid readiness port' in text
                if mode.startswith('interrupt'):assert 'interrupted' in text
                if mode in ['success','wrong-ping','wrong-add','wrong-type']:
                    import struct
                    requests=json.loads((out/(label+'-requests.json')).read_text())
                    expected=[dict(key='bin:u:Ping',payload=struct.pack('<Q',100).hex())]
                    if mode in ['success','wrong-add']:expected.append(dict(key='bin:u:Add',payload=struct.pack('<QQ',3,4).hex()))
                    assert requests==expected,(label,requests)
                if mode=='wrong-ping':assert 'Ping(100): expected' in text
                if mode=='wrong-add':assert 'Add(3,4): expected' in text
                if mode=='wrong-type':assert 'unexpected Ping response type' in text
                rows.append(dict(name=name,mode=mode,exit=code,seconds=time.monotonic()-started,state=state,port_retired=True,descendants_retired=True,sentinel_alive=True))
                (out/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
        assert sentinel.poll() is None
    languages=1 if os.environ.get('PARROT_PROBE_LANGUAGE') else 2
    assert len(rows)==languages*(1 if os.environ.get('PARROT_PROBE_CASE') else 14)
    assert len(argument_rows)==(0 if os.environ.get('PARROT_PROBE_CASE') else languages*9)
    print(str(len(rows))+' real-process lifecycle contracts and '+str(len(argument_rows))+' argument contracts passed',flush=True)


if __name__=='__main__':
    if os.environ.get('PARROT_PROBE_EVIDENCE'):
        out=Path(os.environ['PARROT_PROBE_EVIDENCE']);out.mkdir(exist_ok=False,parents=True);run(out)
    else:
        with tempfile.TemporaryDirectory(prefix='parrot-probe-contract-') as directory:run(Path(directory))
