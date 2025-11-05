"""An independent data directory must not stop or overwrite another live app."""
from contextlib import ExitStack
from pathlib import Path
import argparse
import hashlib
import json
import tempfile
import signal

from run_component_lifecycle import Client, paths, intended_paths, status
from parrot_protocol import wire, admin_v2

from run_integration import Suite, require, until, results
from run_frontier_recovery import stop_app, HeldReply
from run_integration import CorruptFirstReply

ROOT = Path(__file__).resolve().parents[3]


def business(client, path, key, body=b''):
    client.sequence += 1
    client.socket.sendall(wire.build_frame(wire.FT_ASK, client.sequence, path, key,
                                         wire.with_reply_to_prefix('/observer/reply', body)))
    while True:
        frame = client.decoder.next_frame()
        if frame is None:
            data = client.socket.recv(65536)
            require(data, 'observer disconnected'); client.decoder.feed(data); continue
        if frame.ft == wire.FT_HEARTBEAT:
            client.socket.sendall(wire.build_frame(wire.FT_HEARTBEAT_ACK, frame.cid, '', '', b'')); continue
        require(frame.cid == client.sequence, 'business correlation mismatch')
        return frame


class HeldDeployment(HeldReply):
    def __init__(self, upstream):
        super().__init__(upstream, b'')

    def exact(self, sock, size):
        import struct
        body = CorruptFirstReply.exact(sock, size)
        if len(body) >= 28 and body[1] == wire.FT_SYSTEM_EVENT and not self.observed.is_set():
            path_size, = struct.unpack_from('<I', body, 20)
            key_size, = struct.unpack_from('<I', body, 24 + path_size)
            payload = body[28 + path_size + key_size:]
            try:
                reply = admin_v2.decode_admin_reply_v2(payload)
            except ValueError:
                return body
            if reply['kind'] == 'deployed':
                self.receipt = payload; self.observed.set()
                while not self.stopping.is_set() and not self.release.wait(.1):
                    pass
        return body


def lost_deployment_receipt(binary, report):
    root = Path(tempfile.mkdtemp(prefix='ws-ownack-', dir='/tmp'))
    report['ack_runtime_root'] = str(root)
    with ExitStack() as stack:
        suite = Suite(root, binary, stack); data = root / 'data'
        gateways, ports = suite.gateways(data)
        proxy = HeldDeployment(ports['jvm']); stack.callback(proxy.close)
        app, _ = suite.app(data, dict(ports, jvm=proxy.port), suite.corpus.base + '/extra',
                           '--pages','1','--sites','1','--maxdepth','0')
        until(proxy.observed.is_set, [app, *gateways])
        before = intended_paths(data)
        receipt = admin_v2.decode_admin_reply_v2(proxy.receipt)
        require(receipt['instances'] == [before['jvm']], 'held deployment must match durable intent')
        require(suite.corpus.requests['/extra'] == 0, 'HTTP ran before all deployment receipts')
        app.child.send_signal(signal.SIGKILL); app.child.wait(timeout=10); proxy.release.set()
        require(app.child.returncode == -signal.SIGKILL, 'actual loss before deployment receipt')
        recovered, port = suite.app(data, ports, suite.corpus.base + '/extra', '--keep',
                                    '--pages','1','--sites','1','--maxdepth','0')
        suite.completed(recovered, port, gateways, 1)
        suite.assert_search(port, ['/extra'])
        status(ports, dict(erl=False,ray=False,jvm=False), before)
        require(suite.corpus.requests['/extra'] == 1, 'lost deployment recovery fetch count')
        stop_app(recovered)
        report['cases'].append(dict(name='lost real Deployed acknowledgement recovers on surviving gateways',
                                    receipt_hex=proxy.receipt.hex(), old_paths=before, requests=dict(suite.corpus.requests)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix='ws-owner-', dir='/tmp'))
    binary = ROOT / 'target/debug/websearch'
    report = dict(status='failed', runtime_root=str(root), cases=[],
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    print('Artifacts:', root, flush=True)
    try:
        with ExitStack() as stack:
            suite = Suite(root, binary, stack)
            data = root / 'first'
            gateways, ports = suite.gateways(data)
            first, port = suite.app(data, ports, suite.corpus.base + '/child',
                                    '--pages', '1', '--sites', '1', '--maxdepth', '0')
            suite.completed(first, port, gateways, 1)
            before = suite.assert_search(port, ['/child'])
            contender, other_port = suite.app(root / 'second', ports, suite.corpus.base + '/extra',
                                              '--pages', '1', '--sites', '1', '--maxdepth', '0')
            until(lambda: contender.child.poll() is not None or '爬取收尾完成' in contender.text(), gateways)
            case = dict(name='independent directory cannot replace a live index', exit=contender.child.poll(),
                        requests=dict(suite.corpus.requests))
            report['cases'].append(case)
            if contender.child.poll() is None:
                case['contender_health'] = suite.ready(contender, other_port, gateways)
                case['first_service_hits_after_contender'] = results(port, suite.token)[0]
            require(contender.child.poll() is not None and contender.child.returncode != 0,
                    'independent application replaced live components: ' + str(case))
            require('directory already has an owner' in contender.text(),
                    'contender must fail on the occupied JVM index, not an unrelated error: ' + contender.text())
            first.alive()
            require(suite.corpus.requests['/extra'] == 0, 'contender fetched before acquiring index ownership')
            require(suite.assert_search(port, ['/child']) == before, 'first index changed after contender cleanup')
            case['contender_cleanup'] = status(ports, dict(erl=False, ray=False, jvm=False), intended_paths(root / 'second'))
            original_paths = paths(first)
            replay, _ = suite.app(root / 'second', ports, '--serve-only')
            until(lambda: replay.child.poll() is not None, gateways)
            require(replay.child.returncode != 0 and 'directory already has an owner' in replay.text(),
                    'independent serve-only must not steal the index')
            require(suite.assert_search(port, ['/child']) == before, 'serve-only contender changed live search')
            report['cases'].append(dict(name='independent serve-only cannot steal running index'))

            # Do not restart gateways: stale components are still real and live.
            first.child.send_signal(signal.SIGKILL); first.child.wait(timeout=10)
            require(first.child.returncode == -signal.SIGKILL, 'actual application SIGKILL')
            read_only, read_port = suite.app(data, ports, '--serve-only')
            require(suite.ready(read_only, read_port, gateways)['docs'] == 1, 'serve-only restart lost index')
            stop_app(read_only)
            recovered, port = suite.app(data, ports, suite.corpus.base + '/child', '--keep',
                                       '--pages', '1', '--sites', '1', '--maxdepth', '0')
            suite.completed(recovered, port, gateways, 1)
            current = paths(recovered)
            require(set(current) == set(original_paths) == {'erl','ray','jvm'}, 'all deployment receipts required')
            require(all(current[tag] != original_paths[tag] for tag in current), 'restart reused a stale address')
            require(suite.corpus.requests['/child'] == 1 and suite.corpus.requests['/extra'] == 0,
                    'recovery fetched completed or foreign URL')
            require(suite.assert_search(port, ['/child']) == before, 'same-gateway recovery changed index')
            report['cases'].append(dict(name='SIGKILL then serve-only then full recovery preserves all role intents',
                                        before=original_paths, after=current))

            with ExitStack() as observer:
                stale = {}
                for tag, key, body in [('erl','bin:ws/Push',bytes(4)), ('ray','bin:ws/Tokenize',bytes(4)),
                                       ('jvm','bin:ws/Clear',b'')]:
                    client = Client(observer, ports[tag])
                    frame = business(client, original_paths[tag], key, body)
                    require(frame.ft == wire.FT_REPLY_ERR, 'stale business address reached replacement: ' + tag)
                    reply = client.admin('stop', path_prefix=original_paths[tag])
                    require(reply['kind'] == 'failed' and reply['code'] == admin_v2.ERR_COMPONENT_NOT_FOUND,
                            'stale retirement reached replacement: ' + str(reply))
                    alive = client.admin('status', path_prefix=current[tag])
                    require(alive['kind'] == 'status' and len(alive['states']) == 1, 'replacement disappeared')
                    stale[tag] = dict(reply_error=frame.payload.hex(), stop=reply, running=alive)
                require(suite.assert_search(port, ['/child']) == before, 'stale Clear changed the index')
                report['cases'].append(dict(name='stale business and Stop addresses cannot affect replacements', evidence=stale))
            stop_app(recovered)

            # A legacy fixed-name component belongs to an explicit administrator.
            # Its existence does not authorize an application to stop it.
            with ExitStack() as observer:
                client = Client(observer, ports['jvm'])
                reply = client.admin('deploy', component=dict(name='search', version='legacy-owner',
                    artifact=dict(kind='jvm', main_class='websearch.search.SearchComponent', coords=None,
                                  uri=(ROOT/'apps/websearch/jvm/target/websearch-jvm-1.0.0.jar').as_uri()),
                    instances=dict(kind='singleton'), config=None))
                require(reply['kind'] == 'deployed', 'legacy fixture deployment')
                denied, _ = suite.app(data, ports, '--serve-only')
                until(lambda: denied.child.poll() is not None, gateways)
                require(denied.child.returncode != 0 and 'directory already has an owner' in denied.text(),
                        'application stopped an unowned legacy component')
                alive = client.admin('status', path_prefix='/user/search')
                require(alive['kind'] == 'status' and alive['states'][0]['version'] == 'legacy-owner', 'legacy owner lost')
                frame = business(client, '/user/search', 'bin:ws/Healthz')
                require(frame.ft == wire.FT_REPLY and json.loads(frame.payload)['docs'] == 1, 'legacy index changed')
                require(client.admin('stop', path_prefix='/user/search')['kind'] == 'stopped', 'legacy fixture retirement')
                report['cases'].append(dict(name='unowned legacy fixed component preserved', status=alive))
        lost_deployment_receipt(binary, report)
        report['status'] = 'passed'
        print('PASS independent component ownership', flush=True)
    except BaseException as error:
        report['error'] = str(error)
        raise
    finally:
        body = json.dumps(report, indent=2) + '\n'
        (root / 'result.json').write_text(body)
        if args.report:
            args.report.write_text(body)


if __name__ == '__main__':
    main()
