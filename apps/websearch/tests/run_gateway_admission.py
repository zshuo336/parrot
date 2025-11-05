#!/usr/bin/env python3
"""Real native-host process-group admission, rollback and slot reuse by compilers."""
from contextlib import ExitStack
from pathlib import Path
import argparse,hashlib,json,subprocess,tempfile
from run_integration import REPO,APP,Suite,require,until
from run_manifest_integration import Host,verify_port_probe


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary',type=Path,default=REPO/'target/debug/websearch')
    parser.add_argument('--report',type=Path,required=True)
    args=parser.parse_args();root=Path(tempfile.mkdtemp(prefix='ws-admission-',dir='/tmp'))
    report=dict(status='failed',directory=str(root),cases=[],resources={},binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest())
    print('GATEWAY_ADMISSION_ARTIFACTS='+str(root),flush=True)
    try:
        verify_port_probe(root)
        with ExitStack() as stack:
            suite=Suite(root,args.binary.resolve(),stack);manifest=APP/'websearch.app.toml'
            for limit in [1,2]:
                host=Host(suite,'limited-'+str(limit),manifest,'--gateway-process-limit',str(limit),
                          '--port','0','--data',root/('limited-'+str(limit)),'--pages','1','--maxdepth','0',
                          '--crawl-origin',suite.corpus.base,suite.corpus.base+'/extra')
                until(lambda:host.process.child.poll() is not None)
                resources=host.finish(signum=None,expected=1,ray=limit==2)
                commands=[row['command'] for row in host.identities.values()]
                require('gateway process group budget exhausted (in use '+str(limit)+', limit '+str(limit)+')' in host.process.text(),
                        'failure was unrelated to actual process-group admission')
                # Deployment receipts are printed only after the entire assembly.
                # Observe the actual compiler, gateway and business peer instead.
                require(any('erl_compile' in command for command in commands),
                        'private gateway compilation was not observed')
                require(any(any('parrot_gw:main(' in arg for arg in command) for command in commands),
                        'compiler slot was not reused by the Erlang gateway')
                require(any('peer' in command for command in commands),
                        'Erlang business peer was not started before admission failed')
                require(any(any('raylet' in arg for arg in command) for command in commands)==(limit==2),
                        'unexpected admitted Ray runtime')
                require('manifest assembled' not in host.process.text(),'over-budget manifest assembled')
                require(not any(any('ParrotGatewayMain' in arg for arg in command) for command in commands),
                        'rejected JVM gateway was started')
                name='limit-'+str(limit)+'-rejects-and-retires-partial-assembly'
                report['resources'][name]=resources;report['cases'].append(dict(name=name))
                print('PASS '+name,flush=True)
            suite.corpus.assert_requests({})
            data=root/'successful-data'
            host=Host(suite,'three-slots',manifest,'--gateway-process-limit','3','--port','0','--data',data,
                      '--pages','1','--maxdepth','0','--crawl-origin',suite.corpus.base,suite.corpus.base+'/extra')
            scores=suite.assert_search(host.completed(1),['/extra'])
            report['resources']['three-slots-full-business']=host.finish()
            report['cases'].append(dict(name='three-slots-full-business'))
            print('PASS three-slots-full-business',flush=True)
            replay=Host(suite,'one-slot-replay',manifest,'--gateway-process-limit','1',
                        '--port','0','--data',data,'--serve-only')
            port=replay.port();suite.ready(replay.process,port,[])
            require(suite.assert_search(port,['/extra'])==scores,'single-slot replay changed exact results')
            report['resources']['one-slot-exact-replay']=replay.finish(ray=False)
            report['cases'].append(dict(name='one-slot-exact-replay'))
            print('PASS one-slot-exact-replay',flush=True)
            for index,flags in enumerate([
                ['--gateway-process-limit','0','--manifest',str(manifest)],
                ['--gateway-process-limit','2'],
                ['--gateway-process-limit','18446744073709551615','--manifest',str(manifest)],
            ]):
                data=root/('invalid-'+str(index))
                result=subprocess.run([str(suite.binary),'--data',str(data),*flags],env=suite.env,capture_output=True,text=True,timeout=20)
                (root/('invalid-'+str(index)+'.log')).write_text(result.stdout+result.stderr)
                require(result.returncode==2 and not result.stdout and not data.exists(),'invalid process policy caused effects or was accepted')
            report['cases'].append(dict(name='invalid-policy-precedes-side-effects'))
            suite.corpus.assert_requests({'/robots.txt':1,'/extra':1})
            report['requests']=dict(suite.corpus.requests)
        report['status']='passed'
    except BaseException as error:
        report['error']=repr(error);raise
    finally:
        text=json.dumps(report,indent=2,ensure_ascii=False)+'\n'
        (root/'result.json').write_text(text);args.report.write_text(text)


if __name__=='__main__':main()
