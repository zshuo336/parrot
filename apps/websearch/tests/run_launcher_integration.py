"""Exercise the actual shell entrypoints with real polyglot gateways and HTTP."""
from pathlib import Path
from contextlib import ExitStack
import argparse, hashlib, json, os, re, signal, sys, tempfile
ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT/'apps/websearch/tests'))
import run_integration as web

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--report', type=Path, help='Optional copy of the result JSON')
options = parser.parse_args()
root = Path(tempfile.mkdtemp(prefix='ws-launchers-', dir='/tmp'))
report = {'runtime_root':str(root), 'scope':'host direct/registry/serve-only actual entrypoints', 'cases':[]}
print('Artifacts:', root, flush=True)
def interrupted(signum, _frame):
    raise KeyboardInterrupt(f'signal {signum}')
signal.signal(signal.SIGTERM, interrupted)
try:
    original = None
    for name in ['direct', 'registry', 'serve-only']:
        directory = root/name
        directory.mkdir()
        with ExitStack() as stack:
            suite = web.Suite(directory, ROOT/'target/debug/websearch', stack)
            environment = dict(suite.env)
            for key in list(environment):
                if key.startswith('WS_'): environment.pop(key)
            environment.update(WS_PROFILE='dev', WS_RUN_DIR=str(directory/'run'), WS_STARTUP_TIMEOUT='90',
                               WS_TIMEOUT='300', PYTHON=sys.executable, WS_RAY_HTTP_PROXY=suite.runtime_proxy.base)
            if name != 'direct': environment['WS_SKIP_BUILD'] = '1'
            command = ['bash', str(ROOT/('apps/websearch/deploy/run-registry.sh' if name == 'registry' else 'apps/websearch/run.sh')), '--port', '0']
            if name == 'serve-only':
                command += ['--serve-only', '--data', original['data']]
            else:
                command += [suite.corpus.base+'/root', '--crawl-origin', suite.corpus.base,
                            '--pages', '10', '--sites', '1', '--maxdepth', '1']
            owner = suite.process('launcher', command, environment)
            match = web.until(lambda: re.search(r'WEBSEARCH_READY=http://127\.0\.0\.1:(\d+)', owner.text()), [owner], seconds=300)
            port = int(match.group(1))
            if name != 'serve-only':
                app_log = directory/'run/app.log'
                web.until(lambda: '爬取收尾完成' in app_log.read_text(), [owner], seconds=90)
            stats = json.loads(web.text_at(port, '/stats'))
            web.require(stats['docs'] == 2 and stats['terms'] > 0 and stats['postings'] >= 2, f'actual index: {stats}')
            if name == 'serve-only':
                urls, scores = web.results(port, original['token'])
                web.require(dict(zip(urls, scores)) == original['scores'], 'restarted index changed URLs/scores')
                suite.corpus.assert_requests({})
            else:
                scores = suite.assert_search(port, ['/root', '/child'])
                suite.corpus.assert_requests({'/robots.txt':1, '/root':1, '/child':1})
                if name == 'direct':
                    original = dict(token=suite.token, scores=scores, data=str(directory/'run/data'))
            owner.child.send_signal(signal.SIGTERM)
            owner.child.wait(timeout=20)
            web.require(owner.child.returncode == 143, f'signal exit code: {owner.child.returncode}')
            terminal = json.loads((directory/'run/result.json').read_text())
            web.require(terminal['ready'] and terminal['exit_code'] == 143, f'terminal result: {terminal}')
            web.require(all(child['retired'] and child['guardian_exit'] is not None for child in terminal['children']), 'guardians not reaped')
            web.require(next(child for child in terminal['children'] if child['name']=='app')['exit_code']==0, 'application did not acknowledge graceful shutdown')
            web.require('shutdown complete' in (directory/'run/app.log').read_text(), 'application retirement marker missing')
            roles = [child['name'] for child in terminal['children']]
            app_command = next(child['command'] for child in terminal['children'] if child['name']=='app')
            web.require('--insecure-development' not in app_command, 'launcher weakened the default Local transport policy')
            if name == 'serve-only': web.require('jvm' in roles and 'erl' not in roles and 'ray' not in roles, roles)
            if name == 'direct':
                web.require(terminal['build'] == 'dev' and terminal['binary'] == str(ROOT/'target/debug/websearch'), 'wrong build provenance')
                web.require(all(next(child for child in terminal['children'] if child['name']==stage)['exit_code']==0 for stage in ['build-rust','build-gateways','build-search','build-frontier']), 'build stage did not succeed')
            report['cases'].append(dict(name=name, stats=stats, http_requests=dict(suite.corpus.requests),
                                        blocked_requests=suite.corpus.blocked, runtime_probes=suite.runtime_proxy.blocked, terminal=terminal))
        print('Passed actual entrypoint:', name, flush=True)
    report['status'] = 'passed'
except (Exception, KeyboardInterrupt) as error:
    report['status'], report['error'] = 'failed', str(error)
    raise
finally:
    report['binary_sha256'] = hashlib.sha256((ROOT/'target/debug/websearch').read_bytes()).hexdigest()
    result_text = json.dumps(report, indent=2, ensure_ascii=False)+'\n'
    (root/'result.json').write_text(result_text)
    if options.report is not None:
        options.report.write_text(result_text)
