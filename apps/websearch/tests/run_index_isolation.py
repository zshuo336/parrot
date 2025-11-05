"""Real shared-gateway applications must use their own durable index locations."""
from contextlib import ExitStack
from pathlib import Path
import argparse
import hashlib
import json
import signal
import tempfile

from run_component_lifecycle import Client, paths, intended_paths, status
from run_frontier_recovery import stop_app
from run_integration import Suite, require, until

ROOT = Path(__file__).resolve().parents[3]


def digest_tree(directory):
    return {str(p.relative_to(directory)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in directory.rglob('*') if p.is_file()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix='ws-index-isolation-', dir='/tmp'))
    binary = ROOT / 'target/debug/websearch'
    report = dict(status='failed', runtime_root=str(root), cases=[],
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  search_jar_sha256=hashlib.sha256((ROOT/'apps/websearch/jvm/target/websearch-jvm-1.0.0.jar').read_bytes()).hexdigest())
    print('Artifacts:', root, flush=True)
    try:
        with ExitStack() as stack:
            suite = Suite(root, binary, stack)
            fallback = root / 'gateway-default'
            gateways, ports = suite.gateways(fallback)
            # Independent TOML producer: literal strings preserve quotes and
            # backslashes. A regex must not silently substitute WS_DATA.
            literal = root / 'literal " 中文 back\\slash'
            with ExitStack() as observer:
                client = Client(observer, ports['jvm'])
                component = dict(name='config-owner', version='config-test',
                    artifact=dict(kind='jvm', main_class='websearch.search.SearchComponent', coords=None,
                                  uri=(ROOT/'apps/websearch/jvm/target/websearch-jvm-1.0.0.jar').as_uri()),
                    instances=dict(kind='singleton'),
                    config=("data_dir = '" + str(literal) + "'\n").encode())
                reply = client.admin('deploy', component=component)
                require(reply['kind'] == 'deployed', 'valid literal TOML deployment: ' + str(reply))
                case = dict(name='literal TOML selects exact directory', expected=str(literal),
                            actual_index_directories=[str(p) for p in root.rglob('index')])
                report['cases'].append(case)
                require((literal/'.search.lock').is_file() and (literal/'index').is_dir() and not (fallback/'index').exists(),
                        'component configuration silently selected a different index: ' + str(case))
                require(client.admin('stop', path_prefix='/user/config-owner')['kind']=='stopped', 'retire config fixture')
                for bad in [b'data_dri = "/tmp/wrong"', b'data_dir = 123', b'data_dir = "unterminated',
                            b'data_dir = "relative"', b'data_dir = "a"\ndata_dir = "b"', b'\xff']:
                    reply = client.admin('deploy', component=dict(component, config=bad))
                    require(reply['kind'] == 'failed', 'malformed configuration was accepted: ' + str(reply))
                require(not (fallback/'index').exists(), 'invalid explicit config fell back to WS_DATA')
                report['cases'].append(dict(name='invalid configuration rejected before default storage'))

            first_local, second_local = root/'journal-A', root/'journal-B'
            first_index = root/'remote A " quote \\ 中文'
            second_index = root/'remote B #?%'
            first, port_a = suite.app(first_local, ports, suite.corpus.base+'/child', '--index-data', str(first_index),
                                       '--pages','1','--sites','1','--maxdepth','0')
            suite.completed(first, port_a, gateways, 1)
            require((first_index/'index').is_dir() and not (fallback/'index').exists(),
                    'explicit CLI index was not applied to the real JVM deployment')
            expected_a = suite.assert_search(port_a, ['/child'])
            first_paths = paths(first)
            second, port_b = suite.app(second_local, ports, suite.corpus.base+'/extra', '--index-data', str(second_index),
                                        '--pages','1','--sites','1','--maxdepth','0')
            suite.completed(second, port_b, gateways, 1)
            expected_b = suite.assert_search(port_b, ['/extra'])
            require(suite.assert_search(port_a, ['/child']) == expected_a, 'second app changed first index')
            require(not (fallback/'index').exists(), 'applications used gateway fallback')
            for local, remote in [(first_local,first_index),(second_local,second_index)]:
                require((local/'crawl.journal').is_file(), 'local crawl journal missing')
                require(not (local/'index').exists(), 'remote index created under local journal')
                require(bool(list((remote/'index').glob('seg-*.segment'))), 'remote committed snapshot missing')
                require(not (remote/'crawl.journal').exists(), 'journal leaked to remote index directory')
            report['cases'].append(dict(name='two live applications share gateways with exact separate indexes',
                                        first=expected_a, second=expected_b))

            contender_data = root/'contender'
            contender, _ = suite.app(contender_data, ports, '--serve-only', '--index-data', str(first_index))
            until(lambda: contender.child.poll() is not None, gateways)
            require(contender.child.returncode != 0 and 'directory already has an owner' in contender.text(),
                    'same explicit index was not exclusively owned: '+contender.text())
            status(ports, dict(erl=False,ray=False,jvm=False), intended_paths(contender_data))
            require(suite.assert_search(port_a, ['/child']) == expected_a, 'contender changed first data')
            require(suite.assert_search(port_b, ['/extra']) == expected_b, 'contender changed second data')
            report['cases'].append(dict(name='same explicit index rejects contender without touching either owner'))

            first.child.send_signal(signal.SIGKILL); first.child.wait(timeout=10)
            require(first.child.returncode == -signal.SIGKILL, 'actual SIGKILL')
            replay, replay_port = suite.app(first_local, ports, '--serve-only')
            require(suite.ready(replay, replay_port, gateways)['docs']==1, 'persisted remote path not used')
            require(suite.assert_search(replay_port, ['/child']) == expected_a, 'SIGKILL replay changed first scores')
            require(suite.assert_search(port_b, ['/extra']) == expected_b, 'first recovery affected second')
            stop_app(replay)
            recovered, recovered_port = suite.app(first_local, ports, suite.corpus.base+'/child', '--keep', '--pages','1','--sites','1','--maxdepth','0')
            suite.completed(recovered, recovered_port, gateways, 1)
            require(suite.assert_search(recovered_port, ['/child']) == expected_a, 'full recovery changed first scores')
            status(ports, dict(erl=False,ray=False,jvm=False), first_paths)
            require(suite.corpus.requests['/child']==1 and suite.corpus.requests['/extra']==1, 'recovery repeated completed fetches')
            stop_app(recovered)
            require(suite.assert_search(port_b, ['/extra']) == expected_b, 'first retirement affected second')
            report['cases'].append(dict(name='SIGKILL and serve-only/full recovery reuse persisted index; sibling remains live'))

            before = digest_tree(first_local)
            for mode in ['--keep','--serve-only']:
                bad, _ = suite.app(first_local, ports, mode, '--index-data', str(second_index))
                until(lambda: bad.child.poll() is not None, gateways)
                require(bad.child.returncode != 0 and 'persisted remote index' in bad.text(), 'resume switched index: '+bad.text())
                require('[ws] deploy ' not in bad.text() and '[ws] connected:' not in bad.text(), 'mismatch reached remote services')
                require(digest_tree(first_local)==before, 'mismatch changed journal or deployment intents')
            require(suite.assert_search(port_b, ['/extra'])==expected_b, 'mismatch affected sibling')
            report['cases'].append(dict(name='resume rejects explicit index mismatch before remote side effects'))
            old_index = digest_tree(first_index)
            new_index = root/'explicit-cold-index'
            fresh, fresh_port = suite.app(first_local, ports, suite.corpus.base+'/root', '--index-data',str(new_index),
                                          '--pages','1','--sites','1','--maxdepth','0')
            suite.completed(fresh, fresh_port, gateways, 1)
            expected_fresh = suite.assert_search(fresh_port, ['/root'])
            require(digest_tree(first_index)==old_index, 'explicit cold selection modified the old index')
            require(suite.assert_search(port_b, ['/extra'])==expected_b, 'cold selection affected sibling')
            stop_app(fresh)
            report['cases'].append(dict(name='explicit fresh selection changes persisted target and preserves old index'))
            stop_app(second)
            for gateway in gateways: gateway.stop()
            gateways, ports = suite.gateways(fallback, jvm_only=True)
            for local, expected, urls, options in [
                (first_local,expected_fresh,['/root'],[]),
                (second_local,expected_b,['/extra'],[]),
                (root/'original-reader',expected_a,['/child'],['--index-data',str(first_index)])]:
                replay, port = suite.app(local, ports, '--serve-only', *options)
                require(suite.ready(replay, port, gateways)['docs']==1, 'JVM restart lost index')
                require(suite.assert_search(port, urls)==expected, 'JVM restart changed exact scores')
                stop_app(replay)
            require(not (fallback/'index').exists(), 'restart used fallback')
            report['cases'].append(dict(name='fresh JVM process replays persisted selections and the preserved old index'))
        report['status']='passed'
        print('PASS remote index isolation', flush=True)
    except BaseException as error:
        report['error']=str(error)
        raise
    finally:
        body=json.dumps(report,indent=2,ensure_ascii=False)+'\n'
        (root/'result.json').write_text(body)
        if args.report: args.report.write_text(body)


if __name__ == '__main__':
    main()
