#!/usr/bin/env python3
"""Apply the peer/owner boundary contracts to the actual JVM command fixture."""
from pathlib import Path
import os
import unittest

from matrix_lifecycle import FIXTURE
import test_probe_peer_retirement as retirement


class MatrixPeerRetirement(retirement.PeerRetirement):
    fixture = FIXTURE
    unexpected_request = 'unexpected request key'
    invalid_request_payload = b'\x00'
    messages = [
        ('bin:parrot.interop.Cpu#v1', b'\x29', b'\x2a'),
        ('bin:parrot.interop.Echo#v1', b'\xfb\xfb\x00', b'\xfb\xfb\x00'),
    ]


def run_contracts(evidence):
    MatrixPeerRetirement.evidence = Path(evidence)
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(MatrixPeerRetirement)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        raise RuntimeError('JVM fixture peer retirement contracts failed')


if __name__ == '__main__':
    if os.environ.get('PARROT_MATRIX_PEER_EVIDENCE'):
        MatrixPeerRetirement.evidence = Path(os.environ['PARROT_MATRIX_PEER_EVIDENCE'])
    unittest.main()
