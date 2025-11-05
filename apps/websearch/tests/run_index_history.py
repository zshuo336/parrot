"""Reject a restored index branch even after its generation catches up.

Uses real HTTP, gateways, crawler journals, and durable Flush acknowledgements.
All removed test snapshots are retained in the invocation's evidence directory.
"""
from contextlib import ExitStack
from pathlib import Path
import argparse
import hashlib
import json
import shutil
import tempfile

from run_frontier_recovery import stop_app
from run_integration import Suite, require, until
from run_component_lifecycle import Client, paths
from parrot_protocol import wire

ROOT = Path(__file__).resolve().parents[3]


def ask(client, key, body=b'', *, path='/user/search'):
    client.sequence += 1
    client.socket.sendall(wire.build_frame(
        wire.FT_ASK, client.sequence, path, 'bin:ws/' + key,
        wire.with_reply_to_prefix('/observer/reply', body)))
    while True:
        frame = client.decoder.next_frame()
        if frame is None:
            chunk = client.socket.recv(65536)
            require(chunk, 'observer disconnected before reply')
            client.decoder.feed(chunk)
            continue
        if frame.ft == wire.FT_HEARTBEAT:
            client.socket.sendall(wire.build_frame(wire.FT_HEARTBEAT_ACK, frame.cid, '', '', b''))
            continue
        require(frame.cid == client.sequence and frame.ft == wire.FT_REPLY, f'business receipt: {frame}')
        return frame.payload


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    options = parser.parse_args()
    directory = Path(tempfile.mkdtemp(prefix='ws-hist-', dir='/tmp'))
    binary = ROOT / 'target/debug/websearch'
    report = dict(status='failed', runtime_root=str(directory), cases=[],
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    print('Artifacts:', directory, flush=True)
    try:
        with ExitStack() as stack:
            suite = Suite(directory, binary, stack)
            data = directory / 'data'

            def crawl(path, pages=1):
                gateways, ports = suite.gateways(data)
                app, port = suite.app(data, ports, suite.corpus.base + path, '--keep',
                                      '--pages', str(pages), '--sites', '1000', '--maxdepth', '0')
                return gateways, ports, app, port

            def retire(gateways, app):
                stop_app(app)
                for gateway in gateways:
                    gateway.stop()

            gateways, _, app, port = crawl('/extra')
            suite.completed(app, port, gateways, 1)
            retire(gateways, app)
            backup = directory / 'old-backup'
            shutil.copytree(data / 'index', backup)

            gateways, _, app, port = crawl('/child')
            suite.completed(app, port, gateways, 2)
            suite.assert_search(port, ['/extra', '/child'])
            retire(gateways, app)
            current = directory / 'original-completed-index'
            (data / 'index').rename(current)
            checkpoint = json.loads((data / 'crawl.journal').read_text().splitlines()[-1])['record']['Complete']['checkpoint']
            acknowledged = checkpoint['generation']
            counts = dict(suite.corpus.requests)

            for advance in (0, 1):
                shutil.copytree(backup, data / 'index')
                gateways, ports = suite.gateways(data, jvm_only=True)
                app, port = suite.app(data, ports, '--serve-only')
                require(suite.ready(app, port, gateways)['docs'] == 1, 'backup must actually lack the completed page')
                with ExitStack() as observer:
                    client = Client(observer, ports['jvm'])
                    state = ask(client, 'CrawlState', path=paths(app)['jvm'])
                    while int.from_bytes(state[16:24], 'little') < acknowledged + advance:
                        require(ask(client, 'Flush', path=paths(app)['jvm']) == b'\x01\x00\x00\x00', 'real durable Flush required')
                        state = ask(client, 'CrawlState', path=paths(app)['jvm'])
                    require(list(state[:16]) == checkpoint['epoch'], 'fork must retain the same epoch')
                retire(gateways, app)

                gateways, _, app, port = crawl('/child')
                until(lambda: app.child.poll() is not None or '爬取收尾完成' in app.text(), gateways)
                case = dict(name='same epoch fork with ' + ('equal' if advance == 0 else 'higher') + ' generation',
                            acknowledged_generation=acknowledged, remote_generation=int.from_bytes(state[16:24], 'little'),
                            exit=app.child.poll(), requests=dict(suite.corpus.requests))
                report['cases'].append(case)
                if app.child.poll() is None:
                    case['incorrectly_accepted_health'] = suite.ready(app, port, gateways)
                require(app.child.poll() is not None and app.child.returncode != 0,
                        'forked history accepted completed tasks despite missing document: ' + str(case))
                require('history mismatch' in app.text(), 'rejection must be a history validation failure: ' + app.text())
                require(dict(suite.corpus.requests) == counts, 'history rejection performed HTTP effects')
                for gateway in gateways:
                    gateway.stop()
                (data / 'index').rename(directory / f'fork-{advance}')
                print('PASS ' + case['name'], flush=True)

            shutil.copytree(current, data / 'index')
            gateways, _, app, port = crawl('/child')
            suite.completed(app, port, gateways, 2)
            suite.assert_search(port, ['/extra', '/child'])
            require(dict(suite.corpus.requests) == counts, 'matching history refetched completed pages')
            retire(gateways, app)
            report['cases'].append(dict(name='matching history restored; completed pages not fetched'))

            # A v1 journal's checksums exclude the newly optional digest field.
            # Recreate that exact legacy encoding, retaining every URL and depth.
            journal = data / 'crawl.journal'
            shutil.copy2(journal, directory / 'journal-before-legacy-migration.saved')
            previous = bytes(32)
            legacy = []
            compact = lambda value: json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode()
            for line in journal.read_text().splitlines():
                if line == 'PRWSJ2':
                    continue
                record = json.loads(line)['record']
                if record == 'Seal':
                    continue
                require(isinstance(record, dict) and set(record) <= {'Reset','Begin','Discover','Complete','AdoptHistory'}, 'legacy fixture must emit only version-1 records')
                for fields in record.values():
                    if 'checkpoint' in fields:
                        fields['checkpoint'].pop('digest', None)
                digest = hashlib.sha256(previous + compact(record)).digest()
                legacy.append(compact(dict(previous=list(previous), record=record, checksum=list(digest))))
                previous = digest
            journal.write_bytes(b'\n'.join(legacy) + b'\n')
            gateways, _, app, port = crawl('/child', pages=2)
            suite.completed(app, port, gateways, 2)
            require('legacy crawl journal migration' in app.text(), 'legacy completion proof was silently trusted')
            require(suite.corpus.requests['/extra'] == counts['/extra'] + 1 and
                    suite.corpus.requests['/child'] == counts['/child'] + 1,
                    'legacy migration must replay both completed URLs')
            suite.assert_search(port, ['/extra', '/child'])
            retire(gateways, app)
            report['cases'].append(dict(name='legacy journal completions conservatively replayed', requests=dict(suite.corpus.requests)))
            migrated_counts = dict(suite.corpus.requests)
            gateways, _, app, port = crawl('/child')
            suite.completed(app, port, gateways, 2)
            require(dict(suite.corpus.requests) == migrated_counts, 'completed migration was repeated after restart')
            retire(gateways, app)
            report['cases'].append(dict(name='migrated history persists across fresh gateways'))
        report['status'] = 'passed'
    except BaseException as error:
        report['error'] = str(error)
        raise
    finally:
        body = json.dumps(report, indent=2) + '\n'
        (directory / 'result.json').write_text(body)
        if options.report:
            options.report.write_text(body)


if __name__ == '__main__':
    main()
