"""Read actual gateway producers from Python and reconcile complete Wire traffic."""
from contextlib import contextmanager
from pathlib import Path
import socket
import sys
import time

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'interop/python'))
from parrot_protocol import admin_v2, wire


@contextmanager
def client(port):
    with socket.create_connection(('127.0.0.1', port), timeout=10) as sock:
        decoder = wire.FrameDecoder()
        sequence = 0

        def exchange(kind, *, payload=b'', path='', key=''):
            nonlocal sequence
            sequence += 1
            encoded = wire.build_frame(kind, sequence, path, key, payload)
            sock.sendall(encoded)
            while True:
                frame = decoder.next_frame()
                if frame is not None:
                    if kind != wire.FT_HANDSHAKE:
                        assert frame.cid == sequence
                    return frame, len(encoded), len(frame.encode())
                chunk = sock.recv(65536)
                assert chunk, 'gateway closed before reply'
                decoder.feed(chunk)

        hello, _, _ = exchange(wire.FT_HANDSHAKE, payload=wire.handshake_body('gateway-metrics-contract'))
        assert hello.ft == wire.FT_HANDSHAKE_ACK
        peer = wire.decode_handshake(hello.payload, acknowledgement=True)
        assert peer['capabilities'] & (1 << 10)

        def sample():
            request = admin_v2.encode_admin_cmd_v2({'kind': 'measured_metrics', 'req_id': sequence + 1})
            response, sent, received = exchange(wire.FT_SYSTEM_EVENT, payload=request, path='admin')
            assert response.ft == wire.FT_SYSTEM_EVENT
            reply = admin_v2.decode_admin_reply_v2(response.payload)
            assert reply['kind'] == 'measured_metrics'
            return reply['metrics'], sent, received

        yield sample, exchange


def check_gateway_metrics(port, process_kind, label):
    with client(port) as (sample, exchange):
        before, _, snapshot_bytes = sample()
        assert before['available'] == (1 << 17) - 1  # This runner is a Linux integration gate.
        assert before['epoch'] and before['process_kind'] == process_kind
        assert before['snapshot']['processes'] > 0 and before['snapshot']['memory_rss'] > 0
        error, ask_bytes, error_bytes = exchange(wire.FT_ASK, path='/user/missing-metrics-probe',
            key='bin:missing', payload=wire.with_reply_to_prefix('', b'probe'))
        assert error.ft == wire.FT_REPLY_ERR
        after, sample_bytes, sample_reply_bytes = sample()
        # A worker can be preempted between successful send and counter update.
        # Accumulate intervening samples before the exact byte comparison.
        sent_samples = sample_bytes
        deadline = time.monotonic() + 2
        while (after['snapshot']['replies_tx'] == before['snapshot']['replies_tx']
               or after['snapshot']['reply_errs'] == before['snapshot']['reply_errs']):
            assert time.monotonic() < deadline, 'reply accounting did not complete'
            snapshot_bytes += sample_reply_bytes
            after, sample_bytes, sample_reply_bytes = sample()
            sent_samples += sample_bytes
        assert after['epoch'] == before['epoch']
        a, b = after['snapshot'], before['snapshot']
        assert a['asks_rx'] - b['asks_rx'] == 1
        assert a['replies_tx'] - b['replies_tx'] == 1
        assert a['reply_errs'] - b['reply_errs'] == 1
        assert a['bytes_rx'] - b['bytes_rx'] == ask_bytes + sent_samples
        assert a['bytes_tx'] - b['bytes_tx'] == snapshot_bytes + error_bytes
    with client(port) as (sample, _):
        deadline = time.monotonic() + 5
        while True:
            reopened, _, _ = sample()
            assert reopened['epoch'] == before['epoch']
            if reopened['snapshot']['connections'] == 1:
                break
            assert time.monotonic() < deadline, reopened
            time.sleep(.01)
        assert reopened['snapshot']['handshakes_ok'] > after['snapshot']['handshakes_ok']
        assert reopened['snapshot']['asks_rx'] >= after['snapshot']['asks_rx']
        assert reopened['snapshot']['uptime_start_ms'] == before['snapshot']['uptime_start_ms']
    print(f'{label}_MEASURED_OK: actual capabilities, units, epoch, exact Wire traffic, failed ASK, reconnect gauge', flush=True)
