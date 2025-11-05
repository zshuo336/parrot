"""Actual child VM and negotiated Wire budgets; assertions observe delivered work."""
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest

from test_connection import Peer, admin_v2, gateway_peer, wire


class GatewayBudget:
    def setUp(self):
        self.files = tempfile.TemporaryDirectory(prefix='parrot-frontier-reply-')
        self.addCleanup(self.files.cleanup)
        directory = Path(self.files.name)
        self.compile_artifact(directory)
        self.context = gateway_peer()
        self.listener, _ = self.context.__enter__()
        self.addCleanup(self.context.__exit__, None, None, None)
        self.peer = Peer(self.listener)
        self.addCleanup(lambda: self.peer.sock.close())
        self.peer.ack()
        self.cid = 1
        self.admin({'kind': 'deploy', 'component': {
            'name': 'q', 'version': '1', 'artifact': {
                'kind': 'beam', 'app': 'frontier', 'uri': directory.as_uri()},
            'instances': {'kind': 'singleton'}, 'config': None}}, 'deployed')
        # Stop while the parent VM still exists, so the disk janitor can finish.
        self.addCleanup(self.stop)

    def compile_artifact(self, directory):
        source = Path(__file__).resolve().parents[2] / 'apps' / self.app / 'erlang/frontier.erl'
        subprocess.run(['erlc', '-o', str(directory), str(source)], check=True, timeout=15)

    def reconnect(self, limit=1048576):
        self.peer.sock.close()
        self.peer = Peer(self.listener)
        fields = wire.parse_tlv(wire.handshake_ack_body('hub'))
        body = b''.join(struct.pack('<BH', tag, len(value)) + value
                        for tag, value in [(tag, struct.pack('<I', limit) if tag == 5 else value)
                                           for tag, value in fields])
        self.peer.send(wire.build_frame(wire.FT_HANDSHAKE_ACK, 1, '', '', body))
        self.limit = min(limit, 1048576)

    def stop(self):
        # A failed assertion may have observed EOF on the small-budget channel.
        self.reconnect()
        self.admin({'kind': 'stop', 'path_prefix': '/user/q'}, 'stopped')

    def admin(self, command, expected):
        self.cid += 1
        command['req_id'] = self.cid
        self.peer.send(wire.build_frame(wire.FT_SYSTEM_EVENT, self.cid, '/admin', '',
                                       admin_v2.encode_admin_cmd_v2(command)))
        reply = self.peer.read()
        self.assertEqual((reply.ft, reply.cid), (wire.FT_SYSTEM_EVENT, self.cid))
        self.assertEqual(admin_v2.decode_admin_reply_v2(reply.payload)['kind'], expected)

    def encode(self, entries):
        data = [struct.pack('<I', len(entries))]
        for ident, url, depth in entries:
            if self.app == 'crawler-lab':
                data.append(struct.pack('<Q', ident))
            data.extend([struct.pack('<I', len(url)), url, struct.pack('<H', depth)])
        return b''.join(data)

    def ask(self, key, payload, error=None):
        self.cid += 1
        self.peer.send(wire.build_frame(wire.FT_ASK, self.cid, '/user/q', self.prefix + key,
                                       b'\0' * 4 + payload))
        reply = self.peer.read()
        self.assertEqual(reply.cid, self.cid)
        self.assertLessEqual(28 + len(reply.path.encode()) + len(reply.type_key.encode()) + len(reply.payload),
                             getattr(self, 'limit', 1048576))
        if error is None:
            self.assertEqual(reply.ft, wire.FT_REPLY, reply)
        else:
            self.assertEqual(reply.ft, wire.FT_REPLY_ERR, reply)
            self.assertEqual(reply.payload[:4], struct.pack('<HH', error, 0))
            reply.payload[4:].decode('utf8')
        return reply

    def push(self, entries, error=None):
        reply = self.ask('Push', self.encode(entries), error)
        if error is None:
            self.assertEqual(reply.type_key, self.prefix + ('Ack' if self.app == 'crawler-lab' else 'PushAck'))
            self.assertEqual(reply.payload, struct.pack('<I', 1 if self.app == 'crawler-lab' else len(entries)))

    def next(self, count=0xffffffff, error=None):
        reply = self.ask('Next', struct.pack('<I', count), error)
        if error is not None:
            return []
        self.assertEqual(reply.type_key, self.prefix + 'Batch')
        data = reply.payload
        count, = struct.unpack_from('<I', data)
        offset = 4
        entries = []
        for _ in range(count):
            ident = None
            if self.app == 'crawler-lab':
                ident, = struct.unpack_from('<Q', data, offset)
                offset += 8
            size, = struct.unpack_from('<I', data, offset)
            offset += 4
            url = data[offset:offset + size]
            offset += size
            depth, = struct.unpack_from('<H', data, offset)
            offset += 2
            entries.append((ident, url, depth))
        self.assertEqual(offset, len(data))
        return entries

    def expected(self, entries):
        return [(i if self.app == 'crawler-lab' else None, u, d) for i, u, d in entries]

class FrontierBudget(GatewayBudget):
    def test_accumulated_pushes_are_delivered_in_bounded_fifo_batches(self):
        entries = [(i, f'http://local/{i}/'.encode() + b'x' * 400000, 0) for i in range(3)]
        for item in entries:
            self.push([item])
        self.assertEqual(self.next(), self.expected(entries[:2]))
        self.assertEqual(self.next(), self.expected(entries[2:]))
        self.assertEqual(self.next(), [])
        print(f'FRONTIER_REPLY_OK {self.app} accumulated 1.2MiB delivered 2+1 without loss', flush=True)

    def test_exact_boundary_and_oversized_head_preserve_fifo_and_depth(self):
        limit = 128
        overhead = 14 if self.app == 'crawler-lab' else 6
        length = limit - 28 - len((self.prefix + 'Batch').encode()) - 4 - overhead
        entries = [(1, b'a' * length, 0), (2, b'b' * (length + 1), 0), (3, b'c', 0), (4, b'd', 1)]
        for item in entries:
            self.push([item])
        self.reconnect(limit)
        self.assertEqual(self.next(0), [])
        self.assertEqual(self.next(), self.expected(entries[:1]))
        self.next(error=10)
        self.next(error=10)  # repeated refusal must not consume, skip, or close
        self.reconnect()
        self.assertEqual(self.next(), self.expected(entries[1:3]))
        self.assertEqual(self.next(), self.expected(entries[3:]))
        self.assertEqual(self.next(), [])
        print(f'FRONTIER_REPLY_OK {self.app} exact bytes and oversized FIFO head preserved', flush=True)

    def reject_transport(self, key, payload, completion=False):
        self.cid += 1
        encoded = wire.build_frame(wire.FT_TELL if completion else wire.FT_ASK,
            self.cid, '/user/q', self.prefix + key,
            payload if completion else b'\0' * 4 + payload, flags=16 if completion else 0)
        self.assertGreater(len(encoded) - 4, self.limit)
        self.peer.send(encoded)
        self.assertEqual(self.peer.sock.recv(1), b'')

    def test_oversize_push_rejects_before_queue_and_dedup_mutation(self):
        entry = (42, b'http://local/retry', 0)
        for _ in range(2):
            self.reconnect(32)
            self.reject_transport('Push', self.encode([entry]))
        self.reconnect()
        self.assertEqual(self.next(), [])
        self.push([entry])
        self.assertEqual(self.next(), self.expected([entry]))
        print(f'FRONTIER_REPLY_OK {self.app} oversized Push admission and same item retry', flush=True)

    def test_oversize_next_request_leaves_work_available(self):
        entries = [(1, b'a', 0), (2, b'b', 0)]
        for item in entries:
            self.push([item])
        for count in (0xffffffff, 0):
            self.reconnect(32)
            self.reject_transport('Next', struct.pack('<I', count))
        self.reconnect()
        self.assertEqual(self.next(), self.expected(entries))
        print(f'FRONTIER_REPLY_OK {self.app} oversized Next preserves every queued item', flush=True)

    def test_oversize_completion_request_cannot_execute(self):
        entry = (1, b'completion', 0)
        self.reconnect(32)
        self.reject_transport('Push', self.encode([entry]), completion=True)
        self.reconnect()
        self.assertEqual(self.next(), [])
        self.push([entry])
        self.assertEqual(self.next(), self.expected([entry]))
        print(f'FRONTIER_REPLY_OK {self.app} oversized completion admission prevents mutation', flush=True)


class CrawlerBudget(FrontierBudget, unittest.TestCase):
    app = 'crawler-lab'
    prefix = 'bin:crawl/Frontier'


class WebsearchBudget(FrontierBudget, unittest.TestCase):
    app = 'websearch'
    prefix = 'bin:ws/'


class ComponentBudget(GatewayBudget, unittest.TestCase):
    prefix = ''

    def compile_artifact(self, directory):
        source = directory / 'frontier.erl'
        context = self._testMethodName.endswith('request')
        source.write_text('-module(frontier).\n-export([parrot_init/1,parrot_service/3]).\nparrot_init(_)->{ok,0}.\nparrot_service(<<"large">>,_,S)->{reply,{<<"large">>,binary:copy(<<0>>,1024)},S+1};\nparrot_service(<<"budget">>,_,S)->{reply,{<<"budget">>,<<16777216:32/little,S:32/little>>},S+1};\nparrot_service(<<"badkey">>,_,S)->{reply,{<<255>>,<<>>},S+1};\nparrot_service(<<"count">>,_,S)->{reply,{<<"count">>,<<S:32/little>>},S}.\n')
        if context:
            text = source.read_text().replace('-export([parrot_init/1,parrot_service/3]).',
                                               '-export([parrot_init/1,parrot_service/3,parrot_service/4]).')
            text += 'parrot_service(<<"budget">>,_,S,#{reply_bytes:=N})->{reply,{<<"budget">>,<<N:32/little,S:32/little>>},S+1};\nparrot_service(K,B,S,_)->parrot_service(K,B,S).\n'
            source.write_text(text)
        subprocess.run(['erlc', '-o', str(directory), str(source)], check=True, timeout=15)

    def test_legacy_stateful_oversized_reply_does_not_commit_next_state(self):
        self.reconnect(128)
        self.ask('large', b'', error=10)
        self.assertEqual(self.ask('count', b'').payload, struct.pack('<I', 0))
        self.reconnect()
        self.assertEqual(self.ask('large', b'').payload, b'\0' * 1024)
        self.assertEqual(self.ask('count', b'').payload, struct.pack('<I', 1))
        print('COMPONENT_REPLY_OK legacy callback keeps state on oversized output and later succeeds', flush=True)

    def test_invalid_utf8_reply_key_does_not_commit_next_state(self):
        self.ask('badkey', b'', error=12)
        self.assertEqual(self.ask('count', b'').payload, struct.pack('<I', 0))
        print('COMPONENT_REPLY_OK invalid UTF-8 key refuses state transition', flush=True)

    def test_high_peer_capacity_is_capped_by_local_handshake_per_request(self):
        for count, limit in enumerate([0xffffffff, 16777216, 128]):
            self.reconnect(limit)
            self.assertEqual(self.ask('budget', b'').payload,
                             struct.pack('<II', min(limit, 1048576) - 28, count))
        print('COMPONENT_REPLY_OK large peer capacity respects local advertised ceiling and later small request', flush=True)

    def test_optional_context_tracks_each_request(self):
        for count, limit in enumerate([64, 256, 64]):
            self.reconnect(limit)
            self.assertEqual(self.ask('budget', b'').payload, struct.pack('<II', limit - 28, count))
        self.assertEqual(self.ask('count', b'').payload, struct.pack('<I', 3))
        print('COMPONENT_REPLY_OK callback receives current peer budget across successive connections', flush=True)


if __name__ == '__main__':
    unittest.main()
