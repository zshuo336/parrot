"""Source-only escript deployment and parent-VM death with actual child BEAMs."""
from pathlib import Path
import os
import sys
import shutil
import struct
import subprocess
import tempfile
import time
import unittest
from unittest import mock

import test_connection as connection
from test_connection import Peer, admin_v2, gateway_peer, wire

MODULES = ('parrot_gw', 'parrot_trace', 'parrot_component_runtime', 'parrot_beam_guest',
           'parrot_beam_host', 'parrot_beam_snapshot')


def wait_gone(pid):
    until = time.monotonic() + 10
    while True:
        # With a Linux subreaper, orphaned grandchildren can be reaped here.
        # Otherwise the operating system's init reaps them.
        try:
            reaped, _ = os.waitpid(pid, os.WNOHANG)
            if reaped == pid:
                return
        except ChildProcessError:
            pass
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        if time.monotonic() >= until:
            raise AssertionError(f'owned guest {pid} still exists')
        time.sleep(0.025)


def admin(peer, command):
    request = command['req_id']
    peer.send(wire.build_frame(wire.FT_SYSTEM_EVENT, request, '/admin', '',
                              admin_v2.encode_admin_cmd_v2(command)))
    reply = peer.read()
    if (reply.ft, reply.cid) != (wire.FT_SYSTEM_EVENT, request):
        raise AssertionError('unexpected admin reply envelope')
    return admin_v2.decode_admin_reply_v2(reply.payload)


def ask(peer, request, key):
    peer.send(wire.build_frame(wire.FT_ASK, request, '/user/bundle', key, b'\0' * 4))
    reply = peer.read()
    if (reply.ft, reply.cid) != (wire.FT_REPLY, request):
        raise AssertionError('unexpected ASK reply envelope: %r' % (reply,))
    return reply


class SupportContracts(unittest.TestCase):
    def test_guest_deadline_survives_optimization(self):
        with mock.patch.object(os, 'waitpid', side_effect=ChildProcessError), \
             mock.patch.object(os, 'kill', side_effect=[None, ProcessLookupError()]), \
             mock.patch.object(time, 'monotonic', side_effect=[0, 11]), \
             mock.patch.object(time, 'sleep') as sleep:
            with self.assertRaises(AssertionError):
                wait_gone(12345)
            sleep.assert_not_called()

    def test_admin_validates_type_and_correlation(self):
        for kind, cid in [(wire.FT_HEARTBEAT, 17), (wire.FT_SYSTEM_EVENT, 18)]:
            with self.subTest(kind=kind, cid=cid):
                peer = mock.Mock()
                peer.read.return_value = wire.Frame(kind, cid, '', '', b'')
                with mock.patch.object(admin_v2, 'decode_admin_reply_v2', return_value={}):
                    with self.assertRaises(AssertionError):
                        admin(peer, {'kind': 'metrics', 'req_id': 17})

    def test_ask_validates_type_and_correlation(self):
        for kind, cid in [(wire.FT_HEARTBEAT, 17), (wire.FT_REPLY, 18)]:
            with self.subTest(kind=kind, cid=cid):
                peer = mock.Mock()
                peer.read.return_value = wire.Frame(kind, cid, '', '', b'')
                with self.assertRaises(AssertionError):
                    ask(peer, 17, 'inspect')


class Bundle(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # This runner only owns its test gateways. Containers without an init
        # otherwise leave orphan zombies after the deliberate parent SIGKILL.
        if sys.platform.startswith('linux'):
            import ctypes
            libc = ctypes.CDLL(None, use_errno=True)
            if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
                raise OSError(ctypes.get_errno(), 'cannot reap owned orphan guests')

    def exercise(self, kill_parent):
        snapshot = None
        pid = None
        with tempfile.TemporaryDirectory(prefix='parrot-beam-bundle-') as temp:
            root = Path(temp)
            bundle = root / 'host'
            artifact = root / 'artifact'
            bundle.mkdir()
            artifact.mkdir()
            source = Path(__file__).parent
            for name in [*(module + '.erl' for module in MODULES), 'parrot_gw.es']:
                shutil.copyfile(source / name, bundle / name)
            (artifact / 'bundle_fixture.erl').write_text('''-module(bundle_fixture).
-export([parrot_init/1,parrot_service/3]).
parrot_init(_)->{ok,0}.
parrot_service(<<"inspect">>,_,S)->
    {reply,{<<"identity">>,iolist_to_binary([os:getpid(),"\\n",filename:dirname(code:which(?MODULE))])},S};
parrot_service(<<"inc">>,_,S)->{reply,{<<"count">>,<<(S+1):64/little>>},S+1}.
''')
            subprocess.run(['erlc', 'bundle_fixture.erl'], cwd=artifact, check=True, timeout=15)
            old = connection.GATEWAY
            connection.GATEWAY = bundle / 'parrot_gw.es'
            try:
                with gateway_peer() as (listener, proc):
                    peer = Peer(listener)
                    try:
                        peer.ack()
                        reply = admin(peer, {'kind': 'deploy', 'req_id': 2, 'component': {
                            'name': 'bundle', 'version': '1', 'artifact': {
                                'kind': 'beam', 'app': 'bundle_fixture', 'uri': artifact.as_uri()},
                            'instances': {'kind': 'singleton'}, 'config': None}})
                        self.assertEqual(reply['kind'], 'deployed', reply)
                        pid_text, data_dir = ask(peer, 3, 'inspect').payload.decode().split('\n')
                        pid = int(pid_text)
                        self.assertNotEqual(pid, proc.pid)
                        snapshot = Path(data_dir).parent
                        self.assertEqual(snapshot.parent, Path('/tmp'))
                        self.assertTrue(snapshot.name.startswith(f'parrot-beam-host-{proc.pid}-'))
                        self.assertEqual(list(bundle.glob('*.beam')), [])
                        shutil.rmtree(artifact)
                        self.assertEqual(ask(peer, 4, 'inc').payload, struct.pack('<Q', 1))
                        if kill_parent:
                            proc.kill()
                            proc.wait(timeout=5)
                            wait_gone(pid)
                            print(f'BEAM_BUNDLE_VM_CRASH_OK child exited; crash_snapshot_retained={snapshot.exists()}', flush=True)
                        else:
                            reply = admin(peer, {'kind': 'stop', 'req_id': 5, 'path_prefix': '/user/bundle'})
                            self.assertEqual(reply['kind'], 'stopped', reply)
                            wait_gone(pid)
                            self.assertFalse(snapshot.exists())
                            print('BEAM_BUNDLE_SOURCE_ONLY_OK memory bootstrap, actual deploy/ASK/Stop, child and snapshot retired', flush=True)
                    finally:
                        peer.sock.close()
            finally:
                connection.GATEWAY = old
                # VM-wide SIGKILL cannot run its in-VM disk janitor. Cleanup only
                # this test's observed snapshot after the owned child exited.
                if snapshot is not None and snapshot.exists():
                    if pid is not None:
                        wait_gone(pid)
                    shutil.rmtree(snapshot)

    def test_source_only_escript(self):
        self.exercise(False)

    def test_whole_gateway_vm_death_retires_child(self):
        self.exercise(True)


if __name__ == '__main__':
    unittest.main()
