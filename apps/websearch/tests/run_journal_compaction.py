#!/usr/bin/env python3
"""Actual 256 MiB journal admission, atomic replacement and post-kill recovery.

The independent fixture emits valid legacy checksum chains with redundant reset
intents and legal JSON whitespace. No product budget or timing is relaxed.
"""
from contextlib import ExitStack
from pathlib import Path
import argparse, hashlib, json, os, signal, tempfile
from run_integration import Suite, require, until, REPO
from run_frontier_recovery import stop_app
MAX_LOG = 256 * 1024 * 1024
MAX_RECORD = 2 * 1024 * 1024


def file_hash(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def encoded(previous, record):
    raw = json.dumps(record, ensure_ascii=False, separators=(',', ':')).encode()
    checksum = hashlib.sha256(previous + raw).digest()
    frame = dict(previous=list(previous), record=record, checksum=list(checksum))
    return checksum, json.dumps(frame, ensure_ascii=False, separators=(',', ':')).encode() + b'\n'


def state(path):
    checkpoint, tasks, initial = None, {}, None
    previous = bytes(32)
    for line in path.read_bytes().splitlines(keepends=True):
        if line == b'PRWSJ2\n':
            continue
        frame = json.loads(line)
        digest, _ = encoded(previous, frame['record'])
        require(frame['previous'] == list(previous) and frame['checksum'] == list(digest), 'independent journal checksum mismatch')
        previous = digest
        kind, value = next(iter(frame['record'].items())) if isinstance(frame['record'], dict) else (frame['record'], {})
        if kind == 'Begin':
            checkpoint = value['checkpoint']; initial = initial or checkpoint
            tasks = {url: dict(url=url, depth=depth, complete=False) for url, depth in value['seeds']}
        elif kind == 'Discover':
            tasks.update((url, dict(url=url, depth=depth, complete=False)) for url, depth in value['tasks'])
        elif kind == 'Complete':
            checkpoint = value['checkpoint']
            for url in value['urls']: tasks[url]['complete'] = True
        elif kind == 'Snapshot':
            checkpoint = value['checkpoint']; tasks = {}
        elif kind == 'RestoreTasks':
            tasks.update((entry['url'], entry) for entry in value['tasks'])
        elif kind not in ('Reset', 'Seal'):
            raise AssertionError('unexpected journal record: ' + kind)
    require(checkpoint is not None, 'missing real checkpoint')
    return checkpoint, list(tasks.values()), initial


def expand(data, root, label, initial, origin):
    path = data / 'crawl.journal'
    checkpoint, tasks, _ = state(path)
    require(checkpoint['epoch'] == initial['epoch'] and checkpoint['generation'] > initial['generation'], 'fixture needs real same-lineage checkpoints')
    seeds = [[task['url'], task['depth']] for task in tasks]
    records = [dict(Begin=dict(version=1, checkpoint=initial, seeds=seeds, scope=[origin])),
               dict(Complete=dict(checkpoint=checkpoint, urls=[task['url'] for task in tasks if task['complete']]))]
    reset = dict(Reset=dict(seeds=seeds, scope=[origin]))
    target = MAX_LOG - 1
    previous = bytes(32)
    temporary = data / 'fixture-next'
    with temporary.open('xb') as output:
        # Reserve one full frame plus a bounded final state tail.
        while output.tell() + 2 * MAX_RECORD < target:
            previous, frame = encoded(previous, reset)
            require(len(frame) <= MAX_RECORD, 'fixture record too large')
            output.write(frame[:-1] + b' ' * (MAX_RECORD - len(frame)) + b'\n')
        next_digest, frame = encoded(previous, reset)
        tail = []
        for record in records:
            next_digest, line = encoded(next_digest, record); tail.append(line)
        # Fill any extra space with a second complete, independently chained frame.
        remaining = target - output.tell() - len(frame) - sum(map(len, tail))
        if len(frame) + remaining > MAX_RECORD:
            previous, pad = encoded(previous, reset)
            output.write(pad[:-1] + b' ' * (MAX_RECORD - len(pad)) + b'\n')
            next_digest, frame = encoded(previous, reset); tail = []
            for record in records:
                next_digest, line = encoded(next_digest, record); tail.append(line)
            remaining = target - output.tell() - len(frame) - sum(map(len, tail))
        require(0 <= remaining and len(frame) + remaining <= MAX_RECORD, 'invalid fixture padding')
        output.write(frame[:-1] + b' ' * remaining + b'\n')
        for line in tail: output.write(line)
        output.flush(); os.fsync(output.fileno())
    os.replace(temporary, path)
    require(path.stat().st_size == target, 'fixture did not reach the production limit')
    retained = root / (label + '-before.journal')
    os.link(path, retained)
    digest = file_hash(path)
    return dict(path=str(retained), sha256=digest, bytes=target, checkpoint=checkpoint, tasks=tasks)


def check_replaced(data, evidence):
    retained = Path(evidence['path']); current = data / 'crawl.journal'
    require(file_hash(retained) == evidence['sha256'], 'compaction rewrote the published journal in place')
    require(current.stat().st_ino != retained.stat().st_ino, 'journal was not atomically replaced')
    require(current.stat().st_size < 65536, 'redundant journal history was not reclaimed')
    require(current.read_bytes().startswith(b'PRWSJ2\n'), 'missing explicit compact journal format')
    state(current)  # Verify the full resulting checksum chain independently.


def single_child_corpus(suite):
    owner = suite.corpus
    base_handler = owner.server.RequestHandlerClass
    class Handler(base_handler):
        def do_GET(self):
            if self.path != '/root':
                return super().do_GET()
            with owner.lock: owner.requests[self.path] += 1
            body = ('<html><title>alpha ' + suite.token + '</title><body>' + suite.token +
                    '<a href="/child">child</a></body></html>').encode()
            self.send_response(200);self.send_header('Content-Type','text/html')
            self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
    owner.server.RequestHandlerClass = Handler


def oversized_reset(data, origin):
    # A valid old Reset fits, but its Begin would exceed the unchanged frame cap.
    seeds = [[origin + '/' + str(n) + '/' + 'x' * 8100, 0] for n in range(257)]
    seeds.append([origin + '/tail', 0])
    record = dict(Reset=dict(seeds=seeds, scope=[origin]))
    worst = dict(previous=[0]*32, record=record, checksum=[255]*32)
    size = len(json.dumps(worst,separators=(',',':')).encode())+1
    padding = MAX_RECORD-size-16
    require(0 <= padding and len(seeds[-1][0])+padding <= 8192, 'bad reset boundary fixture')
    seeds[-1][0] += 'x'*padding
    _, frame = encoded(bytes(32),record)
    require(len(frame)<=MAX_RECORD, 'legacy reset must fit')
    path = data/'crawl.journal'
    (data/'journal-before-reset-budget.saved').write_bytes(path.read_bytes())
    path.write_bytes(frame)
    return dict(path=str(path), bytes=len(frame), sha256=file_hash(path))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=REPO / 'target/debug/websearch')
    parser.add_argument('--report', type=Path, required=True)
    args = parser.parse_args(); binary = args.binary.resolve()
    root = Path(tempfile.mkdtemp(prefix='ws-journal-', dir='/tmp'))
    report = dict(directory=str(root), binary_sha256=file_hash(binary), cases=[], inputs=[], status='failed')
    print('JOURNAL_COMPACTION_ARTIFACTS=' + str(root), flush=True)
    try:
        for mode in ['fresh', 'keep']:
            with ExitStack() as stack:
                directory = root / mode; directory.mkdir(); suite = Suite(directory, binary, stack); data = directory / 'data'
                single_child_corpus(suite)
                gateways, ports = suite.gateways(data)
                def start(seed, keep=False):
                    return suite.app(data, ports, suite.corpus.base + seed, '--pages','1','--sites','1000','--maxdepth','1', *(['--keep'] if keep else []))
                app, port = start('/root'); suite.completed(app, port, gateways, 1)
                suite.assert_search(port, ['/root']); stop_app(app)
                _, tasks, initial = state(data / 'crawl.journal')
                require([(t['url'],t['depth'],t['complete']) for t in tasks] == [(suite.corpus.base+'/root',0,True),(suite.corpus.base+'/child',1,False)], 'initial real pending/completed task evidence missing')
                evidence = expand(data, directory, mode, initial, suite.corpus.base); report['inputs'].append(evidence)
                app, port = start('/extra' if mode == 'fresh' else '/root', mode == 'keep')
                suite.completed(app, port, gateways, 1 if mode == 'fresh' else 2)
                expected = ['/extra'] if mode == 'fresh' else ['/root','/child']
                scores = suite.assert_search(port, expected); stop_app(app)
                check_replaced(data, evidence)
                if mode == 'keep':
                    # Reopen the exact published file before any fixture rewrite.
                    # In-memory state alone cannot prove the serialized flags.
                    app, port = start('/root', True)
                    suite.completed(app, port, gateways, 2)
                    require(suite.assert_search(port, expected) == scores,
                            'reopening compact journal changed existing results')
                    stop_app(app)
                suite.corpus.assert_requests({'/robots.txt':2, '/root':1, '/extra':1} if mode == 'fresh' else {'/robots.txt':2, '/root':1, '/child':1})
                report['cases'].append(dict(name='full legacy journal '+mode, scores=scores, bytes=(data/'crawl.journal').stat().st_size))
                print('PASS full legacy journal ' + mode, flush=True)
                if mode == 'keep':
                    evidence = expand(data, directory, 'kill', initial, suite.corpus.base); report['inputs'].append(evidence)
                    app, _ = start('/slow', True)
                    until(suite.corpus.slow_started.is_set, [app,*gateways])
                    app.child.send_signal(signal.SIGKILL);app.child.wait(timeout=10)
                    require(app.child.returncode == -signal.SIGKILL, 'did not kill actual crawler')
                    check_replaced(data, evidence)
                    suite.corpus.slow_release.set()
                    app, port = start('/slow', True);suite.completed(app, port, gateways, 3)
                    suite.assert_search(port, ['/root','/child','/slow']);stop_app(app)
                    require(suite.corpus.requests['/root']==1 and suite.corpus.requests['/child']==1 and suite.corpus.requests['/slow']==2, 'compaction lost completion or unconfirmed intent')
                    report['cases'].append(dict(name='kill after compaction preserves pending intent and completed tasks',requests=dict(suite.corpus.requests)))
                    print('PASS kill after compaction recovery',flush=True)
                    # Obtain an exact replay oracle before installing an old boundary intent.
                    app, port = suite.app(data,ports,'--serve-only')
                    suite.ready(app,port,gateways);before_scores=suite.assert_search(port,['/root','/child','/slow']);stop_app(app)
                    report['inputs'].append(oversized_reset(data,suite.corpus.base))
                    app,_=start('/root',True)
                    until(lambda: app.child.poll() is not None,gateways,seconds=90)
                    rejected_text=app.text();rejected_code=app.child.returncode
                    serve,port=suite.app(data,ports,'--serve-only')
                    stats=suite.ready(serve,port,gateways)
                    report['rejected_reset']=dict(exit=rejected_code,error=rejected_text,stats=stats)
                    require(stats['docs']==3,'index changed before rejecting an unrepresentable reset: '+str(stats))
                    require(suite.assert_search(port,['/root','/child','/slow'])==before_scores,'reset admission changed existing scores')
                    require(rejected_code==1 and 'next Begin' in rejected_text,'reset admission must reject before remote Clear')
                    stop_app(serve)
                    report['cases'].append(dict(name='old near-limit reset rejected before destructive Clear',stats=stats))
                    print('PASS reset record capacity protects existing index',flush=True)
        report['status']='passed'
    except Exception as error:
        report['error']=str(error)
        raise
    finally:
        args.report.write_text(json.dumps(report,indent=2)+'\n');(root/'result.json').write_text(json.dumps(report,indent=2)+'\n')

if __name__ == '__main__': main()
