#!/usr/bin/env python3
"""Real JVM probe command with controlled Wire peer and listening descendant.

This verifies the public process contract without requiring Java. Actual JVM
interoperability and the unmodified RTT gate are separate required validation.
"""
from pathlib import Path
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import shutil

ROOT=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(ROOT/'apps/test_support'))
from owned_process import OwnedProcess,process_exited
from probe_lifecycle import until,closed

BOUNDARIES=[0,41,126,127,128,249,250,251,65534,65535,65536,(1<<32)-2,(1<<32)-1,1<<32,(1<<63)-1,1<<63,(1<<64)-2]
MODES=['silent','invalid','eof','oversized','business-stall','terminate-startup','interrupt-startup','terminate-business','kill-launcher','flood','success','wrong-cpu','missing-overflow','wrong-recovery','wrong-echo','wrong-type','slow-rtt','raw-classpath']
CHILD=r"""import socket,sys,time,json,os
from pathlib import Path
s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
p=Path(sys.argv[1]);t=p.with_suffix('.tmp');t.write_text(json.dumps(dict(pid=os.getpid(),port=s.getsockname()[1])));t.replace(p)
while True:time.sleep(.1)
"""
FIXTURE=r"""import os,sys,json,subprocess,time,socket,struct
from pathlib import Path
child=subprocess.Popen([sys.executable,'-c',CHILD_CODE,os.environ['PROBE_CHILD_FILE']],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
while not Path(os.environ['PROBE_CHILD_FILE']).exists():time.sleep(.01)
p=Path(os.environ['PROBE_PID_FILE']);t=p.with_suffix('.tmp');t.write_text(json.dumps(dict(pid=os.getpid(),child=child.pid,port=s.getsockname()[1],argv_hex=[os.fsencode(v).hex() for v in sys.argv])));t.replace(p)
mode=os.environ['PROBE_MODE']
if mode=='invalid':print('PARROT_JVM_PORT=0',flush=True)
elif mode=='eof':sys.exit(23)
elif mode=='oversized':print('x'*70000,flush=True)
elif mode not in ('silent','terminate-startup','interrupt-startup','kill-launcher'):
 print('PARROT_JVM_PORT='+str(s.getsockname()[1]),flush=True)
 if mode=='flood':
  for i in range(4000):print('x'*1024,flush=True)
  Path(os.environ['PROBE_DRAINED']).write_text('drained')
if mode in ('success','wrong-cpu','missing-overflow','wrong-recovery','wrong-echo','wrong-type','slow-rtt','raw-classpath'):
 sys.path.insert(0,os.environ['PROBE_WIRE_ROOT'])
 from parrot_protocol.wire import FrameDecoder,build_frame,decode_handshake,handshake_ack_body,split_reply_to,encode_err_payload,FT_HANDSHAKE,FT_HANDSHAKE_ACK,FT_HEARTBEAT,FT_HEARTBEAT_ACK,FT_ASK,FT_REPLY,FT_REPLY_ERR,ERR_CODEC_ERROR
 def decode(b):
  if len(b)==1 and b[0]<=250:return b[0]
  for tag,width in [(251,2),(252,4),(253,8)]:
   if len(b)==width+1 and b[0]==tag:return int.from_bytes(b[1:],'little')
  raise AssertionError('noncanonical test request width')
 def encode(n):
  if n<=250:return bytes([n])
  for tag,width in [(251,2),(252,4),(253,8)]:
   if n<(1<<(8*width)):return bytes([tag])+n.to_bytes(width,'little')
  raise AssertionError('test reply exceeds u64')
 peer,_=s.accept();decoder=FrameDecoder();cpu=[];echo=[]
 def receipt():
  Path(os.environ['PROBE_REQUESTS']).write_text(json.dumps(dict(cpu=cpu,echo=echo)))
 while True:
  chunk=peer.recv(65536)
  if not chunk:break
  decoder.feed(chunk)
  while (frame:=decoder.next_frame()) is not None:
   if frame.ft==FT_HANDSHAKE:
    hello=decode_handshake(frame.payload)
    peer.sendall(build_frame(FT_HANDSHAKE_ACK,frame.cid,'','__handshake__',handshake_ack_body('jvm-gw-1',chosen_codec=hello['chosen_codec'])))
   elif frame.ft==FT_HEARTBEAT:peer.sendall(build_frame(FT_HEARTBEAT_ACK,frame.cid,'','',b''))
   elif frame.ft==FT_ASK:
    reply_to,payload=split_reply_to(frame.payload);n=decode(payload);assert encode(n)==payload
    if frame.type_key=='bin:parrot.interop.Cpu#v1':
     cpu.append(n);receipt()
     if n==(1<<64)-1 and mode!='missing-overflow':
      peer.sendall(build_frame(FT_REPLY_ERR,frame.cid,reply_to,'',encode_err_payload(ERR_CODEC_ERROR,'CPU overflow')));continue
     value=0 if mode=='wrong-cpu' or n==(1<<64)-1 or (mode=='wrong-recovery' and len(cpu)==19) else n+1
     key='bin:parrot.interop.Echoed#v1' if mode=='wrong-type' else 'bin:parrot.interop.CpuResult#v1'
    elif frame.type_key=='bin:parrot.interop.Echo#v1':
     echo.append(n);value=n+1 if mode=='wrong-echo' else n;key='bin:parrot.interop.Echoed#v1'
     if mode=='slow-rtt':time.sleep(.0005)
     if (mode=='wrong-echo' and len(echo)==1) or len(echo)==2500:receipt()
    else:raise AssertionError('unexpected request key')
    peer.sendall(build_frame(FT_REPLY,frame.cid,reply_to,key,encode(value)))
 peer.close()
while True:time.sleep(.05)
""".replace('CHILD_CODE',repr(CHILD))


def run(out):
    root=out/'root';(root/'tools/parrot-obs').mkdir(parents=True)
    shutil.copy2(ROOT/'tools/parrot-obs/gateway_worker.py',root/'tools/parrot-obs/gateway_worker.py')
    tools=out/'bin';tools.mkdir();java=tools/'java';java.write_text('#!'+sys.executable+'\n'+FIXTURE);java.chmod(0o755)
    binary=str(Path(os.environ['PARROT_TEST_MATRIX_BINARY']).resolve())
    rows=[]
    input_rows=[]
    if not os.environ.get('PARROT_MATRIX_CASE'):
        bad_args=[['-1'],['65536'],['1','2'],['--startup-timeout-ms','0'],['--startup-timeout-ms'],['--business-timeout-ms','600001'],['--business-timeout-ms','not-a-number'],['--startup-timeout-ms','1','--startup-timeout-ms','2'],['--unknown']]
        for args in bad_args:
            result=subprocess.run([binary,*args],env=dict(os.environ,PARROT_JVM_CP='fixture',PARROT_SOURCE_ROOT=str(out/'missing-root')),cwd=out,capture_output=True,text=True,timeout=5)
            assert result.returncode==1 and 'gateway logs:' not in result.stderr and 'source root:' not in result.stderr
            input_rows.append(dict(args=args,exit=result.returncode,stderr=result.stderr))
        fake_jar=out/'no-classpath.jar';fake_jar.write_bytes(b'fixture')
        for mode,override,expected in [('empty',{'PARROT_JVM_CP':''},'PARROT_JVM_CP is empty'),('missing-jar',{'PARROT_JVM_JAR':str(out/'missing.jar')},'JVM jar missing'),('missing-cp',{'PARROT_JVM_JAR':str(fake_jar)},'JVM dependency classpath')]:
            env=dict(os.environ,PARROT_SOURCE_ROOT=str(root));env.pop('PARROT_JVM_CP',None);env.pop('PARROT_JVM_JAR',None);env.update(override)
            result=subprocess.run([binary],env=env,cwd=out,capture_output=True,text=True,timeout=5)
            assert result.returncode==1 and expected in result.stderr and 'gateway logs:' not in result.stderr
            input_rows.append(dict(classpath=mode,exit=result.returncode,stderr=result.stderr))
        assert len(input_rows)==12
        (out/'input-results.json').write_text(json.dumps(input_rows,indent=2)+'\n')
    with OwnedProcess([sys.executable,'-c','import time;time.sleep(240)']) as sentinel:
        for mode in MODES:
            if os.environ.get('PARROT_MATRIX_CASE') and mode!=os.environ['PARROT_MATRIX_CASE']:continue
            record=out/(mode+'-pid.json');child=out/(mode+'-child.json');drained=out/(mode+'-drained');requests=out/(mode+'-requests.json')
            env=dict(os.environ,PATH=str(tools)+os.pathsep+os.environ['PATH'],PARROT_SOURCE_ROOT=str(root),PARROT_JVM_CP='fixture classpath with spaces',PROBE_PID_FILE=str(record),PROBE_CHILD_FILE=str(child),PROBE_DRAINED=str(drained),PROBE_REQUESTS=str(requests),PROBE_WIRE_ROOT=str(ROOT/'interop/python'),PROBE_MODE=mode)
            if mode=='raw-classpath':env['PARROT_JVM_CP']=b'fixture-\xff-classpath'
            path=out/(mode+'.log');start=time.monotonic()
            with path.open('wb') as log,OwnedProcess([binary,'--startup-timeout-ms','1500','--business-timeout-ms',('1000' if mode=='business-stall' else '6000')],env=env,cwd=out,stdout=log,stderr=log) as process:
                until(lambda:record.exists() and child.exists())
                state=json.loads(record.read_text());desc=json.loads(child.read_text())
                if mode=='terminate-business':until(lambda:'jvm gateway port:' in path.read_text())
                if mode=='flood':until(drained.exists)
                if mode in ['terminate-startup','terminate-business','flood']:os.kill(process.pid,signal.SIGTERM)
                if mode=='interrupt-startup':os.kill(process.pid,signal.SIGINT)
                if mode=='kill-launcher':os.kill(process.pid,signal.SIGKILL)
                code=process.wait(timeout=18)
                expected=-signal.SIGKILL if mode=='kill-launcher' else (0 if mode in ['success','raw-classpath'] else 1)
                assert code==expected,(mode,code,path.read_text())
                if mode=='kill-launcher':until(lambda:process_exited(state['pid']) and process_exited(desc['pid']))
                else:assert process_exited(state['pid']) and process_exited(desc['pid']),(mode,'returned with live descendant')
                assert closed(state['port']) and closed(desc['port'])
                assert sentinel.poll() is None
            text=path.read_text();assert ('interop-matrix PASS' in text)==(mode in ['success','raw-classpath'])
            if mode=='silent':assert 'startup deadline' in text
            if mode=='oversized':assert 'output exceeds limit' in text
            if mode=='business-stall':assert 'matrix deadline' in text
            if mode=='wrong-cpu':assert 'CPU value 0: expected 1' in text
            if mode=='missing-overflow':assert 'CPU overflow did not return CodecError' in text
            if mode=='wrong-recovery':assert 'CPU did not recover' in text
            if mode=='wrong-echo':assert 'echo payload mismatch' in text
            if mode=='wrong-type':assert 'unexpected CPU reply type' in text
            if mode=='slow-rtt':assert 'interop RTT gate FAILED' in text
            if mode in ['success','raw-classpath','slow-rtt']:
                values=json.loads(requests.read_text());assert values['cpu']==BOUNDARIES+[(1<<64)-1,41];assert values['echo']==list(range(2500))
            assert bytes.fromhex(state['argv_hex'][2])==(b'fixture-\xff-classpath' if mode=='raw-classpath' else b'fixture classpath with spaces')
            rows.append(dict(mode=mode,exit=code,seconds=time.monotonic()-start,descendants_retired=True,ports_retired=True,sentinel_alive=True))
            (out/'results.json').write_text(json.dumps(rows,indent=2)+'\n')
    assert len(rows)==(1 if os.environ.get('PARROT_MATRIX_CASE') else len(MODES))
    print(str(len(rows))+' JVM command process contracts and '+str(len(input_rows))+' input contracts passed',flush=True)


if __name__=='__main__':
    if os.environ.get('PARROT_MATRIX_EVIDENCE'):
        out=Path(os.environ['PARROT_MATRIX_EVIDENCE']);out.mkdir(exist_ok=False,parents=True);run(out)
    else:
        with tempfile.TemporaryDirectory(prefix='parrot-matrix-contract-') as directory:run(Path(directory))
