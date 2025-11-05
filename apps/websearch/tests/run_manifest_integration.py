#!/usr/bin/env python3
"""Real embedded manifest host, business oracle, recovery and owned process retirement.

Requires built Rust/Erlang/Ray/JVM artifacts. No external gateways are started by
this runner: the application's standard host must own and assemble them itself.
"""
from contextlib import ExitStack
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import sys
import tempfile
import threading
import time

from ray._private.services import psutil
from run_integration import APP, REPO, Suite, require, until
sys.path.insert(0, str(REPO / "crates/parrot-app/tests"))
from run_gateway_ownership import bind_endpoint, probe_retired_listeners, verify_port_probe


class Host:
    def __init__(self, suite, name, manifest, *flags):
        self.suite, self.name = suite, name
        env = dict(suite.env, RAY_USAGE_STATS_ENABLED="0")
        env.pop("WS_DATA", None)
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
            env[key] = suite.runtime_proxy.base
        self.process = suite.process(name, [suite.binary, "--insecure-development", "--manifest", manifest, *flags], env)
        self.identities, self.listeners, self.errors = {}, set(), []
        self.done = threading.Event()
        self.thread = threading.Thread(target=self.observe, daemon=True)
        self.thread.start()
        suite.stack.callback(self.close)

    def close(self):
        self.done.set()
        self.thread.join(5)

    def observe(self):
        next_ports = 0
        while not self.done.is_set():
            ports = time.monotonic() >= next_ports
            if ports:
                next_ports = time.monotonic() + .3
            try:
                root = psutil.Process(self.process.child.pid)
                for child in [root, *root.children(recursive=True)]:
                    try:
                        created = child.create_time()
                        key = (child.pid, created)
                        if key not in self.identities:
                            self.identities[key] = dict(pid=child.pid, created=created, command=child.cmdline())
                        if ports:
                            for connection in child.net_connections(kind='tcp'):
                                if connection.status == psutil.CONN_LISTEN:
                                    self.listeners.add((int(connection.family), connection.laddr.ip, connection.laddr.port))
                    except (psutil.NoSuchProcess, ProcessLookupError):
                        pass
                    except psutil.AccessDenied as error:
                        self.errors.append(str(error))
            except psutil.NoSuchProcess:
                pass
            self.done.wait(.05)

    def port(self):
        match = until(lambda: re.search(r"PARROT_WEBSEARCH_PORT=(\d+)", self.process.text()), [self.process])
        return int(match.group(1))

    def completed(self, count):
        port = self.port()
        self.suite.completed(self.process, port, [], count)
        return port

    def finish(self, signum=signal.SIGTERM, expected=0, ray=True):
        if signum:
            self.process.child.send_signal(signum)
        code = self.process.child.wait(timeout=90)
        self.close()
        deadline = time.monotonic() + 20
        while True:
            live = []
            for (pid, created), row in self.identities.items():
                try:
                    child = psutil.Process(pid)
                    if child.create_time() == created and child.status() != psutil.STATUS_ZOMBIE:
                        live.append(row)
                except psutil.NoSuchProcess:
                    pass
            if not live or time.monotonic() >= deadline:
                break
            time.sleep(.05)
        port_errors, port_history = probe_retired_listeners({bind_endpoint(endpoint) for endpoint in self.listeners})
        record = dict(exit=code, expected=expected, signal=signum, identities=list(self.identities.values()),
                      live=live, listeners=sorted(self.listeners), port_errors=port_errors, port_probe_history=port_history, sampling_errors=self.errors)
        (self.suite.root / (self.name + '-resources.json')).write_text(json.dumps(record, indent=2) + '\n')
        require(code == expected, f"{self.name}: exit {code} != {expected}: {self.process.text()[-5000:]}")
        require(not live and not port_errors, f"{self.name}: resources survived: {record}")
        require(self.listeners, f"{self.name}: no actual listener observed")
        commands = [' '.join(row['command']) for row in self.identities.values()]
        require(any('raylet' in command for command in commands) == ray, f"{self.name}: unexpected Ray runtime set")
        require(("manifest shutdown complete" in self.process.text()) == (expected == 0), "incorrect shutdown acknowledgement")
        self.process.stop()
        return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=REPO / 'target/debug/websearch')
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix='ws-manifest-', dir='/tmp'))
    report = dict(directory=str(root), passed=[], resources={}, business={}, binary_sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest())
    print('WEBSEARCH_MANIFEST_ARTIFACTS=' + str(root), flush=True)
    try:
        verify_port_probe(root)
        with ExitStack() as stack:
            suite = Suite(root, args.binary.resolve(), stack)
            def passed(name, host=None, **finish):
                if host:
                    report['resources'][name] = host.finish(**finish)
                suite.passed_case(name)
                report['passed'].append(name)
            shipped = APP / 'websearch.app.toml'
            data = root / 'shipped-data'
            host = Host(suite, 'shipped', shipped, '--port', '0', '--data', data, '--pages', '2',
                        '--sites', '2', '--maxdepth', '1', '--crawl-origin', suite.corpus.base, suite.corpus.base + '/root')
            port = host.completed(2)
            original = suite.assert_search(port, ['/root', '/child'])
            report['business']['shipped_scores'] = original
            for component in ['frontier', 'tokenizer', 'search']:
                require(re.search(r'manifest deployed ' + component + r' node=.* version=1\.0\.0', host.process.text()), 'missing deployment version/status')
            suite.corpus.assert_requests({'/robots.txt':1, '/root':1, '/child':1})
            passed('shipped-manifest-real-business', host)
            host = Host(suite, 'serve', shipped, '--port', '0', '--data', data, '--serve-only')
            port = host.port()
            suite.ready(host.process, port, [])
            require(suite.assert_search(port, ['/root', '/child']) == original, 'serve-only changed BM25 scores')
            require('components=["search", "crawler"]' in host.process.text(), 'serve-only started crawl dependencies')
            suite.corpus.assert_requests({'/robots.txt':1, '/root':1, '/child':1})
            passed('serve-only-exact-replay-no-crawl', host, ray=False)

            source = shipped.read_text()
            for path in ['erlang/', 'python/', 'jvm/target/websearch-jvm-1.0.0.jar']:
                source = source.replace('"' + path + '"', '"' + (APP / path).as_uri() + '"')
            for old, new in [('frontier','queue'), ('tokenizer','terms'), ('search','finder'), ('crawler','driver')]:
                source = source.replace('name = "' + old + '"', 'name = "' + new + '"')
                source = re.sub(r'deps = \[[^\]]*\]', lambda match: match.group(0).replace('"' + old + '"', '"' + new + '"'), source)
                source = source.replace('"' + old + ':/user/', '"' + new + ':/user/')
                source = source.replace('to = "' + new + ':/user/' + old + '"', 'to = "' + new + ':/user/' + new + '"')
            source = source.replace('version = "1.0.0"', 'version = "7.8.9"')
            settings = dict(pages=1, sites=2, maxdepth=0, port=0, data='renamed-data',
                            index_data='renamed-index', crawl_origins=[suite.corpus.base], seeds=[suite.corpus.base + '/extra'])
            custom = root / 'renamed.app.toml'
            source = re.sub(r"settings = '[^']*'", lambda _: "settings = '" + json.dumps(settings) + "'", source)
            custom.write_text(source)
            host = Host(suite, 'renamed', custom)
            port = host.completed(1)
            suite.assert_search(port, ['/extra'])
            for component in ['queue', 'terms', 'finder']:
                require(re.search(r'manifest deployed ' + component + r' node=.* version=7\.8\.9', host.process.text()), 'renamed deployment/version ignored')
            require('components=["queue", "terms", "finder", "driver"]' in host.process.text(), 'renamed assembly ignored')
            require((root / 'renamed-index/index').is_dir(), 'manifest index directory ignored')
            require(not (root / 'renamed-data/index').exists(), 'journal and explicit index were conflated')
            passed('renamed-components-version-settings-and-wires', host)
            host = Host(suite, 'override', custom, '--data', root / 'override-data', '--index-data', root / 'override-index',
                        '--pages', '2', '--maxdepth', '1', suite.corpus.base + '/root')
            port = host.completed(2)
            suite.assert_search(port, ['/root', '/child'])
            require((root / 'override-index/index').is_dir(), 'explicit index override ignored')
            passed('explicit-cli-overrides-manifest-settings', host)

            data = root / 'slow-data'
            host = Host(suite, 'term-slow', shipped, '--data', data, '--port', '0', '--pages', '1',
                        '--crawl-origin', suite.corpus.base, suite.corpus.base + '/slow')
            until(suite.corpus.slow_started.is_set, [host.process])
            host.process.child.send_signal(signal.SIGTERM)
            until(lambda: 'stop requested:' in host.process.text(), [host.process])
            suite.corpus.slow_release.set()
            passed('term-during-fetch-persists-before-retirement', host, signum=None)
            host = Host(suite, 'term-replay', shipped, '--data', data, '--port', '0', '--serve-only')
            port = host.port()
            suite.ready(host.process, port, [])
            suite.assert_search(port, ['/slow'])
            passed('term-index-replays-after-owned-gateways-exit', host, ray=False)

            data = root / 'kill-data'
            host = Host(suite, 'kill', custom, '--data', data, '--index-data', root / 'kill-index')
            port = host.completed(1)
            before = suite.assert_search(port, ['/extra'])
            report['business']['kill_scores'] = before
            passed('kill-owner-guardians-retire-descendants', host, signum=signal.SIGKILL, expected=-signal.SIGKILL)
            counts = dict(suite.corpus.requests)
            host = Host(suite, 'kill-recover', custom, '--data', data, '--index-data', root / 'kill-index', '--keep')
            port = host.completed(1)
            require(suite.assert_search(port, ['/extra']) == before, 'keep recovery changed exact scores')
            require(dict(suite.corpus.requests) == counts, 'keep recovery fetched committed URL again')
            passed('kill-keep-recovery-exact-index-without-repeated-http', host)

            for name, signum, expected in [('term-startup', signal.SIGTERM, 0), ('kill-startup', signal.SIGKILL, -signal.SIGKILL)]:
                host = Host(suite, name, shipped, '--data', root / (name + '-data'), '--port', '0', '--pages', '1',
                            '--crawl-origin', suite.corpus.base, suite.corpus.base + '/extra')
                # Observe actual Ray startup before injecting interruption; do not
                # infer child ownership from a log marker or PID number alone.
                until(lambda: any('raylet' in ' '.join(row['command']) for row in list(host.identities.values())), [host.process])
                require('manifest assembled' not in host.process.text(), 'startup interruption arrived after assembly')
                counts = dict(suite.corpus.requests)
                passed(name + '-retires-partial-assembly', host, signum=signum, expected=expected)
                require(dict(suite.corpus.requests) == counts, 'interrupted startup still fetched business pages')

            bad = root / 'bad-class.app.toml'
            bad.write_text(source.replace('websearch.search.SearchComponent', 'websearch.search.DoesNotExist'))
            host = Host(suite, 'rollback', bad, '--data', root / 'rollback-data', '--index-data', root / 'rollback-index')
            passed('later-deployment-failure-retires-earlier-gateways', host, signum=None, expected=1)
            require(not suite.corpus.blocked, 'unexpected outbound business fetches')
            report['requests'] = dict(suite.corpus.requests)
            report['runtime_probes'] = suite.runtime_proxy.blocked
    except Exception as error:
        report['error'] = repr(error)
        raise
    finally:
        (root / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
        if args.report:
            args.report.write_text(json.dumps(report, indent=2) + '\n')


if __name__ == '__main__':
    main()
