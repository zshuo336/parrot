"""Real Frontier deployments: atomic bounded admission and resource release."""
from pathlib import Path
import struct
import unittest
from test_frontier_reply_budget import GatewayBudget
from test_connection import wire


class CapacityCases(GatewayBudget):
    def configure(self, raw=None, policy=None, expected='deployed', **limits):
        config = raw if raw is not None else '\n'.join(f'{k} = {v}' for k, v in limits.items()).encode()
        self.admin({'kind': 'deploy', 'component': {
            'name': 'q', 'version': '1', 'artifact': {'kind': 'beam', 'app': 'frontier',
                'uri': Path(self.files.name).as_uri()},
            'instances': policy or {'kind': 'singleton'}, 'config': config}}, expected)

    def push_exact(self, entries, accepted, error=None):
        reply = self.ask('Push', self.encode(entries), error)
        if error is None:
            self.assertEqual(reply.payload, struct.pack('<I', 1 if self.app == 'crawler-lab' else accepted))

    def ask(self, key, payload, error=None):
        if not hasattr(self, 'actor_path'):
            return super().ask(key, payload, error)
        self.cid += 1
        self.peer.send(wire.build_frame(wire.FT_ASK, self.cid, self.actor_path, self.prefix + key,
                                       b'\0' * 4 + payload))
        reply = self.peer.read()
        self.assertEqual(reply.cid, self.cid)
        self.assertEqual(reply.ft, wire.FT_REPLY if error is None else wire.FT_REPLY_ERR)
        if error is not None:
            self.assertEqual(reply.payload[:4], struct.pack('<HH', error, 0))
        return reply

    def marker(self, name):
        print(f'FRONTIER_CAPACITY_OK {self.app} {name}', flush=True)

    def test_queue_count_rejects_whole_batch_and_releases_on_next(self):
        self.configure(max_queued_items=2)
        a, b, c = [(i, str(i).encode(), 0) for i in range(3)]
        self.push_exact([a], 1)
        self.push_exact([b, c], 0, error=10)
        self.assertEqual(self.next(), self.expected([a]))
        self.push_exact([b, c], 2)
        self.push_exact([b, b, c], 0)
        self.assertEqual(self.next(), self.expected([b, c]))
        self.marker('queue count, atomic refusal, duplicate accounting and released capacity')

    def test_queue_bytes_are_exact_utf8_encoded_cost_not_characters(self):
        first = (1, 'é🙂'.encode(), 0)  # six UTF-8 bytes
        equal = (2, 'ö🙃'.encode(), 0)
        large = (3, 'ä🙂x'.encode(), 0)
        cost = 6 + (14 if self.app == 'crawler-lab' else 6)
        self.configure(max_queued_bytes=2 * cost)
        self.push_exact([first], 1)
        self.push_exact([large], 0, error=10)
        self.push_exact([equal], 1)
        self.assertEqual(self.next(1), self.expected([first]))
        self.push_exact([large], 0, error=10)
        self.assertEqual(self.next(), self.expected([equal]))
        self.push_exact([large], 1)
        self.assertEqual(self.next(), self.expected([large]))
        self.marker('UTF-8 byte boundary and exact queue byte release')

    def test_seen_count_survives_consumption_and_resets_only_on_redeploy(self):
        self.configure(max_queued_items=1, max_seen_items=2)
        entries = [(i, str(i).encode(), 0) for i in range(3)]
        for item in entries[:2]:
            self.push_exact([item], 1)
            self.assertEqual(self.next(), self.expected([item]))
        self.push_exact([entries[2]], 0, error=10)
        self.push_exact([entries[0]], 0)
        self.assertEqual(self.next(), [])
        self.configure(max_queued_items=1, max_seen_items=2)
        self.push_exact([entries[2]], 1)
        self.assertEqual(self.next(), self.expected(entries[2:]))
        self.marker('seen lifetime is distinct from queued work and deploy starts empty')

    def test_seen_bytes_refuse_atomically_after_queue_is_empty(self):
        entries = [(i, value.encode(), 0) for i, value in enumerate(['éé', 'öö', 'ää'])]
        self.configure(max_seen_bytes=16)
        for item in entries[:2]:
            self.push_exact([item], 1)
            self.assertEqual(self.next(), self.expected([item]))
        self.push_exact([entries[0], entries[2]], 0, error=10)
        self.assertEqual(self.next(), [])
        self.push_exact([entries[0], entries[1]], 0)
        self.assertEqual(self.next(), [])
        self.marker('seen exact bytes persist after Next and mixed refused batch leaves no queue')

    def test_duplicates_charge_once_and_keep_first_payload(self):
        self.configure(max_queued_items=2, max_seen_items=2)
        a, b = (1, b'a', 0), (2, b'b', 0)
        self.push_exact([a, (1, b'a', 9), b], 2)
        self.push_exact([a, b], 0)
        self.assertEqual(self.next(), self.expected([a, b]))
        self.push_exact([a, b], 0)
        self.assertEqual(self.next(), [])
        self.marker('within-batch and historical duplicates do not consume capacity')

    def test_invalid_limits_fail_upgrade_and_keep_existing_work(self):
        first = (7, b'kept', 0)
        self.push_exact([first], 1)
        invalid = [b'max_queued_items=0', b'max_queued_items=-1', b'max_queued_items=true',
            b'max_queued_items="2"', b'max_queued_items=1\nmax_queued_items=2',
            b'max_queue_items=1', b'[frontier]\nmax_queued_items=2',
            b'max_queued_bytes=67108865', b'max_seen_items=262145',
            b'max_seen_bytes=1.5', b'\xff', b'x' * 4097]
        for raw in invalid:
            with self.subTest(config=raw[:50]):
                self.configure(raw=raw, expected='failed')
        self.assertEqual(self.next(), self.expected([first]))
        self.configure(raw=b'# bounded settings\r\nmax_queued_items = +2 # exact\nmax_seen_items=2_0\n')
        self.push_exact([(1, b'a', 0), (2, b'b', 0)], 2)
        self.push_exact([(3, b'c', 0)], 0, error=10)
        self.marker('strict bounded scalar configuration and failed upgrades preserve old instance')

    def test_default_queue_count_is_actually_enforced(self):
        for start in range(0, 65536, 4096):
            self.push_exact([(i, str(i).encode(), 0) for i in range(start, start + 4096)], 4096)
        extra = (65536, b'extra', 0)
        self.push_exact([extra], 0, error=10)
        self.assertEqual(self.next(1), self.expected([(0, b'0', 0)]))
        self.push_exact([extra], 1)
        self.marker('65536 actual queued entries reach the default limit and Next returns a slot')

    def test_default_seen_count_is_actually_enforced_after_consumption(self):
        for start in range(0, 262144, 4096):
            entries = [(i, str(i).encode(), 0) for i in range(start, start + 4096)]
            self.push_exact(entries, 4096)
            self.assertEqual(self.next(), self.expected(entries))
        self.push_exact([(262144, b'extra', 0)], 0, error=10)
        self.push_exact([(0, b'0', 0)], 0)
        self.assertEqual(self.next(), [])
        self.marker('262144 consumed entries still occupy the real default dedup budget')

    def test_default_queue_byte_limit_with_real_large_resources(self):
        self.configure(max_seen_bytes=67108864)
        entries = [(i, str(i).encode().rjust(8, b'0') + b'x' * 999992, 0) for i in range(17)]
        for item in entries[:16]:
            self.push_exact([item], 1)
        self.push_exact(entries[16:], 0, error=10)
        self.assertEqual(self.next(1), self.expected(entries[:1]))
        self.push_exact(entries[16:], 1)
        self.marker('real 16MiB default queued byte budget, with independent seen capacity')

    def test_pool_members_own_capacity_and_retiring_one_keeps_the_other(self):
        self.configure(policy={'kind': 'pool', 'count': 2}, max_queued_items=1, max_seen_items=2)
        a, b = (1, b'a', 0), (2, b'b', 0)
        try:
            for path in ['/user/q-0', '/user/q-1']:
                self.actor_path = path
                self.push_exact([a], 1)
                self.push_exact([b], 0, error=10)
            self.actor_path = '/user/q-0'
            self.assertEqual(self.next(), self.expected([a]))
            self.push_exact([b], 1)
            self.admin({'kind': 'stop', 'path_prefix': '/user/q-0'}, 'stopped')
            self.actor_path = '/user/q-1'
            self.assertEqual(self.next(), self.expected([a]))
            self.push_exact([b], 1)
            self.assertEqual(self.next(), self.expected([b]))
        finally:
            del self.actor_path
        self.marker('pool limits are per actual instance and sibling retirement preserves work')

    def test_per_push_work_limit_is_enforced_before_dedup(self):
        item = (1, b'a', 0)
        self.push_exact([item] * 65537, 0, error=10)
        self.assertEqual(self.next(), [])
        self.push_exact([item] * 65536, 1)
        self.assertEqual(self.next(), self.expected([item]))
        self.marker('65536 entry parse budget counts duplicates but preserves whole-batch refusal')


class CrawlerCapacity(CapacityCases, unittest.TestCase):
    app = 'crawler-lab'
    prefix = 'bin:crawl/Frontier'


class WebsearchCapacity(CapacityCases, unittest.TestCase):
    app = 'websearch'
    prefix = 'bin:ws/'


if __name__ == '__main__':
    unittest.main()
