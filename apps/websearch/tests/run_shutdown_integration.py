"""Real launcher TERM during an admitted fetch, followed by a fresh JVM replay.

Uses explicitly prepared current debug Rust and JVM/Erlang business artifacts;
run after run_launcher_integration.py, which builds and validates those artifacts.
"""
from contextlib import ExitStack
from pathlib import Path
import argparse, hashlib, json, os, re, signal, sys, tempfile
ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT/'apps/websearch/tests'))
import run_integration as web


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    directory = Path(tempfile.mkdtemp(prefix='ws-shutdown-', dir='/tmp'))
    binary = ROOT/'target/debug/websearch'
    report = dict(scope='real direct/registry launcher in-flight stop and fresh JVM replay',
                  artifacts='explicitly prepared', runtime_root=str(directory), cases=[],
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), status='failed')
    print('Artifacts:', directory, flush=True)
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f'signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    try:
        for mode in ('direct', 'registry'):
            data = directory/(mode+'-index')
            token = origin = None
            for replay in (False, True):
                name = mode+('-replay' if replay else '-interrupt')
                case = directory/name; case.mkdir()
                with ExitStack() as stack:
                    suite = web.Suite(case, binary, stack)
                    env = {k:v for k,v in suite.env.items() if not k.startswith('WS_')}
                    env.update(WS_PROFILE='dev', WS_SKIP_BUILD='1', WS_BIN=str(binary),
                               WS_RUN_DIR=str(case/'run'), WS_STARTUP_TIMEOUT='90', WS_TIMEOUT='180',
                               PYTHON=sys.executable, WS_RAY_HTTP_PROXY=suite.runtime_proxy.base)
                    entry = 'apps/websearch/deploy/run-registry.sh' if mode=='registry' else 'apps/websearch/run.sh'
                    command = ['bash', str(ROOT/entry), '--port','0','--data',str(data)]
                    if replay:
                        command += ['--serve-only']
                    else:
                        token, origin = suite.token, suite.corpus.base
                        command += [origin+'/slow','--crawl-origin',origin,'--pages','1','--sites','1','--maxdepth','0']
                    owner = suite.process('launcher', command, env)
                    match = web.until(lambda: re.search(r'WEBSEARCH_READY=http://127\.0\.0\.1:(\d+)', owner.text()), [owner], seconds=120)
                    port = int(match.group(1))
                    if replay:
                        stats = json.loads(web.text_at(port, '/stats'))
                        urls, scores = web.results(port, token)
                        web.require(stats['docs']==1 and stats['terms']>0 and stats['postings']>0, f'persisted index: {stats}')
                        web.require(urls==[origin+'/slow'] and len(scores)==1 and float(scores[0])>0, f'persisted search: {urls}, {scores}')
                        suite.corpus.assert_requests({})
                        owner.child.send_signal(signal.SIGTERM)
                    else:
                        web.until(suite.corpus.slow_started.is_set, [owner], seconds=30)
                        owner.child.send_signal(signal.SIGTERM)
                        app_log = case/'run/app.log'
                        web.until(lambda: 'stop requested:' in app_log.read_text(), [owner], seconds=5)
                        owner.child.send_signal(signal.SIGTERM)
                        suite.corpus.slow_release.set()
                        stats = None
                    owner.child.wait(timeout=75)
                    web.require(owner.child.returncode==143, f'launcher terminal status: {owner.child.returncode}')
                    terminal = json.loads((case/'run/result.json').read_text())
                    app = next(c for c in terminal['children'] if c['name']=='app')
                    web.require(app['exit_code']==0 and 'shutdown complete' in (case/'run/app.log').read_text(), f'application shutdown: {app}')
                    web.require(all(c['retired'] and c['guardian_exit'] is not None for c in terminal['children']), 'unretired guardian')
                    if not replay:
                        suite.corpus.assert_requests({'/robots.txt':1, '/slow':1})
                    report['cases'].append(dict(name=name, stats=stats, terminal=terminal,
                                                http_requests=dict(suite.corpus.requests)))
                print('PASS', name, flush=True)
        report['status']='passed'
    except (Exception, KeyboardInterrupt) as error:
        report['error']=str(error)
        raise
    finally:
        body=json.dumps(report,indent=2,ensure_ascii=False)+'\n'
        (directory/'result.json').write_text(body)
        if args.report:args.report.write_text(body)
    return 0


if __name__=='__main__':
    raise SystemExit(main())
