"""Real collector receipts, bounded retries, ownership and lifecycle contracts."""
import asyncio
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import socket
import ssl
import subprocess
import threading
import time
import types

import pytest

from parrot_protocol.trace import AsyncTraceActor, current_parent
from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig, TraceRecorder
from parrot_protocol.ray_gw import ParrotDispatcher, _require_trace_finish
from parrot_protocol.ray_hosts import IsolatedRuntime
from parrot_protocol.trace_export import TraceExportRejected, _accepted


class Collector:
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = []
        self.response = (200, b'{}', {})
        self.block = None
        self.received = threading.Event()

    def set_response(self, code=200, body=b'{}', headers=None):
        with self.lock:
            self.response = (code, body, headers or {})

    def documents(self):
        with self.lock:
            return [json.loads(body) for _, body in self.requests]

    def spans(self):
        return [span for document in self.documents() for resource in document['resourceSpans']
                for scope in resource['scopeSpans'] for span in scope['spans']]


@contextmanager
def collector(tls=None):
    owner = Collector()
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers['Content-Length']))
            if self.headers['Content-Type'] != 'application/json':
                raise RuntimeError('wrong export content type')
            with owner.lock:
                owner.requests.append((self.path, body))
                code, response, headers = owner.response
                block = owner.block
            owner.received.set()
            if block is not None and not block.wait(10):
                raise RuntimeError('collector test barrier timed out')
            self.send_response(code)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(response)))
            for key, value in headers.items():
                self.send_header(key, value)
            self.end_headers()
            try:
                self.wfile.write(response)
            except (BrokenPipeError, ConnectionResetError):
                pass
        def log_message(self, *args):
            pass
    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    server.daemon_threads = False
    if tls is not None:
        server.socket = tls.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': 0.02})
    thread.start()
    owner.endpoint = ('https' if tls is not None else 'http') + '://127.0.0.1:' + str(server.server_port) + '/v1/traces'
    try:
        yield owner
    finally:
        if owner.block is not None:
            owner.block.set()
        server.shutdown()
        server.server_close()
        thread.join(5)
        assert not thread.is_alive()


def recorder(owner, **options):
    return TraceRecorder(TraceConfig(export=TraceExportConfig(owner.endpoint, interval=0.01, **options)))


def wait_for(predicate, timeout=5):
    end = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= end:
            pytest.fail('condition not observed before deadline')
        time.sleep(0.01)


@pytest.mark.parametrize('endpoint', ['', 'file:///tmp/x', 'http:///path', 'http://a:0',
    'http://a:99999', 'http://user:password@a', 'http://a/#fragment', 'http://a/\r\nx', 'http://a/\u4e2d'])
def test_invalid_endpoints_never_start_worker(endpoint):
    with pytest.raises(ValueError):
        TraceExportConfig(endpoint)


@pytest.mark.parametrize('field,value', [('interval', 0), ('interval', float('nan')),
    ('request_timeout', True), ('request_timeout', 31), ('max_attempts', 0), ('max_attempts', 11),
    ('batch_size', 0), ('batch_size', 129), ('batch_size', 1.5), ('ca_file', '\x00')])
def test_invalid_export_bounds_rejected(field, value):
    with pytest.raises(ValueError):
        TraceExportConfig('http://localhost:4318/v1/traces', **{field: value})


def test_continuous_export_frees_capacity_and_matches_handler_identity():
    with collector() as sink:
        rec = TraceRecorder(TraceConfig(capacity=2, export=TraceExportConfig(sink.endpoint, batch_size=1)))
        identities = []
        try:
            for i in range(6):
                with rec.handler('/user/a', str(i), 'ask') as identity:
                    identities.append(identity)
                wait_for(lambda: rec.export_status()['exported'] == i + 1)
            status = rec.finish_export(5)
            assert status['ok'] and not status['running'] and status['dropped'] == 0
            assert status['exported'] == 6 and rec.snapshot()['records'] == []
            assert {(s['traceId'], s['spanId']) for s in sink.spans()} == {
                (p.trace_id.hex(), p.span_id.hex()) for p in identities}
        finally:
            assert rec.finish_export(5)['ok']


@pytest.mark.parametrize('code,body', [
    (503, b'{}'), (204, b''), (200, b''), (200, b'[]'), (200, b'{"error":"bad"}'),
    (200, b'{"partialSuccess":{"rejectedSpans":"1"}}'),
    (200, b'{"partialSuccess":{"rejectedSpans":true}}'),
    (200, b'{"partialSuccess":{"rejectedSpans":"-1"}}'),
    (200, b'{"partialSuccess":{"rejectedSpans":"2"}}'),
    (200, b'{"partialSuccess":{},"partialSuccess":{}}'), (200, b'x' * 65537),
    (200, '{}'.encode('utf-16')),
    (200, b'{"partialSuccess":{"rejectedSpans":"0","errorMessage":"\\ud800"}}'),
])
def test_invalid_or_partial_receipt_retains_batch_and_explicit_retry_recovers(code, body):
    with collector() as sink:
        sink.set_response(code, body)
        rec = recorder(sink, max_attempts=2)
        try:
            with rec.handler('/user/a', 'key', 'ask') as identity:
                pass
            wait_for(lambda: not rec.export_status()['running'])
            status = rec.export_status()
            assert status['attempts'] == 2 and status['pending'] == 1 and status['exported'] == 0
            assert rec.snapshot()['records'][0]['span_id'] == identity.span_id.hex()
            sink.set_response()
            status = rec.finish_export(5)
            assert status['ok'] and status['attempts'] == 3 and status['exported'] == 1
            assert len(sink.requests) == 3
        finally:
            sink.set_response()
            assert rec.finish_export(5)['ok']


def test_redirect_is_not_followed_or_acknowledged():
    with collector() as sink:
        sink.set_response(302, b'{}', {'Location': sink.endpoint + '/redirect'})
        rec = recorder(sink, max_attempts=1)
        try:
            with rec.handler('/user/a', 'key', 'ask'):
                pass
            wait_for(lambda: not rec.export_status()['running'])
            assert len(sink.requests) == 1 and sink.requests[0][0] == '/v1/traces'
            assert rec.export_status()['pending'] == 1
        finally:
            sink.set_response()
            assert rec.finish_export(5)['ok']


def test_otlp_response_requires_utf8_bytes():
    with pytest.raises(TraceExportRejected):
        _accepted('{} '.encode('utf-16'), 1)


def test_acknowledgements_cannot_race_continuous_export_owner():
    with collector() as sink:
        sink.block = threading.Event()
        rec = recorder(sink)
        try:
            with rec.handler('/user/a', 'key', 'ask') as identity:
                pass
            assert sink.received.wait(5)
            with pytest.raises(RuntimeError, match='owns acknowledgements'):
                rec.acknowledge([identity.span_id.hex()])
            status = rec.finish_export(0.05)
            assert not status['ok'] and status['running'] and status['pending'] == 1
            thread = rec._exporter._thread
            status = rec.finish_export(0.05)
            assert not status['ok'] and rec._exporter._thread is thread
            assert len(sink.requests) == 1
        finally:
            sink.block.set()
            assert rec.finish_export(5)['ok']


def test_flush_retries_failed_worker_without_closing_business_admission():
    with collector() as sink:
        sink.set_response(503)
        rec = recorder(sink, max_attempts=1)
        try:
            with rec.handler('/user/a', 'first', 'ask'):
                pass
            wait_for(lambda: not rec.export_status()['running'])
            sink.set_response()
            status = rec.flush_export(5)
            assert status['ok'] and not status['closed'] and status['exported'] == 1
            with rec.handler('/user/a', 'second', 'ask'):
                pass
            assert rec.finish_export(5)['exported'] == 2
        finally:
            sink.set_response()
            assert rec.finish_export(5)['ok']


def test_socket_timeout_exhausts_attempts_without_acknowledging_request_submission():
    with collector() as sink:
        sink.block = threading.Event()
        rec = recorder(sink, request_timeout=0.05, max_attempts=2)
        try:
            with rec.handler('/user/a', 'key', 'ask'):
                pass
            wait_for(lambda: not rec.export_status()['running'])
            status = rec.export_status()
            assert status['attempts'] == 2 and status['pending'] == 1 and status['exported'] == 0
            assert status['error'] == socket.timeout.__name__
        finally:
            sink.block.set()
            assert rec.finish_export(5)['ok']


def test_busy_collector_does_not_block_handlers_or_break_batch_size_bound():
    with collector() as sink:
        sink.block = threading.Event()
        rec = TraceRecorder(TraceConfig(capacity=8, export=TraceExportConfig(sink.endpoint, batch_size=2)))
        try:
            with rec.handler('/user/a', '0', 'ask'):
                pass
            assert sink.received.wait(5)
            for i in range(1, 7):
                with rec.handler('/user/a', str(i), 'ask'):
                    pass
            assert rec.export_status()['pending'] == 7
            sink.block.set()
            status = rec.finish_export(5)
            assert status['ok'] and status['exported'] == 7 and status['dropped'] == 0
            for document in sink.documents():
                count = sum(len(scope['spans']) for resource in document['resourceSpans'] for scope in resource['scopeSpans'])
                assert 1 <= count <= 2
        finally:
            sink.block.set()
            assert rec.finish_export(5)['ok']


@pytest.mark.parametrize('body', [b'{"partialSuccess":{"rejectedSpans":"0","errorMessage":"warning"}}',
    b'{"partialSuccess":null}', b'{"partialSuccess":{"rejectedSpans":null,"errorMessage":null}}'])
def test_warning_with_zero_rejections_is_successful_otlp_receipt(body):
    with collector() as sink:
        sink.set_response(body=body)
        rec = recorder(sink)
        with rec.handler('/user/a', 'key', 'ask'):
            pass
        assert rec.finish_export(5)['ok']
        assert rec.export_status()['exported'] == 1


def test_export_owner_close_is_atomic_with_delayed_handler_admission():
    with collector() as sink:
        rec = recorder(sink)
        scope = rec.handler('/user/a', 'key', 'ask')
        assert rec.finish_export(5)['ok']
        entered = []
        with pytest.raises(RuntimeError, match='retiring'):
            with scope:
                entered.append(True)
        assert entered == [] and sink.requests == []


def test_async_native_finish_yields_for_admitted_handler_and_rejects_new_business():
    with collector() as sink:
        async def run():
            entered, release = asyncio.Event(), asyncio.Event()
            class Actor(AsyncTraceActor):
                async def ask(self, key, body):
                    entered.set()
                    await release.wait()
                    return key, current_parent()
            actor = Actor()
            actor.parrot_trace_configure(TraceConfig(export=TraceExportConfig(sink.endpoint)), '/user/async')
            task = asyncio.create_task(actor.ask_traced('key', b'', None))
            finish = None
            try:
                await asyncio.wait_for(entered.wait(), 5)
                finish = asyncio.create_task(actor.parrot_trace_finish(5))
                end = time.monotonic() + 2
                while not actor._parrot_recorder.closed:
                    assert time.monotonic() < end
                    await asyncio.sleep(0.01)
                with pytest.raises(RuntimeError, match='retiring'):
                    await actor.ask_traced('key', b'new', None)
                release.set()
                _, identity = await asyncio.wait_for(task, 5)
                status = await asyncio.wait_for(finish, 5)
                assert status['ok'] and status['exported'] == 1 and not status['running']
                assert sink.spans()[0]['spanId'] == identity.span_id.hex()
            finally:
                release.set()
                await task
                if finish is not None:
                    await finish
                assert (await actor.parrot_trace_finish(5))['ok']
        asyncio.run(run())


def test_https_requires_trusted_certificate_and_accepts_explicit_ca(tmp_path):
    key, cert = tmp_path/'key.pem', tmp_path/'cert.pem'
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                    '-keyout', str(key), '-out', str(cert), '-subj', '/CN=127.0.0.1',
                    '-addext', 'subjectAltName=IP:127.0.0.1'], check=True, capture_output=True, timeout=15)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    with collector(context) as sink:
        rejected = recorder(sink, max_attempts=1)
        with rejected.handler('/user/a', 'key', 'ask'):
            pass
        status = rejected.finish_export(5)
        assert not status['ok'] and not status['running'] and status['pending'] == 1
        assert status['error'] == 'SSLCertVerificationError' and sink.requests == []
        trusted = recorder(sink, max_attempts=1, ca_file=str(cert))
        with trusted.handler('/user/a', 'key', 'ask'):
            pass
        assert trusted.finish_export(5)['ok']
        assert len(sink.spans()) == 1


def test_confirmed_owner_cannot_be_retired_with_disabled_or_malformed_receipt():
    disabled = dict(ok=True, enabled=False)
    _require_trace_finish(disabled, allow_disabled=True)
    with pytest.raises(RuntimeError):
        _require_trace_finish(disabled)
    with pytest.raises(RuntimeError):
        _require_trace_finish(dict(ok=1, enabled=0), allow_disabled=True)
    good = dict(ok=True, enabled=True, running=False, pending=0, active=0, error=None)
    _require_trace_finish(good)
    for field, value in [('pending', False), ('active', False), ('ok', 1), ('running', True), ('error', 'lost')]:
        with pytest.raises(RuntimeError):
            _require_trace_finish(dict(good, **{field: value}), allow_disabled=True)


def test_dead_deployment_without_export_receipt_remains_explicitly_uncertain_on_retry():
    class ActorDiedError(Exception):
        pass
    class Ray:
        exceptions = types.SimpleNamespace(ActorDiedError=ActorDiedError)
        def get(self, ref, **kwargs):
            raise ActorDiedError('host process died')
    with collector() as sink:
        runtime = IsolatedRuntime(ParrotDispatcher(), Ray(),
            trace_config=TraceConfig(export=TraceExportConfig(sink.endpoint)))
        runtime._entries['/user/a'] = dict(owner='a', handle=None, state='running')
        runtime._deployments['a'] = dict(host=types.SimpleNamespace(
            retire_paths=types.SimpleNamespace(remote=lambda *args: 'request')),
            dead=False, setup_failed=False, trace_configured=True, job=None,
            snapshot=types.SimpleNamespace(close=lambda: None))
        try:
            for _ in range(2):
                assert runtime.retire_paths('a', ['/user/a']) == {'/user/a': 'stop_failed'}
                assert '/user/a' in runtime._entries and runtime._deployments['a']['trace_uncertain']
            assert runtime.trace_export_status('/user/a') == dict(
                enabled=True, ok=False, error='TraceCompletionUnknown', retained=True)
            assert runtime.close() == dict(ok=False, pending=['a'])
        finally:
            assert runtime.trace_finish()['ok']
