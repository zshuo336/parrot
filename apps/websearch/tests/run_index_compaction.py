#!/usr/bin/env python3
"""Real index capacity/compaction contracts with an independent legacy fixture."""
from contextlib import ExitStack
from pathlib import Path
import argparse, hashlib, io, json, os, signal, struct, tarfile, tempfile, zlib
from run_integration import Suite, require, until, REPO
from run_frontier_recovery import stop_app
from run_journal_compaction import file_hash, encoded
from run_component_lifecycle import Client, paths
from parrot_protocol import wire


def checkpoint(client, key, body=b'', *, path):
    client.sequence += 1
    client.socket.sendall(wire.build_frame(wire.FT_ASK, client.sequence, path, 'bin:ws/' + key,
                                          wire.with_reply_to_prefix('/observer/reply', body)))
    while True:
        frame = client.decoder.next_frame()
        if frame is None:
            chunk = client.socket.recv(65536)
            require(chunk, 'observer disconnected before checkpoint receipt')
            client.decoder.feed(chunk)
            continue
        if frame.ft == wire.FT_HEARTBEAT:
            client.socket.sendall(wire.build_frame(wire.FT_HEARTBEAT_ACK, frame.cid, '', '', b''))
            continue
        expected = 'bin:ws/' + ('CrawlStateV2R' if key == 'CrawlStateV2' else 'CheckpointV2Ack')
        require(frame.ft == wire.FT_REPLY and frame.cid == client.sequence and frame.type_key == expected
                and len(frame.payload) == 56, 'invalid history receipt: ' + str(frame))
        return frame.payload

LEGACY_GENERATIONS = 100000


def payload(path):
    raw = path.read_bytes()
    require(raw[:4] == b'PRWS' and raw[4:7] == b'IDX', 'fixture needs a modern actual snapshot')
    require(zlib.crc32(raw[:-4]) == struct.unpack('>I', raw[-4:])[0], 'actual snapshot CRC mismatch')
    version = raw[7:8]
    require(version in (b'3', b'4', b'5'), 'fixture needs an actual crawl epoch')
    offset = 24
    if version == b'4':
        offset = 64
    elif version == b'5':
        count = struct.unpack('>I', raw[32:36])[0]
        require(count <= LEGACY_GENERATIONS, 'unbounded ancestry fixture')
        offset = 36 + 40 * count
    require(offset + 8 <= len(raw) - 4, 'snapshot body truncated')
    return raw[8:24], raw[offset:-4]


def full_legacy_index(data, root):
    index = data / 'index'
    latest = max(index.glob('seg-*.segment'), key=lambda p: int(p.stem[4:]))
    epoch, body = payload(latest)
    index.rename(root / 'index-before-fixture')
    index.mkdir()
    previous = bytes(32)
    chain = hashlib.sha256()
    total = 0
    archive = root / 'legacy-index-input.tar.gz'
    with tarfile.open(archive, 'w:gz') as saved:
        for generation in range(LEGACY_GENERATIONS):
            content = b'PRWSIDX4' + epoch + struct.pack('>q', generation - 1) + previous + body
            raw = content + struct.pack('>I', zlib.crc32(content))
            name = f'seg-{generation:020d}.segment'
            (index / name).write_bytes(raw)
            previous = hashlib.sha256(raw).digest()
            chain.update(struct.pack('>Q', generation) + previous)
            total += len(raw)
            member = tarfile.TarInfo(name)
            member.size = len(raw)
            saved.addfile(member, io.BytesIO(raw))
    return dict(archive=str(archive), archive_sha256=file_hash(archive),
                count=LEGACY_GENERATIONS, total_bytes=total, latest_sha256=previous.hex(),
                chain_sha256=chain.hexdigest(), epoch=list(epoch))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=REPO / 'target/debug/websearch')
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--only-legacy-migration', action='store_true')
    options = parser.parse_args()
    binary = options.binary.resolve()
    root = Path(tempfile.mkdtemp(prefix='ws-index-gc-', dir='/tmp'))
    report = dict(status='failed', directory=str(root), cases=[],
                  scope='legacy-migration' if options.only_legacy_migration else 'full',
                  binary_sha256=file_hash(binary),
                  jvm_sha256=file_hash(REPO / 'apps/websearch/jvm/target/websearch-jvm-1.0.0.jar'))
    print('INDEX_COMPACTION_ARTIFACTS=' + str(root), flush=True)
    try:
        with ExitStack() as stack:
            suite = Suite(root, binary, stack)
            data = root / 'data'
            gateways, ports = suite.gateways(data)
            app, port = suite.app(data, ports, suite.corpus.base + '/extra',
                                  '--pages', '1', '--sites', '1000', '--maxdepth', '0')
            suite.completed(app, port, gateways, 1)
            suite.assert_search(port, ['/extra'])
            stop_app(app)
            for gateway in gateways:
                gateway.stop()
            report['legacy_input'] = full_legacy_index(data, root)
            print('FIXTURE full legal version4 index: 100000 generations', flush=True)
            # Match a completed legacy journal to the actual generated ancestry.
            # No task is lost or invented merely to make migration acceptable.
            journal = data / 'crawl.journal'
            (root / 'journal-before-legacy-fixture.saved').write_bytes(journal.read_bytes())
            state = lambda n: dict(epoch=report['legacy_input']['epoch'], generation=n,
                                   digest=list(bytes.fromhex(file_hash(data/'index'/f'seg-{n:020d}.segment'))))
            previous, begin = encoded(bytes(32), dict(Begin=dict(version=1,
                checkpoint=state(LEGACY_GENERATIONS-2), seeds=[[suite.corpus.base+'/extra',0]], scope=[suite.corpus.base])))
            _, complete = encoded(previous, dict(Complete=dict(checkpoint=state(LEGACY_GENERATIONS-1),
                                                                urls=[suite.corpus.base+'/extra'])))
            journal.write_bytes(begin + complete)
            gateways, ports = suite.gateways(data)
            app, port = suite.app(data, ports, suite.corpus.base + '/extra', '--keep',
                                  '--pages', '1', '--sites', '1000', '--maxdepth', '0')
            suite.completed(app, port, gateways, 1)
            suite.assert_search(port, ['/extra'])
            def reclaimed():
                current = suite.ready(app, port, gateways)
                return current if current['segments'] == 1 else None
            stats = until(reclaimed, [app, *gateways])
            stop_app(app)
            for gateway in gateways:
                gateway.stop()
            suite.corpus.assert_requests({'/robots.txt': 1, '/extra': 1})
            report['cases'].append(dict(name='keep migrates the complete legacy chain at the real generation cap',stats=stats))
            print('PASS keep migration at real legacy capacity', flush=True)
            if options.only_legacy_migration:
                report['status'] = 'passed'
                return
            (data/'index').rename(root/'migrated-cap-index.saved')
            (data/'index').mkdir()
            # Restore the exact saved legacy input to test fresh admission too.
            require(file_hash(Path(report['legacy_input']['archive'])) == report['legacy_input']['archive_sha256'],
                    'legacy input archive changed')
            with tarfile.open(report['legacy_input']['archive']) as archive:
                count = 0
                for member in archive:
                    require(member.isfile() and member.name == f'seg-{count:020d}.segment', 'unsafe legacy fixture member')
                    (data/'index'/member.name).write_bytes(archive.extractfile(member).read())
                    count += 1
            require(count == LEGACY_GENERATIONS, 'legacy fixture restoration count mismatch')
            gateways, ports = suite.gateways(data)
            app, port = suite.app(data, ports, suite.corpus.base + '/root',
                                  '--pages', '1', '--sites', '1000', '--maxdepth', '0')
            stats = suite.completed(app, port, gateways, 1)
            scores = suite.assert_search(port, ['/root'])
            stats = until(reclaimed, [app, *gateways])
            stop_app(app)
            files = list((data / 'index').glob('*.segment'))
            require(len(files) == 1 and stats['segments'] == 1, 'superseded full snapshots were not reclaimed')
            suite.corpus.assert_requests({'/robots.txt': 2, '/extra': 1, '/root': 1})
            report['cases'].append(dict(name='fresh crawl at real legacy generation cap', stats=stats, scores=scores))
            print('PASS fresh crawl replaces a full legacy index', flush=True)

            app, port = suite.app(data, ports, '--serve-only')
            suite.ready(app, port, gateways)
            with ExitStack() as observer:
                client = Client(observer, ports['jvm'])
                address = paths(app)['jvm']
                anchor = checkpoint(client, 'CrawlStateV2', path=address)
                current = anchor
                for _ in range(128):
                    newer = checkpoint(client, 'CheckpointV2', current, path=address)
                    require(newer[:16] == anchor[:16] and int.from_bytes(newer[16:24], 'little') ==
                            int.from_bytes(current[16:24], 'little') + 1, 'checkpoint did not commit exactly once')
                    current = newer
                require(checkpoint(client, 'CrawlStateV2', anchor, path=address) == current,
                        'old acknowledged checkpoint was forgotten')
            until(reclaimed, [app, *gateways])
            require(suite.assert_search(port, ['/root']) == scores, 'checkpoint-only commits changed search results')
            stop_app(app)
            for gateway in gateways:
                gateway.stop()
            gateways, ports = suite.gateways(data)
            app, port = suite.app(data, ports, suite.corpus.base + '/root', '--keep',
                                  '--pages', '1', '--sites', '1000', '--maxdepth', '0')
            suite.completed(app, port, gateways, 1)
            require(suite.assert_search(port, ['/root']) == scores, 'fresh JVM replay changed search results')
            until(reclaimed, [app, *gateways])
            stop_app(app)
            for gateway in gateways:
                gateway.stop()
            suite.corpus.assert_requests({'/robots.txt': 2, '/extra': 1, '/root': 1})
            report['cases'].append(dict(name='128 checkpoints retain the old ancestor through fresh JVM replay',
                                        anchor=anchor.hex(), latest=current.hex(), requests=dict(suite.corpus.requests)))
            print('PASS compact checkpoint history across fresh JVM replay', flush=True)

            # Restore only obsolete owned files alongside the current valid head.
            # The body source remains outside index: deleting links must not alter it.
            index = data / 'index'
            head = next(index.glob('*.segment')); head_hash = file_hash(head)
            source = root / 'obsolete-source.saved'
            with tarfile.open(report['legacy_input']['archive']) as archive:
                member = archive.next()
                require(member.isfile() and member.name == 'seg-00000000000000000000.segment', 'invalid saved fixture')
                source.write_bytes(archive.extractfile(member).read())
            source_hash = file_hash(source)
            # Keep every inode below Linux ext4's hardlink limit. The fixture
            # still contains the full 100000 obsolete directory entries.
            sources = [source]
            for generation in range(LEGACY_GENERATIONS):
                if generation and generation % 10000 == 0:
                    saved = root / f'obsolete-source-{len(sources)}.saved'
                    saved.write_bytes(source.read_bytes())
                    sources.append(saved)
                os.link(sources[-1], index / f'seg-{generation:020d}.segment')
            first = index / 'seg-00000000000000000000.segment'
            last = index / f'seg-{LEGACY_GENERATIONS - 1:020d}.segment'
            gateways, ports = suite.gateways(data, jvm_only=True)
            app, port = suite.app(data, ports, '--serve-only')
            until(lambda: not first.exists() and last.exists(), [app, *gateways])
            jvm = gateways[0]
            jvm.child.send_signal(signal.SIGKILL); jvm.child.wait(timeout=10)
            require(jvm.child.returncode == -signal.SIGKILL, 'actual JVM was not killed during reclamation')
            remaining = sum(int(path.stem[4:]) < LEGACY_GENERATIONS for path in index.glob('*.segment'))
            require(0 < remaining < LEGACY_GENERATIONS, 'test did not interrupt partial reclamation')
            require(head.exists() and file_hash(head) == head_hash, 'reclamation removed or changed authoritative head')
            app.stop(); jvm.stop()
            gateways, ports = suite.gateways(data, jvm_only=True)
            app, port = suite.app(data, ports, '--serve-only')
            stats = until(reclaimed, [app, *gateways])
            require(stats['docs'] == 1 and suite.assert_search(port, ['/root']) == scores,
                    'interrupted reclamation changed the published index')
            stop_app(app)
            require(len(list(index.glob('*.segment'))) == 1 and file_hash(head) == head_hash,
                    'restart did not preserve the exact head and finish reclamation')
            require(all(file_hash(saved) == source_hash for saved in sources),
                    'reclamation modified a retained hardlink source')
            suite.corpus.assert_requests({'/robots.txt': 2, '/extra': 1, '/root': 1})
            report['cases'].append(dict(name='SIGKILL during physical reclamation resumes from the same durable head',
                                        obsolete_remaining_after_kill=remaining, head_sha256=head_hash, stats=stats))
            print('PASS actual JVM kill during partial reclamation and exact restart', flush=True)
        report['status'] = 'passed'
    except Exception as error:
        report['error'] = str(error)
        raise
    finally:
        body = json.dumps(report, indent=2) + '\n'
        options.report.write_text(body)
        (root / 'result.json').write_text(body)


if __name__ == '__main__':
    main()
