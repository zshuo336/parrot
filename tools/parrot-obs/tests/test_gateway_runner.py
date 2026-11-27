import json
from pathlib import Path
import queue
import socket
import sys
import tempfile
import unittest

from run_gateways import OwnedGateways, check_watch_samples, manager

FIXTURE=Path(__file__).with_name('gateway_fixture.py')


class WatchSamples(unittest.TestCase):
    nodes = ('erl-gw-1', 'jvm-search-1', 'ray-gw-1')

    def check_rounds(self, first, second):
        ready = queue.Queue()
        for tick, rows in enumerate((first, second)):
            for name, rate in rows:
                ready.put(f'{tick}.0s] {name}: asks/s={rate} tells/s=0.00')
        ready.put(None)
        check_watch_samples(ready, self.nodes)

    def test_both_rounds_cover_all_nodes_in_any_order(self):
        self.check_rounds([(n, 'n/a') for n in self.nodes], [(n, '0.00') for n in reversed(self.nodes)])

    def test_missing_duplicate_or_unmeasured_second_sample_fails(self):
        first = [(n, 'n/a') for n in self.nodes]
        second = [(n, '0.00') for n in self.nodes]
        for bad in [second[:-1], second[:2] + [second[0]], second[:2] + [(self.nodes[2], 'n/a')]]:
            with self.subTest(second=bad), self.assertRaises(AssertionError):
                self.check_rounds(first, bad)

    def test_first_round_must_establish_a_baseline(self):
        with self.assertRaises(AssertionError):
            self.check_rounds([(n, '0.00') for n in self.nodes], [(n, '0.00') for n in self.nodes])


class RunnerOwnership(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix='pg-run-',dir='/tmp')
        self.directory=Path(self.temp.name)
        self.config={'directory':str(self.directory),'startup_timeout':5.0,'gateways':[
            dict(name=name,node=name+'-custom',port=0,marker='READY=',cwd=str(FIXTURE.parent),env={},
                 command=[sys.executable,str(FIXTURE),'--mode','child'])
            for name in ['erl','jvm','ray']]}

    def tearDown(self):
        manager.stop(self.directory)
        self.temp.cleanup()

    def assert_retired(self,ports):
        self.assertFalse(manager.owner_active(self.directory))
        self.assertEqual(json.loads((self.directory/'state.json').read_text())['phase'],'stopped')
        for port in ports.values():
            with socket.socket() as listener:
                listener.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
                listener.bind(('127.0.0.1',port))

    def test_close_retires_descendants_even_after_direct_gateways_exit_on_term(self):
        with OwnedGateways(self.config) as gateways:
            ports=gateways.snapshot['ports']
            self.assertEqual(gateways.nodes(),[f'{name}@{name}-custom=127.0.0.1:{ports[name]}' for name in ['erl','jvm','ray']])
            gateways.close()
            self.assert_retired(ports)
        self.assert_retired(ports)

    def test_later_probe_failure_preserves_error_and_retires_every_gateway_group(self):
        with self.assertRaisesRegex(RuntimeError,'business probe failed'):
            with OwnedGateways(self.config) as gateways:
                ports=gateways.snapshot['ports']
                raise RuntimeError('business probe failed')
        self.assert_retired(ports)


if __name__=='__main__':unittest.main()
