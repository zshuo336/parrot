#!/usr/bin/env python3
"""Verify actual app/Erlang/Ray/JVM images in two isolated Compose projects.

Requires Linux Docker Compose and prepared musl app/JVM artifacts. Default builds
the checked-in Dockerfiles. --skip-build requires an explicit image prefix whose
four existing tags are PREFIX-app, PREFIX-erl, PREFIX-ray and PREFIX-jvm.
Corpus traffic uses owned project networks and an explicit crawl origin.
Networks use the production bridge mode so host port publication is exercised.
Reports survive cleanup.
"""
import argparse
import copy
import hashlib
from html.parser import HTMLParser
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
ROLES = ('app', 'erl', 'ray', 'jvm')
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))
spec = importlib.util.spec_from_file_location('websearch_compose_config', ROOT/'apps/websearch/deploy/compose_config.py')
compose_config = importlib.util.module_from_spec(spec)
spec.loader.exec_module(compose_config)


def require(condition, detail):
    if not condition:
        raise RuntimeError(detail)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Links(HTMLParser):
    def __init__(self):
        super().__init__()
        self.urls = []

    def handle_starttag(self, tag, attrs):
        href = dict(attrs).get('href', '')
        if tag == 'a' and href.startswith('http://'):
            self.urls.append(href)


class Acceptance:
    def __init__(self, directory, image_prefix, skip_build):
        self.directory = directory
        self.identity = 'parrot-websearch-' + uuid.uuid4().hex[:12]
        self.prefix = image_prefix or self.identity + ':image'
        self.skip_build = skip_build
        self.projects = []
        self.networks = []
        self.corpora = []
        self.images = []
        self.events = []
        self.report = dict(status='failed', build='explicitly skipped' if skip_build else 'required', cases=[], images={})

    def run(self, *command, timeout=60, check=True):
        start = time.monotonic()
        try:
            result = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired as error:
            name = f'command-{len(self.events):04d}-timeout.log'
            output = b''.join(part.encode() if isinstance(part, str) else (part or b'')
                              for part in (error.stdout, error.stderr))
            (self.directory/name).write_bytes(output)
            self.events.append(dict(command=command, timeout=True, seconds=time.monotonic()-start, log=name))
            raise RuntimeError(f'command timed out: {command}') from error
        event = dict(command=command, exit=result.returncode, seconds=time.monotonic()-start)
        if result.returncode:
            name = f'command-{len(self.events):04d}-failed.log'
            (self.directory/name).write_text(result.stdout+result.stderr)
            event['log'] = name
        self.events.append(event)
        if check and result.returncode:
            raise RuntimeError(f'{command}: exit {result.returncode}\n{result.stdout[-4000:]}\n{result.stderr[-4000:]}')
        return result

    def docker(self, *args, **kwargs):
        return self.run('docker', *args, **kwargs)

    def compose(self, project, *args, **kwargs):
        return self.docker('compose', '--project-name', project['name'], '-f', str(project['config']), *args, **kwargs)

    def write(self, name, value):
        (self.directory/name).write_text(json.dumps(value, indent=2, ensure_ascii=False)+'\n')

    def model(self, name):
        environment = dict(os.environ, WS_PLATFORM=self.platform, WS_WEB_PORT='0', COMPOSE_PROJECT_NAME=name)
        result = subprocess.run(['docker', 'compose', '--project-name', name, '-f',
            str(ROOT/'apps/websearch/deploy/docker-compose.yml'), 'config', '--format', 'json'],
            env=environment, capture_output=True, text=True, timeout=60, check=True)
        model = json.loads(result.stdout)
        for role in ROLES:
            service = model['services'][role]
            require('container_name' not in service, 'fixed container names defeat project isolation')
            service['image'] = self.prefix + '-' + role
        return model

    def prepare(self):
        require(platform.system() == 'Linux', 'native image acceptance requires Linux')
        arch = {'x86_64': ('amd64', 'x86_64'), 'aarch64': ('arm64', 'aarch64')}.get(platform.machine())
        require(arch is not None, 'unsupported host architecture')
        self.platform = 'linux/' + arch[0]
        self.binary = ROOT/f'target/{arch[1]}-unknown-linux-musl/release/websearch'
        artifacts = [self.binary, ROOT/'interop/jvm/target/parrot-protocol-jvm-0.1.0.jar',
                     ROOT/'apps/websearch/jvm/target/websearch-jvm-1.0.0.jar', ROOT/'gw-jvm-libs.tar']
        self.report['artifact_sha256'] = {str(p.relative_to(ROOT)): digest(p) for p in artifacts}
        self.protocol = {str(p.relative_to(ROOT/'interop/python/parrot_protocol')): digest(p)
                         for p in (ROOT/'interop/python/parrot_protocol').rglob('*.py')}
        self.erlang = {p.name: digest(p) for p in (ROOT/'interop/erlang').glob('parrot_*.erl')
                       if p.name in ('parrot_gw.erl', 'parrot_trace.erl', 'parrot_component_runtime.erl', 'parrot_beam_snapshot.erl',
                                     'parrot_beam_guest.erl', 'parrot_beam_host.erl')}
        self.report['packaged_python_sha256'] = self.protocol
        self.report['packaged_erlang_source_sha256'] = self.erlang
        build = dict(name=self.identity+'-build', config=self.directory/'build.json')
        self.write('build.json', self.model(build['name']))
        if not self.skip_build:
            for role in ROLES:
                existing = self.docker('image', 'inspect', self.prefix+'-'+role, check=False)
                require(existing.returncode != 0 and 'No such' in existing.stderr,
                        'refusing to replace an existing or unverifiable image: '+self.prefix+'-'+role)
            self.images = [self.prefix+'-'+role for role in ROLES]
            result = self.compose(build, 'build', timeout=1800, check=False)
            (self.directory/'build.log').write_text(result.stdout+result.stderr)
            require(result.returncode == 0, 'image build failed; see build.log')
        for role in ROLES:
            metadata = json.loads(self.docker('image', 'inspect', self.prefix+'-'+role).stdout)[0]
            require((metadata['Os'], metadata['Architecture']) == ('linux', arch[0]), f'{role} platform mismatch')
            self.report['images'][role] = metadata['Id']

    def create(self, label, port):
        name = self.identity+'-'+label
        # Reject an existing project before authorizing cleanup by its labels.
        for args in [('ps', '-a'), ('volume', 'ls')]:
            require(not self.docker(*args, '--filter', 'label=com.docker.compose.project='+name, '-q').stdout.strip(),
                    'project identity already exists: '+name)
        network = name+'-network'
        self.networks.append(network)
        self.docker('network', 'create', '--driver', 'bridge', '--label', 'parrot.websearch.verify='+self.identity, network)
        corpus = name+'-corpus'
        token = 'parrot'+uuid.uuid4().hex
        self.corpora.append(corpus)
        self.docker('run', '-d', '--init', '--name', corpus, '--label', 'parrot.websearch.verify='+self.identity,
                    '--network', network, '--network-alias', 'corpus', '--cpus', '1', '--memory', '256m',
                    '--mount', f'type=bind,src={ROOT / "scripts/fixtures/websearch_corpus.py"},dst=/fixture.py,readonly',
                    self.prefix+'-ray', 'python3', '-u', '/fixture.py', token)
        state = json.loads(self.docker('inspect', corpus).stdout)[0]
        address = state['NetworkSettings']['Networks'][network]['IPAddress']
        require(bool(address), 'missing corpus address')
        deadline = time.monotonic()+30
        while True:
            ready = self.docker('exec', corpus, 'python3', '-c',
                'import urllib.request;urllib.request.urlopen("http://127.0.0.1:9000/__state",timeout=2).read()',
                check=False)
            if ready.returncode == 0:
                break
            require(time.monotonic() < deadline, 'corpus readiness deadline')
            time.sleep(.2)
        project = dict(name=name, config=self.directory/(label+'.json'), network=network, corpus=corpus,
                       origin='http://'+address+':9000', token=token, port=port, stopped=False,
                       data='/data/tenant '+label+' ${HOME}', phase='direct')
        self.projects.append(project)
        model = self.model(name)
        model['networks'] = {'ws-net': {'external': True, 'name': network}}
        for role in ROLES:
            service = model['services'][role]
            service.update(cpus=2, mem_limit='4g', pids_limit=2048)
            if role == 'ray':
                service['shm_size'] = '512m'
                service.setdefault('environment', {})['RAY_USAGE_STATS_ENABLED'] = '0'
        project['base'] = model
        self.configure(project, [project['origin']+'/root', project['origin']+'/missing',
                                 '--sites', '1', '--maxdepth', '1', '--pages', '10'])
        self.compose(project, 'up', '-d', '--no-build', timeout=120)
        return project

    def packaged_code(self, project):
        containers = self.containers(project)
        code = 'import pathlib,hashlib,json;p=pathlib.Path("/gw/parrot_protocol");print(json.dumps({str(f.relative_to(p)):hashlib.sha256(f.read_bytes()).hexdigest() for f in p.rglob("*.py")}))'
        actual = json.loads(self.docker('exec', containers['ray'], 'python3', '-c', code).stdout)
        require(actual == self.protocol, 'Ray image protocol package differs from source')
        for filename, expected in self.erlang.items():
            actual = self.docker('exec', containers['erl'], 'sha256sum', '/gw/'+filename).stdout.split()[0]
            require(actual == expected, 'Erlang image source mismatch: '+filename)
        actual = self.docker('exec', containers['jvm'], 'sha256sum', '/gw/parrot-protocol-jvm-0.1.0.jar').stdout.split()[0]
        require(actual == self.report['artifact_sha256']['interop/jvm/target/parrot-protocol-jvm-0.1.0.jar'], 'JVM image gateway artifact mismatch')

    def stopped_states(self, project, roles):
        states = {}
        for role in roles:
            identities = self.compose(project, 'ps', '--all', '--quiet', role).stdout.split()
            require(len(identities) == 1, 'missing stopped container')
            state = json.loads(self.docker('inspect', identities[0]).stdout)[0]['State']
            require(not state['Running'] and not state['OOMKilled'] and state['ExitCode'] in (0,143),
                    f'unclean {role} termination: {state}')
            states[role] = state
        self.report.setdefault('stops', []).append(states)

    def configure(self, project, arguments):
        model = compose_config.configure(copy.deepcopy(project['base']),
            ['--port', str(project['port']), '--data', project['data'], '--crawl-origin', project['origin'], *arguments])
        self.write(project['config'].name, model)
        self.write(project['name']+'-'+project['phase']+'-config.json', model)

    def containers(self, project):
        result = {}
        for role in ROLES:
            ids = self.compose(project, 'ps', '--all', '--quiet', role).stdout.split()
            require(len(ids) == 1, f'{project["name"]}/{role}: expected one container, got {ids}')
            info = json.loads(self.docker('inspect', ids[0]).stdout)[0]
            require(info['Config']['Labels']['com.docker.compose.project'] == project['name'], 'wrong project ownership')
            require(info['State']['Running'] and info['RestartCount'] == 0,
                    f'{role} exited/restarted: {info["State"]}, restarts={info["RestartCount"]}')
            result[role] = ids[0]
        return result

    def text(self, project, path):
        with HTTP.open(project['url']+path, timeout=3) as response:
            require(response.status == 200, f'{path}: {response.status}')
            return response.read().decode()

    def search(self, project, token):
        body = self.text(project, '/search?'+urllib.parse.urlencode({'q':token}))
        parser = Links(); parser.feed(body)
        scores = re.findall(r'score (\d+\.\d+)', body)
        require(len(scores) == len(parser.urls), 'search result/score mismatch')
        return dict(zip(parser.urls, scores)), parser.urls

    def verify(self, project, paths, requests, crawl=True):
        deadline = time.monotonic()+240
        last = None
        while time.monotonic() < deadline:
            containers = self.containers(project)
            logs = self.docker('logs', containers['app']).stdout
            address = self.compose(project, 'port', 'app', str(project['port'])).stdout.strip()
            require(address.startswith('127.0.0.1:'), f'non-loopback publication: {address}')
            project['url'] = 'http://'+address
            try:
                stats = json.loads(self.text(project, '/stats'))
                if compose_config.healthy(stats) and (not crawl or '爬取收尾完成' in logs):
                    break
            except (OSError, ValueError) as error:
                last = repr(error)
            time.sleep(.3)
        else:
            raise RuntimeError(f'business deadline: {project["name"]}; last={last}')
        require(stats['docs'] == len(paths) and stats['terms'] > 0 and stats['postings'] >= len(paths), f'wrong business index: {stats}')
        scores, urls = self.search(project, project['token'])
        expected = {project['origin']+path for path in paths}
        require(set(urls) == expected and len(urls) == len(expected), f'wrong search URLs: {urls}')
        require(all(float(score)>0 for score in scores.values()), 'invalid search scores')
        state = json.loads(self.docker('exec', project['corpus'], 'python3', '-c',
            'import urllib.request;print(urllib.request.urlopen("http://127.0.0.1:9000/__state",timeout=3).read().decode())').stdout)
        require(state == dict(requests=requests, blocked=[]), f'exact corpus mismatch: {state} != {requests}')
        cmd = json.loads(self.docker('inspect', containers['app']).stdout)[0]['Config']['Cmd']
        require(cmd.count('--data') == 1 and cmd[cmd.index('--data')+1] == project['data'], 'literal CLI data path changed')
        embedded = self.docker('exec', containers['app'], 'sha256sum', '/usr/local/bin/app').stdout.split()[0]
        require(embedded == digest(self.binary), 'application image binary differs from prepared artifact')
        self.logs(project)
        row = dict(project=project['name'], phase=project['phase'], stats=stats, scores=scores,
                   url=project['url'], corpus=state, containers=containers)
        self.report['cases'].append(row)
        print('PASS', project['name'], project['phase'], stats, flush=True)
        return scores

    def logs(self, project):
        for role in ROLES:
            result = self.compose(project, 'logs', '--no-color', role, check=False)
            (self.directory/(project['name']+'-'+project['phase']+'-'+role+'.log')).write_text(result.stdout+result.stderr)

    def exercise(self):
        self.prepare()
        expected = {'/robots.txt':1, '/root':1, '/child':1, '/missing':1}
        first = self.create('a', 9090)
        self.verify(first, ['/root','/child'], expected)
        self.packaged_code(first)
        second = self.create('b', 9091)
        self.verify(second, ['/root','/child'], expected)
        require(first['url'] != second['url'], 'projects share a publication')
        for own, other in [(first,second),(second,first)]:
            require(self.search(own, other['token'])[1] == [], 'cross-project index contamination')
        self.compose(first, 'down', '-v', '--remove-orphans', timeout=120)
        first['stopped'] = True
        second['phase'] = 'survives-other-project-down'
        self.verify(second, ['/root','/child'], expected)
        self.compose(second, 'stop', 'app', timeout=90)
        self.stopped_states(second, ('app',))
        second['phase'] = 'keep'
        # Preserve pending discovery depth and budget both retries plus the seed.
        self.configure(second, ['--keep', second['origin']+'/extra', '--sites','1','--maxdepth','1','--pages','3'])
        self.compose(second, 'up', '-d', '--no-build', '--no-deps', 'app', timeout=120)
        expected = dict(expected, **{'/robots.txt':2, '/missing':2, '/extra':1})
        before = self.verify(second, ['/root','/child','/extra'], expected)
        old_jvm = json.loads(self.docker('inspect', self.containers(second)['jvm']).stdout)[0]['State']['StartedAt']
        self.compose(second, 'stop', 'app', timeout=90)
        self.compose(second, 'stop', timeout=120)
        self.stopped_states(second, ROLES)
        second['phase'] = 'replay'
        self.configure(second, ['--serve-only'])
        self.compose(second, 'up', '-d', '--no-build', timeout=120)
        after = self.verify(second, ['/root','/child','/extra'], expected, crawl=False)
        new_jvm = json.loads(self.docker('inspect', self.containers(second)['jvm']).stdout)[0]['State']['StartedAt']
        require(old_jvm != new_jvm and before == after, 'real JVM restart changed persisted search results/scores')
        self.report['jvm_restarted'] = True
        self.interrupt_wave(second, expected)

    def interrupt_wave(self, project, expected):
        self.compose(project, 'stop', 'app', timeout=90)
        project['phase'] = 'interrupt-wave'
        self.configure(project, ['--keep', project['origin']+'/slow', '--sites','1','--maxdepth','1','--pages','3'])
        self.compose(project, 'up', '-d', '--no-build', '--no-deps', 'app', timeout=120)
        container = self.containers(project)['app']
        def corpus(path):
            return json.loads(self.docker('exec', project['corpus'], 'python3', '-c',
                'import urllib.request;print(urllib.request.urlopen("http://127.0.0.1:9000"+'+repr(path)+',timeout=3).read().decode())').stdout)
        deadline = time.monotonic()+120
        while corpus('/__state')['requests'].get('/slow') != 1:
            require(time.monotonic()<deadline, 'slow fetch was never admitted')
            self.containers(project)
            time.sleep(.1)
        self.docker('kill', '--signal', 'TERM', container)
        deadline = time.monotonic()+5
        observed = False
        while time.monotonic()<deadline:
            logs = self.docker('logs', container)
            observed = 'stop requested:' in logs.stdout+logs.stderr
            state = json.loads(self.docker('inspect', container).stdout)[0]['State']
            if observed or not state['Running']:
                break
            time.sleep(.1)
        if observed:
            # A second TERM while the admitted HTTP response remains blocked
            # must not bypass completion of the same shutdown operation.
            self.docker('kill', '--signal', 'TERM', container)
        corpus('/__release')
        deadline = time.monotonic()+65
        while True:
            state = json.loads(self.docker('inspect', container).stdout)[0]['State']
            if not state['Running']:
                break
            require(time.monotonic()<deadline, 'signalled application did not retire')
            time.sleep(.1)
        self.logs(project)
        logs = self.docker('logs', container)
        complete = 'shutdown complete' in logs.stdout+logs.stderr
        self.report['interrupted_wave'] = dict(stop_observed=observed, complete=complete, state=state,
                                                corpus=corpus('/__state'))
        # Restart the actual JVM before inspecting persistence. The old binary
        # is allowed to reach this check so its lost page is directly observable.
        self.compose(project, 'stop', timeout=120)
        project['phase'] = 'interrupted-wave-replay'
        self.configure(project, ['--serve-only'])
        self.compose(project, 'up', '-d', '--no-build', timeout=120)
        requests = dict(expected, **{'/robots.txt':3, '/missing':3, '/slow':1})
        self.verify(project, ['/root','/child','/extra','/slow'], requests, crawl=False)
        require(observed and complete and state['ExitCode']==0 and not state['OOMKilled'],
                'interrupted wave lacked acknowledged clean shutdown')

    def cleanup(self):
        errors = []
        for project in reversed(self.projects):
            if project['stopped']:
                continue
            try:
                self.logs(project)
            except Exception as error:
                errors.append(str(error))
            try:
                self.compose(project, 'stop', 'app', timeout=90)
            except Exception as error:
                errors.append(str(error))
            try:
                self.compose(project, 'down', '-v', '--remove-orphans', timeout=120)
            except Exception as error:
                errors.append(str(error))
        for name in reversed(self.corpora):
            try:
                result = self.docker('inspect', name, check=False)
                if result.returncode and 'No such' in result.stderr:
                    continue
                metadata = json.loads(result.stdout)[0]
                require(metadata['Config']['Labels'].get('parrot.websearch.verify') == self.identity, 'refusing foreign container cleanup')
                self.docker('rm', '-f', '-v', name)
            except Exception as error:
                errors.append(str(error))
        for name in reversed(self.networks):
            try:
                result = self.docker('network', 'inspect', name, check=False)
                if result.returncode and 'No such' in result.stderr:
                    continue
                metadata = json.loads(result.stdout)[0]
                require(metadata['Labels'].get('parrot.websearch.verify') == self.identity, 'refusing foreign network cleanup')
                self.docker('network', 'rm', name)
            except Exception as error:
                errors.append(str(error))
        for name in self.images:
            try:
                result = self.docker('image', 'rm', '-f', name, check=False)
                require(result.returncode == 0 or 'No such' in result.stderr, result.stderr)
            except Exception as error:
                errors.append(str(error))
        self.report['cleanup_errors'] = errors
        require(not errors, '\n'.join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--skip-build', action='store_true')
    parser.add_argument('--image-prefix')
    parser.add_argument('--report-dir', type=Path)
    args = parser.parse_args()
    if args.skip_build and not args.image_prefix:
        parser.error('--skip-build requires --image-prefix')
    if args.report_dir:
        args.report_dir.mkdir(parents=True, exist_ok=False)
        directory = args.report_dir.resolve()
    else:
        directory = Path(tempfile.mkdtemp(prefix='parrot-websearch-images-'))
    print('Artifacts:', directory, flush=True)
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f'signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    acceptance = Acceptance(directory, args.image_prefix, args.skip_build)
    code = 1
    try:
        acceptance.exercise()
        code = 0
    except (Exception, KeyboardInterrupt) as error:
        acceptance.report['error'] = str(error)
        print('FAIL:', error, flush=True)
    finally:
        try:
            acceptance.cleanup()
        except Exception as error:
            code = 1
            acceptance.report['cleanup_error'] = str(error)
        acceptance.report['status'] = 'passed' if code == 0 else 'failed'
        acceptance.write('result.json', acceptance.report)
        acceptance.write('commands.json', acceptance.events)
    return code


if __name__ == '__main__':
    raise SystemExit(main())
