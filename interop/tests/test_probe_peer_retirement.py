#!/usr/bin/env python3
"""Real TCP EOF/reset must end a fixture peer, not the owned gateway.

Exercise the exact fixture used by the public command contracts. Protocol
errors remain fatal; this is not a retry or an exception-swallowing test.
"""
from pathlib import Path
import json
import os
import socket
import struct
import sys
import tempfile
import time
import unittest

from probe_lifecycle import FIXTURE, ROOT

sys.path.insert(0, str(ROOT / 'apps/test_support'))
sys.path.insert(0, str(ROOT / 'interop/python'))
from owned_process import OwnedProcess, process_exited
from parrot_protocol.wire import FrameDecoder, build_frame, with_reply_to_prefix, FT_ASK, FT_REPLY


class PeerRetirement(unittest.TestCase):
    evidence = None

    def wait_for(self, predicate, process, log):
        deadline = time.monotonic() + 5
        while not predicate():
            self.assertIsNone(process.poll(), log.read_text())
            self.assertLess(time.monotonic(), deadline, log.read_text())
            time.sleep(0.01)

    def exercise(self, ending):
        with tempfile.TemporaryDirectory(prefix='probe-peer-') as temporary:
            root = Path(temporary) if self.evidence is None else self.evidence / ending
            root.mkdir(parents=True, exist_ok=True)
            record, closed, requests = [root / n for n in ('pids.json', 'closed.json', 'requests.json')]
            log = root / 'gateway.log'
            env = dict(os.environ, PROBE_MODE='success', PROBE_MARKER='READY=',
                       PROBE_PID_FILE=str(record), PROBE_REQUESTS=str(requests),
                       PROBE_PEER_CLOSED=str(closed), PARROT_NODE_ID='ray-gw-1',
                       PROBE_WIRE_ROOT=str(ROOT / 'interop/python'))
            with log.open('xb') as output, OwnedProcess(
                    [sys.executable, '-u', '-c', FIXTURE], env=env,
                    stdout=output, stderr=output) as process:
                self.wait_for(record.exists, process, log)
                state = json.loads(record.read_text())
                with socket.create_connection(('127.0.0.1', state['port']), timeout=3) as peer:
                    peer.settimeout(3)
                    decoder = FrameDecoder()
                    # The fixture tests lifecycle, not handshake admission.
                    # The production CLI contracts separately exercise handshake.
                    if ending == 'malformed':
                        peer.sendall(build_frame(FT_ASK, 1, 'actor', 'bin:unknown', with_reply_to_prefix('', b'')))
                        self.assertNotEqual(process.wait(timeout=5), 0)
                        self.assertIn('unexpected request type', log.read_text())
                        self.assertFalse(closed.exists(), 'protocol failure must not become a clean disconnect')
                    else:
                        for cid, key, payload, expected in [
                            (1, 'bin:u:Ping', struct.pack('<Q', 100), 102),
                            (2, 'bin:u:Add', struct.pack('<QQ', 3, 4), 1007),
                        ]:
                            peer.sendall(build_frame(FT_ASK, cid, 'actor', key, with_reply_to_prefix('', payload)))
                            frame = None
                            while frame is None:
                                data = peer.recv(65536)
                                self.assertTrue(data, 'fixture ended before replying')
                                decoder.feed(data)
                                frame = decoder.next_frame()
                            self.assertEqual(frame.ft, FT_REPLY)
                            self.assertEqual(frame.cid, cid)
                            self.assertEqual(frame.payload, struct.pack('<Q', expected))
                        if ending == 'reset':
                            peer.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
                            peer.close()
                        else:
                            peer.shutdown(socket.SHUT_WR)
                            self.assertEqual(peer.recv(1), b'', 'fixture must close its peer on EOF')
                        if ending == 'reset':
                            self.wait_for(closed.exists, process, log)
                            receipt = json.loads(closed.read_text())
                            self.assertEqual(receipt['reason'], ending)
                            self.assertEqual(receipt['requests'], 2)
                        self.assertIsNone(process.poll(), 'peer disconnect must not retire the gateway owner')
                        self.assertFalse(process_exited(state['child']))
                # OwnedProcess performs the group cleanup even for the malformed
                # case, whose fixture parent exited while its child stayed alive.
            deadline = time.monotonic() + 5
            while not all(process_exited(state[k]) for k in ('pid', 'child')):
                self.assertLess(time.monotonic(), deadline, 'owned fixture group did not retire')
                time.sleep(0.01)

    def test_eof_preserves_gateway_until_owner_retires_it(self):
        self.exercise('eof')

    def test_reset_preserves_gateway_until_owner_retires_it(self):
        self.exercise('reset')

    def test_protocol_error_remains_fatal(self):
        self.exercise('malformed')


def run_contracts(evidence):
    PeerRetirement.evidence = Path(evidence)
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(PeerRetirement)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        raise RuntimeError('fixture peer retirement contracts failed')


if __name__ == '__main__':
    if os.environ.get('PARROT_PEER_EVIDENCE'):
        PeerRetirement.evidence = Path(os.environ['PARROT_PEER_EVIDENCE'])
    unittest.main()
