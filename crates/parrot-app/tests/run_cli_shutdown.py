#!/usr/bin/env python3
"""Actual generic CLI startup signals, independent JVM/listener retirement oracle.

Requires make build-jvm and Ray's psutil dependency. The Rust cases create and
signal real application hosts; this observer starts no replacement gateways.
"""
import argparse,json,os,signal,subprocess,tempfile,time
from pathlib import Path
from run_gateway_ownership import ROOT,psutil,require,remember_identity,bind_endpoint,probe_retired_listeners,verify_port_probe


def run(output):
    output.mkdir(parents=True,exist_ok=False)
    verify_port_probe(output)
    witnesses=output/"observations";witnesses.mkdir()
    command=['cargo','test','--locked','-p','parrot-app','--features','host','--test','application_wiring',
             'actual_cli_sig','--','--include-ignored','--nocapture']
    identities={};listeners=set();errors=[];monitor_error=None;timed_out=False
    started=time.monotonic()
    with (output/'business.log').open('w') as log:
        process=subprocess.Popen(command,cwd=ROOT,env=dict(os.environ,CARGO_BUILD_JOBS='2',PARROT_TEST_OBSERVATION_DIR=str(witnesses.resolve())),
                                 stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
        root=psutil.Process(process.pid);next_ports=0
        try:
            while process.poll() is None:
                ports=time.monotonic()>=next_ports
                if ports:next_ports=time.monotonic()+.2
                try:children=root.children(recursive=True)
                except psutil.NoSuchProcess:children=[]
                for child in children:
                    try:
                        created=child.create_time()
                        remember_identity(identities,child.pid,created,child.cmdline(),os.getpgid(child.pid))
                        if ports:
                            for connection in child.net_connections(kind='tcp'):
                                if connection.status==psutil.CONN_LISTEN:
                                    listeners.add((int(connection.family),connection.laddr.ip,connection.laddr.port))
                                    (witnesses/(str(child.pid)+'-'+str(connection.laddr.port)+'.json')).write_text(json.dumps(dict(pid=child.pid,created=created,port=connection.laddr.port))+'\n')
                    except (psutil.NoSuchProcess,ProcessLookupError):pass
                    except psutil.AccessDenied as error:errors.append(str(error))
                if time.monotonic()-started>180:
                    timed_out=True;break
                time.sleep(.025)
        except Exception as error:monitor_error=repr(error)
        finally:
            if process.poll() is None:
                os.killpg(process.pid,signal.SIGKILL)
        code=process.wait()
    deadline=time.monotonic()+20
    while True:
        live=[]
        for (pid,created),row in identities.items():
            try:
                child=psutil.Process(pid)
                if child.create_time()==created and child.status()!=psutil.STATUS_ZOMBIE:live.append(row)
            except psutil.NoSuchProcess:pass
        if not live or time.monotonic()>=deadline:break
        time.sleep(.05)
    port_errors,history=probe_retired_listeners({bind_endpoint(endpoint) for endpoint in listeners})
    text=(output/'business.log').read_text()
    cases=[json.loads(line.split('=',1)[1]) for line in text.splitlines() if line.startswith('PARROT_CLI_SHUTDOWN_CASE=')]
    jvms=[row for row in identities.values() if 'parrot.protocol.jvm.ParrotGatewayMain' in row['command']]
    result=dict(exit=code,timed_out=timed_out,monitor_error=monitor_error,cases=cases,jvms=jvms,
                identities=list(identities.values()),listeners=sorted(listeners),live=live,
                port_errors=port_errors,port_probe_history=history,sampling_errors=errors,command=command)
    (output/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    require(code==0 and not timed_out and monitor_error is None,'actual CLI signal cases failed')
    require('test result: ok. 4 passed; 0 failed; 0 ignored;' in text,'four real cases did not run')
    require({(case['signal'],case['partial']) for case in cases}=={(s,p) for s in ['TERM','INT'] for p in [False,True]}
            and len(cases)==4,'missing signal/startup combination')
    require(len(jvms)==6,'missing JVM identities for first and partial deployments')
    require(all(any(row['pid']==case['jvm_pid'] for row in jvms) for case in cases),'unobserved blocking component JVM')
    require(all(any(endpoint[2]==case['port'] for endpoint in listeners) for case in cases),'blocking component listeners not independently observed')
    require(not live and not port_errors,'owned descendants or listeners survived')
    print('CLI_STARTUP_SHUTDOWN_PASS: TERM/INT, first/partial deployment, recording and independent retirement',flush=True)


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path)
    args=parser.parse_args()
    output=args.output or Path(tempfile.mkdtemp(prefix='parrot-cli-shutdown-'))/'result'
    print('CLI_SHUTDOWN_ARTIFACTS='+str(output),flush=True)
    run(output)
