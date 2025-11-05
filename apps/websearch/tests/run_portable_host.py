#!/usr/bin/env python3
"""Exercise an installed native release, independent of its build checkout.

For Linux isolation mount the release read-only and hide the original /repo.
The harness provides HTTP oracles and observes processes; it starts no gateways.
"""
from contextlib import ExitStack
from pathlib import Path
import argparse,hashlib,json,os,shutil,signal,subprocess,sys,tempfile
from run_integration import Suite,require,until,REPO
from run_manifest_integration import Host,verify_port_probe


def sha(path):
    digest=hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda:source.read(1024*1024),b''):digest.update(chunk)
    return digest.hexdigest()


def beam_outputs(host):
    found=set()
    for row in list(host.identities.values()):
        for argument in row['command']:
            if Path(argument).name.startswith('parrot-host-beam-'):
                found.add(argument)
    return found


def runtime_commands(host,runtime):
    commands=[row['command'] for row in host.identities.values()]
    expected=str(runtime)
    require(any(expected+'/tools/parrot-obs/gateway_worker.py' in command for command in commands), 'guardian did not use installed runtime')
    require(any(any(expected+'/interop/jvm/target/parrot-protocol-jvm-0.1.0.jar' in arg for arg in command) for command in commands), 'JVM did not use installed classpath')
    if any(any('raylet' in arg for arg in command) for command in commands):
        # The guardian execs the Ray entrypoint in-process. The worker runtime
        # environment must still import protocol modules from this release.
        require(any(any(expected+'/interop/python' in arg for arg in command) for command in commands),
                'Ray workers did not use selected protocol runtime')
    return commands


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    source=parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--release',type=Path)
    source.add_argument('--archive',type=Path)
    parser.add_argument('--report',type=Path,required=True)
    parser.add_argument('--hidden-checkout',type=Path)
    args=parser.parse_args()
    root=Path(tempfile.mkdtemp(prefix='ws-portable-',dir='/tmp'))
    report=dict(status='failed',directory=str(root),cases=[],resources={},
                hidden_checkout=str(args.hidden_checkout) if args.hidden_checkout else None)
    print('PORTABLE_HOST_ARTIFACTS='+str(root),flush=True)
    try:
        if args.archive:
            sys.path.insert(0,str(REPO/'apps/websearch/deploy'))
            from install_bundle import receive
            with args.archive.open('rb') as stream:receive(root/'installed',sha(args.archive),stream)
            release=root/'moved release space # percent%'
            (root/'installed').rename(release)
        else:
            release=args.release.resolve()
        report['release']=str(release)
        if args.hidden_checkout:
            require(not (args.hidden_checkout/'apps/websearch').exists(),'build checkout remains visible')
        manifest=json.loads((release/'MANIFEST.json').read_text())
        require(all(sha(release/name)==digest for name,digest in manifest.items()),'release differs from verified manifest')
        report['binary_sha256']=sha(release/'bin/websearch')
        report['release_manifest_sha256']=sha(release/'MANIFEST.json')
        with ExitStack() as stack:
            suite=Suite(root,release/'bin/websearch',stack)
            suite.env['PYTHONPATH']=''
            suite.env.pop('PARROT_JVM_CLASSPATH',None)
            verify_port_probe(root)
            shipped=release/'websearch.app.toml';runtime=release/'runtime'
            def finish(name,host,**options):
                report['resources'][name]=host.finish(**options)
                runtime_commands(host,runtime)
                report['cases'].append(dict(name=name));print('PASS '+name,flush=True)
            first=Host(suite,'first',shipped,'--port','0','--data',root/'data-a','--pages','2','--sites','2',
                       '--maxdepth','1','--crawl-origin',suite.corpus.base,suite.corpus.base+'/root')
            port=first.completed(2);scores=suite.assert_search(port,['/root','/child'])
            second=Host(suite,'second',shipped,'--port','0','--data',root/'data-b','--pages','1','--sites','2',
                        '--maxdepth','0','--crawl-origin',suite.corpus.base,suite.corpus.base+'/extra')
            second_port=second.completed(1);suite.assert_search(second_port,['/extra'])
            require(suite.assert_search(port,['/root','/child'])==scores,'second host changed first index')
            first_beams=until(lambda:beam_outputs(first),[first.process,second.process])
            second_beams=until(lambda:beam_outputs(second),[first.process,second.process])
            require(first_beams.isdisjoint(second_beams),'hosts share compiled gateway files')
            require(all(Path(path).is_dir() for path in first_beams|second_beams),'live gateway compilation missing')
            report['private_compilation']=dict(first=sorted(first_beams),second=sorted(second_beams))
            finish('concurrent-host-independent-index',second)
            require(all(not Path(path).exists() for path in second_beams),'retired compilation leaked')
            require(suite.assert_search(port,['/root','/child'])==scores,'peer retirement affected surviving host')
            finish('relocated-full-manifest',first)
            require(all(not Path(path).exists() for path in first_beams),'first compilation leaked')
            counts=dict(suite.corpus.requests)
            replay=Host(suite,'replay',shipped,'--port','0','--data',root/'data-a','--serve-only')
            replay_port=replay.port();suite.ready(replay.process,replay_port,[])
            require(suite.assert_search(replay_port,['/root','/child'])==scores,'fresh JVM replay changed exact results')
            require(dict(suite.corpus.requests)==counts,'serve-only fetched pages')
            finish('relocated-exact-replay',replay,ray=False)
            kept=Host(suite,'kill',shipped,'--port','0','--data',root/'data-a','--keep','--pages','2','--sites','2',
                      '--maxdepth','1','--crawl-origin',suite.corpus.base,suite.corpus.base+'/root')
            kill_port=kept.completed(2)
            require(suite.assert_search(kill_port,['/root','/child'])==scores,'keep changed index')
            # /root also discovers /denied, which remains pending because robots
            # denied it. A new owner must refresh robots for this retry, while
            # committed /root and /child must never be fetched again.
            suite.corpus.assert_requests({'/robots.txt':3,'/root':1,'/child':1,'/extra':1})
            kill_beams=until(lambda:beam_outputs(kept),[kept.process])
            finish('killed-native-owner-retires-descendants',kept,signum=signal.SIGKILL,expected=-signal.SIGKILL)
            # A killed native process cannot run TempDir destructors. Its private
            # compilation may remain for diagnosis; it cannot be reused by peers.
            report['kill_compilation_remaining']=[p for p in kill_beams if Path(p).exists()]
            recovered=Host(suite,'recover',shipped,'--port','0','--data',root/'data-a','--keep','--pages','2','--sites','2',
                           '--maxdepth','1','--crawl-origin',suite.corpus.base,suite.corpus.base+'/root')
            recovered_port=recovered.completed(2)
            require(suite.assert_search(recovered_port,['/root','/child'])==scores,'kill recovery changed scores')
            suite.corpus.assert_requests({'/robots.txt':4,'/root':1,'/child':1,'/extra':1})
            recovered_beams=until(lambda:beam_outputs(recovered),[recovered.process])
            require(recovered_beams.isdisjoint(kill_beams),'recovery reused abandoned compilation')
            finish('kill-recovery-without-build-checkout',recovered)
            require(all(not Path(path).exists() for path in recovered_beams),'recovered compilation leaked')
            counts=dict(suite.corpus.requests)
            complete=Host(suite,'completed-recover',shipped,'--port','0','--data',root/'data-b',
                          '--keep','--pages','1','--sites','2','--maxdepth','0',
                          '--crawl-origin',suite.corpus.base,suite.corpus.base+'/extra')
            suite.assert_search(complete.completed(1),['/extra'])
            require(dict(suite.corpus.requests)==counts,'completed-only journal caused HTTP requests')
            finish('completed-journal-recovery-without-http',complete)
            # An explicit runtime must win over the executable's sibling tree.
            alternative=root/'alternative runtime'
            shutil.copytree(runtime,alternative)
            explicit=Host(suite,'explicit-runtime',shipped,'--port','0','--data',root/'data-b',
                          '--runtime-root',alternative,'--serve-only')
            explicit_port=explicit.port();suite.ready(explicit.process,explicit_port,[])
            suite.assert_search(explicit_port,['/extra'])
            report['resources']['explicit-runtime-precedence']=explicit.finish(ray=False)
            runtime_commands(explicit,alternative)
            report['cases'].append(dict(name='explicit-runtime-precedence'))

            # A syntax error must retire the real erlc child and its private
            # output; no stale business code or successful assembly is allowed.
            broken=root/'broken-component';broken.mkdir()
            (broken/'frontier.erl').write_text('-module(frontier).\nthis is invalid Erlang.\n')
            source=shipped.read_text()
            for old,new in [('erlang/',broken),('python/',release/'python'),
                            ('jvm/target/websearch-jvm-1.0.0.jar',release/'jvm/target/websearch-jvm-1.0.0.jar')]:
                source=source.replace('"'+old+'"','"'+new.as_uri()+'"')
            broken_manifest=root/'broken.app.toml';broken_manifest.write_text(source)
            failed=Host(suite,'compiler-failure',broken_manifest,'--port','0','--data',root/'failed-data')
            until(lambda:failed.process.child.poll() is not None)
            report['resources']['compiler-failure-retires']=failed.finish(signum=None,expected=1,ray=False)
            failed_beams=beam_outputs(failed)
            require(failed_beams,'compiler failure was not observed')
            require(any(str(broken/'frontier.erl') in row['command'] for row in failed.identities.values()),
                    'invalid business source was not passed to the real compiler')
            require('gateway terminated:' in failed.process.text(),'failure was unrelated to compiler exit')
            require('manifest assembled' not in failed.process.text(),'invalid component assembled')
            require(all(not Path(path).exists() for path in failed_beams),'failed compilation leaked')
            report['cases'].append(dict(name='compiler-failure-retires'))
            invalid=root/'missing-runtime';untouched=root/'preflight-data'
            bad=subprocess.run([str(suite.binary),'--insecure-development','--manifest',str(shipped),
                '--runtime-root',str(invalid),'--serve-only','--data',str(untouched),'--validate-only'],
                cwd=root,env=suite.env,text=True,capture_output=True,timeout=20)
            (root/'preflight.log').write_text(bad.stdout+bad.stderr)
            require(bad.returncode==2 and not bad.stdout and not untouched.exists() and 'host runtime' in bad.stderr,
                    'missing explicit runtime fell back or changed storage')
            report['cases'].append(dict(name='missing-runtime-rejected-before-side-effects'))
            suite.corpus.assert_requests({'/robots.txt':4,'/root':1,'/child':1,'/extra':1})
            report['requests']=dict(suite.corpus.requests)
            require(all(sha(release/name)==digest for name,digest in manifest.items()),'runtime changed installed release files')
            expected=set(manifest)|{'MANIFEST.json'}
            actual={str(p.relative_to(release)) for p in release.rglob('*') if p.is_file()}
            require(actual==expected,'runtime wrote undeclared files into release: '+str(actual-expected))
        report['status']='passed'
    except BaseException as error:
        report['error']=repr(error);raise
    finally:
        text=json.dumps(report,indent=2,ensure_ascii=False)+'\n'
        (root/'result.json').write_text(text);args.report.write_text(text)


if __name__=='__main__':main()
