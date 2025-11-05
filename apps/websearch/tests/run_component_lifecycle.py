"""Verify standalone application retirement against live Erlang/Ray/JVM gateways.

Requires explicitly prepared current debug and gateway artifacts (as produced by
run_launcher_integration.py). Gateways stay alive while component absence and
unattempted component preservation are checked over fresh TCP connections.
"""
from contextlib import ExitStack
from pathlib import Path
import argparse
import hashlib
import json
import re
import shutil
import signal
import socket
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'interop/python'))
sys.path.insert(0, str(ROOT / 'apps/websearch/tests'))
from parrot_protocol import admin_v2, wire
import run_integration as web


class Client:
    def __init__(self, stack, port):
        self.socket = stack.enter_context(socket.create_connection(('127.0.0.1', port), timeout=35))
        self.decoder = wire.FrameDecoder()
        self.sequence = -1  # Handshake uses the protocol control correlation id 0.
        reply = self.exchange(wire.FT_HANDSHAKE, wire.handshake_body('ws-lifecycle-observer'))
        web.require(reply.ft == wire.FT_HANDSHAKE_ACK, 'observer handshake failed')

    def exchange(self, kind, payload):
        self.sequence += 1
        self.socket.sendall(wire.build_frame(kind, self.sequence, '', '', payload))
        while True:
            frame = self.decoder.next_frame()
            if frame is None:
                data = self.socket.recv(65536)
                web.require(data, 'gateway disconnected before observer receipt')
                self.decoder.feed(data)
                continue
            if frame.ft == wire.FT_HEARTBEAT:
                self.socket.sendall(wire.build_frame(wire.FT_HEARTBEAT_ACK, frame.cid, '', '', b''))
                continue
            web.require(frame.cid == self.sequence, 'observer receipt correlation mismatch')
            return frame

    def admin(self, kind, **fields):
        request = dict(kind=kind, req_id=self.sequence + 1, **fields)
        frame = self.exchange(wire.FT_SYSTEM_EVENT, admin_v2.encode_admin_cmd_v2(request))
        web.require(frame.ft == wire.FT_SYSTEM_EVENT, 'expected administrative reply')
        reply = admin_v2.decode_admin_reply_v2(frame.payload)
        web.require(reply['req_id'] == request['req_id'], 'admin receipt correlation mismatch')
        return reply


def paths(app):
    """Read actual deployment receipts; do not guess the namespace or run ID."""
    deployed = {}
    for tag, node in [('erl','erl-gw-1'),('ray','ray-gw-1'),('jvm','jvm-search-1')]:
        found = re.findall(r'\[ws\] deploy ' + node + r'/\S+ → (\[[^\n]+\])', app.text())
        if found:
            values = json.loads(found[-1])
            web.require(len(values) == 1 and values[0].startswith('/user/'), 'one real deployment path required')
            deployed[tag] = values[0]
    return deployed


def intended_paths(data):
    manifest = json.loads((data / 'remote.components').read_text())
    roles = {'erl':'frontier', 'ray':'tokenizer', 'jvm':'search'}
    return {tag: '/user/ws-' + manifest['namespace'] + '-' + role + '-' + manifest['roles'][role]
            for tag, role in roles.items() if role in manifest['roles']}


def status(ports, expected, deployed=None):
    results = {}
    with ExitStack() as stack:
        for tag, name in [('erl', 'frontier'), ('ray', 'tokenizer'), ('jvm', 'search')]:
            reply = Client(stack, ports[tag]).admin('status', path_prefix=(deployed or {}).get(tag, '/user/' + name))
            if expected[tag]:
                web.require(reply['kind'] == 'status' and len(reply['states']) == 1,
                            f'{tag}/{name} must be running: {reply}')
            else:
                web.require(reply['kind'] == 'failed' and reply['code'] == admin_v2.ERR_COMPONENT_NOT_FOUND,
                            f'{tag}/{name} survived application exit: {reply}')
            results[tag] = reply
    return results


def finish(app, gateways, success):
    app.child.wait(timeout=65)
    web.require(app.child.returncode == (0 if success else 1), f'application exit: {app.child.returncode}\n{app.text()}')
    web.require(('shutdown complete' in app.text()) == success, 'incorrect shutdown success marker')
    for gateway in gateways:
        gateway.alive()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix='ws-components-', dir='/tmp'))
    binary = ROOT / 'target/debug/websearch'
    report = dict(scope='standalone app with surviving real gateway processes',
                  runtime_root=str(root), artifacts='explicitly prepared', cases=[], status='failed',
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    print('Artifacts:', root, flush=True)
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f'signal {signum}')
    signal.signal(signal.SIGTERM, interrupted)
    try:
        with ExitStack() as stack:
            suite = web.Suite(root, binary, stack)
            data = root / 'data'
            gateways, ports = suite.gateways(data)
            absent = dict(erl=False, ray=False, jvm=False)
            # A separate frontier proves cleanup uses the owned name, not /user.
            with ExitStack() as observer:
                sentinel = Client(observer, ports['erl']).admin('deploy', component=dict(
                    name='untouched', version='1', artifact=dict(kind='beam', app='frontier',
                    uri=(ROOT/'apps/websearch/erlang').as_uri()), instances=dict(kind='singleton'), config=None))
                web.require(sentinel['kind'] == 'deployed', f'sentinel deployment: {sentinel}')

            app, port = suite.app(data, ports, suite.corpus.base + '/root', '--pages', '10', '--sites', '1', '--maxdepth', '1')
            stats = suite.completed(app, port, gateways, 2)
            scores = suite.assert_search(port, ['/root', '/child'])
            owned = paths(app)
            before = status(ports, dict(erl=True, ray=True, jvm=True), owned)
            app.child.send_signal(signal.SIGTERM)
            finish(app, gateways, True)
            report['cases'].append(dict(name='normal-stop', stats=stats, before=before, after=status(ports, absent, owned)))
            print('PASS normal-stop', flush=True)

            # Same JVM process, new component: the previous actor and file lock
            # must be gone, and the committed index must still be queryable.
            app, port = suite.app(data, ports, '--serve-only')
            stats = suite.ready(app, port, gateways)
            web.require(stats['docs'] == 2, f'redeployed persisted index: {stats}')
            web.require(suite.assert_search(port, ['/root', '/child']) == scores, 'redeployment changed index scores')
            owned = paths(app)
            status(ports, dict(erl=False, ray=False, jvm=True), owned)
            app.child.send_signal(signal.SIGTERM)
            finish(app, gateways, True)
            report['cases'].append(dict(name='serve-only-replay-stop', after=status(ports, absent, owned)))
            print('PASS serve-only-replay-stop', flush=True)

            # Keep a legitimate search service while a later invocation fails
            # on Ray, before attempting JVM. It must not destroy that service.
            with ExitStack() as observer:
                jvm = Client(observer, ports['jvm'])
                reply = jvm.admin('deploy', component=dict(name='search', version='preserved',
                    artifact=dict(kind='jvm', main_class='websearch.search.SearchComponent', coords=None,
                    uri=(ROOT/'apps/websearch/jvm/target/websearch-jvm-1.0.0.jar').as_uri()),
                    instances=dict(kind='singleton'), config=None))
                web.require(reply['kind'] == 'deployed', f'preexisting search deployment: {reply}')
            incomplete = root / 'incomplete-artifacts'
            shutil.copytree(ROOT/'apps/websearch/erlang', incomplete/'erlang')
            app, _ = suite.app(data, ports, '--node-root', str(incomplete), '--pages', '1')
            finish(app, gateways, False)
            web.require('deploy ray-gw-1/tokenizer' in app.text(), 'wrong partial-deployment failure')
            after = status(ports, dict(erl=False, ray=False, jvm=True), dict(intended_paths(data), jvm='/user/search'))
            web.require(after['jvm']['states'][0]['version'] == 'preserved', 'unattempted search changed')
            report['cases'].append(dict(name='partial-deployment-rollback', after=after))
            print('PASS partial-deployment-rollback', flush=True)

            # The legacy search belongs to this fixture, not to the app. Retire
            # it explicitly before exercising the unrelated Clear failure path.
            with ExitStack() as observer:
                reply = Client(observer, ports['jvm']).admin('stop', path_prefix='/user/search')
                web.require(reply['kind'] == 'stopped', 'fixture search retirement')

            # Reject the real JVM Clear reply at the transport boundary. All
            # three deployments must retire despite the primary business error.
            proxy = web.CorruptFirstReply(ports['jvm'], key=b'bin:ws/ClearAck', payload=b'')
            stack.callback(proxy.close)
            app, _ = suite.app(data, dict(ports, jvm=proxy.port), '--pages', '1')
            finish(app, gateways, False)
            owned = paths(app)
            web.require(proxy.corrupted == 1 and 'clear index failed' in app.text(), 'fault not exercised')
            report['cases'].append(dict(name='business-error-rollback', after=status(ports, absent, owned)))
            with ExitStack() as observer:
                erl = Client(observer, ports['erl'])
                untouched = erl.admin('status', path_prefix='/user/untouched')
                web.require(untouched['kind'] == 'status' and len(untouched['states']) == 1,
                            f'unrelated component was stopped: {untouched}')
                reply = erl.admin('stop', path_prefix='/user/untouched')
                web.require(reply['kind'] == 'stopped', f'sentinel retirement: {reply}')
            suite.corpus.assert_requests({'/robots.txt': 1, '/root': 1, '/child': 1})
            report['unrelated_component'] = untouched
            report['requests'] = dict(suite.corpus.requests)
            print('PASS business-error-rollback and unrelated component preservation', flush=True)
        report['status'] = 'passed'
    except (Exception, KeyboardInterrupt) as error:
        report['error'] = str(error)
        raise
    finally:
        body = json.dumps(report, indent=2, ensure_ascii=False) + '\n'
        (root/'result.json').write_text(body)
        if args.report:
            args.report.write_text(body)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
