"""Production serve() wiring and actual signal-driven export retirement."""
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import time

from parrot_protocol import wire
from test_causal_context import PARENT, ask
from test_trace_export import collector, wait_for


def test_actual_serve_exports_handler_before_successful_signal_exit(tmp_path):
    configured = os.environ.get('PARROT_EXPORT_SERVE_EVIDENCE')
    root = Path(configured) if configured else tmp_path
    root.mkdir(parents=True, exist_ok=True)
    log_path = root/'gateway.log'
    fixture = Path(__file__).with_name('support')/'export_gateway.py'
    with collector() as sink, log_path.open('xb') as log:
        command = [sys.executable, '-B'] + ['-O'] * sys.flags.optimize + [str(fixture), str(root), sink.endpoint]
        process = subprocess.Popen(command, env=dict(os.environ, PYTHONUNBUFFERED='1'), stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            def ready():
                if process.poll() is not None:
                    raise AssertionError(log_path.read_text())
                return re.search(r'RAY_GW_PORT=([0-9]+)', log_path.read_text())
            wait_for(ready, timeout=30)
            reported = re.search(r'^PARROT_TEST_RUNTIME=(.+)$', log_path.read_text(), re.MULTILINE)
            assert reported is not None, log_path.read_text()
            runtime = json.loads(reported[1])
            assert Path(runtime['executable']).resolve() == Path(sys.executable).resolve()
            assert runtime['version'] == list(sys.version_info[:3])
            assert runtime['optimize'] == sys.flags.optimize
            port = int(ready()[1])
            with socket.create_connection(('127.0.0.1', port), timeout=10) as peer:
                decoder = wire.FrameDecoder()
                def receive():
                    while True:
                        frame = decoder.next_frame()
                        if frame is not None:
                            return frame
                        data = peer.recv(65536)
                        assert data
                        decoder.feed(data)
                peer.sendall(wire.build_frame(wire.FT_HANDSHAKE, 1, '', '',
                    wire.handshake_body('export-client', 3 | wire.CAPS_TRACE_CONTEXT)))
                assert receive().ft == wire.FT_HANDSHAKE_ACK
                peer.sendall(ask(2, b'business', PARENT).encode())
                response = receive()
                assert response.ft == wire.FT_REPLY and response.cid == 2
                identity, body = response.payload[:25], response.payload[25:]
                assert body == b'business' and identity[1:17] == PARENT.trace_id and identity[17:] != PARENT.span_id
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=30) == 0, log_path.read_text()
            assert (root/'finished.txt').read_text() == 'retired\n'
            spans = sink.spans()
            assert len(spans) == 1 and spans[0]['spanId'] == identity[17:].hex()
            assert spans[0]['parentSpanId'] == PARENT.span_id.hex()
            (root/'documents.json').write_text(json.dumps(sink.documents(), indent=2))
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)
