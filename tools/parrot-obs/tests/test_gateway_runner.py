import json
from pathlib import Path
import socket
import sys
import tempfile
import unittest

from run_gateways import OwnedGateways, manager

FIXTURE=Path(__file__).with_name('gateway_fixture.py')


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
