"""Owned OTLP/HTTP worker. Records remain owned until a valid success receipt."""
import http.client
import io
import json
import math
import re
import socket
import ssl
import threading
import time
from urllib.parse import urlsplit

from .trace_recording import snapshot_otlp


_STATUS_LINE = re.compile(rb'HTTP/[0-9]\.[0-9] [0-9]{3} [\t\x20-\x7e\x80-\xff]*\r\n')
_CHUNK_TOKEN = rb"[!#$%&'*+\-.^_`|~0-9A-Za-z]+"
_CHUNK_QUOTED = rb'"(?:[\t \x21\x23-\x5b\x5d-\x7e\x80-\xff]|\\[\t \x21-\x7e\x80-\xff])*"'
# RFC 9112 section 7.1.1 permits BWS at extension separators, and token or quoted values.
_CHUNK_LINE = re.compile(rb'([0-9A-Fa-f]+)(?:[ \t]*;[ \t]*' + _CHUNK_TOKEN +
                         rb'(?:[ \t]*=[ \t]*(?:' + _CHUNK_TOKEN + rb'|' + _CHUNK_QUOTED + rb'))?)*')


class TraceExportRejected(RuntimeError):
    pass


class _ReceiptRaw(io.RawIOBase):
    """Apply the shared response budget before every underlying socket read."""
    def __init__(self, raw, sock, end):
        self._raw = raw
        self._socket = sock
        self._end = end

    def readable(self):
        return True

    def _remaining(self):
        remaining = self._end - time.monotonic()
        if remaining <= 0:
            raise socket.timeout('collector response exceeded deadline')
        return remaining

    def readinto(self, buffer):
        self._socket.settimeout(self._remaining())
        count = self._raw.readinto(buffer)
        self._remaining()
        return count

    def close(self):
        try:
            self._raw.close()
        finally:
            super().close()


class _ReceiptSocket:
    def __init__(self, sock, end):
        self._socket = sock
        self._end = end

    def makefile(self, mode):
        raw = self._socket.makefile(mode, buffering=0)
        return io.BufferedReader(_ReceiptRaw(raw, self._socket, self._end))


def _validate_receipt_field(line):
    name, separator, value = line[:-2].partition(b':')
    tokens = b"!#$%&'*+-.^_`|~0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
    if not separator or not name or any(byte not in tokens for byte in name):
        raise TraceExportRejected('invalid HTTP response field name')
    if any((byte < 32 and byte != 9) or byte == 127 for byte in value):
        raise TraceExportRejected('invalid HTTP response field value')


class _ReceiptHeadReader:
    """Bound and validate the head while http.client owns HTTP response parsing."""
    def __init__(self, source):
        self._source = source
        self._bytes = 0
        self._status = True

    def readline(self, limit=-1):
        line = self._source.readline(8193 if limit < 0 else min(limit, 8193))
        self._bytes += len(line)
        if len(line) > 8192 or self._bytes > 65536 or not line.endswith(b'\r\n'):
            raise TraceExportRejected('incomplete or oversized HTTP response head')
        if self._status:
            # http.client accepts signed/four-digit codes and whitespace that
            # is not part of HTTP's status-line grammar. These cannot confirm a batch.
            if _STATUS_LINE.fullmatch(line) is None:
                raise TraceExportRejected('invalid HTTP response status line')
            self._status = False
        elif line == b'\r\n':
            # Informational responses and the final response share one budget.
            self._status = True
        else:
            _validate_receipt_field(line)
        return line

    def __getattr__(self, name):
        return getattr(self._source, name)


class _ReceiptResponse(http.client.HTTPResponse):
    """Keep HTTP's compatibility leniency out of completion acknowledgements."""
    def _read_status(self):
        # HTTPResponse skips only 100. Other informational heads also precede
        # the final response and must consume the same byte and time budgets.
        while True:
            version, status, reason = super()._read_status()
            if not 100 <= status < 200:
                return version, status, reason
            if status == 101:
                raise TraceExportRejected('collector protocol switching is unsupported')
            headers = http.client.parse_headers(self.fp)
            if headers.defects or 'Content-Length' in headers or 'Transfer-Encoding' in headers:
                raise TraceExportRejected('invalid informational HTTP response head')

    def begin(self):
        if self.headers is not None:
            return
        source = self.fp
        bounded = _ReceiptHeadReader(source)
        self.fp = bounded
        try:
            super().begin()
            # The email-based parser may otherwise stop at a malformed field,
            # hiding later framing fields and accepting a truncated JSON body.
            if self.headers.defects:
                raise TraceExportRejected('malformed HTTP response head')
        finally:
            if self.fp is bounded:
                self.fp = source

    def _framing_line(self):
        line = self.fp.readline(8193)
        consumed = getattr(self, '_framing_bytes', 0) + len(line)
        self._framing_bytes = consumed
        if len(line) > 8192 or consumed > 65536 or not line.endswith(b'\r\n'):
            raise TraceExportRejected('incomplete or oversized HTTP framing')
        return line

    def _read_next_chunk_size(self):
        line = self._framing_line()
        parsed = _CHUNK_LINE.fullmatch(line[:-2])
        if parsed is None:
            raise TraceExportRejected('invalid HTTP chunk size or extension')
        size = parsed.group(1)
        # A declared chunk above the entire response budget cannot be accepted.
        size = size.lstrip(b'0') or b'0'
        if len(size) > 5 or int(size, 16) > 65536:
            raise TraceExportRejected('collector response exceeds byte bound')
        return int(size, 16)

    def _read_and_discard_trailer(self):
        for _ in range(101):
            line = self._framing_line()
            if line == b'\r\n':
                return
            _validate_receipt_field(line)
        raise TraceExportRejected('too many HTTP trailers')

    def _get_chunk_left(self):
        # HTTPResponse otherwise discards these two bytes without checking
        # that they are CRLF, and accepts EOF in place of the final blank line.
        remaining = self.chunk_left
        if not remaining:
            if remaining is not None and self._safe_read(2) != b'\r\n':
                raise TraceExportRejected('invalid HTTP chunk terminator')
            remaining = self._read_next_chunk_size()
            if remaining == 0:
                self._read_and_discard_trailer()
                self._close_conn()
                remaining = None
            self.chunk_left = remaining
        return remaining


def _read_receipt(response):
    encodings = response.headers.get_all('Content-Encoding', [])
    if encodings and (len(encodings) != 1 or encodings[0].strip().lower() != 'identity'):
        raise TraceExportRejected('unsupported HTTP content encoding')
    lengths = response.headers.get_all('Content-Length', [])
    codings = response.headers.get_all('Transfer-Encoding', [])
    expected = None
    if codings:
        if (lengths or len(codings) != 1 or codings[0].strip().lower() != 'chunked'
                or not response.chunked):
            raise TraceExportRejected('ambiguous or unsupported HTTP framing')
    elif lengths:
        value = lengths[0].strip()
        if (len(lengths) != 1 or not value.isascii() or not value.isdecimal()
                or len(value.lstrip('0')) > 5):
            raise TraceExportRejected('invalid HTTP content length')
        expected = int(value.lstrip('0') or '0')
        if expected > 65536:
            raise TraceExportRejected('collector response exceeds byte bound')
    body = response.read(65537)
    if len(body) > 65536:
        raise TraceExportRejected('collector response exceeds byte bound')
    if expected is not None and len(body) != expected:
        # read(amount) intentionally tolerates an early EOF in http.client.
        # Even a valid JSON prefix does not acknowledge an incomplete message.
        raise TraceExportRejected('incomplete HTTP response body')
    return body


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise TraceExportRejected('duplicate receipt field')
        result[key] = value
    return result


def _accepted(body, count):
    try:
        receipt = json.loads(body.decode('utf-8'), object_pairs_hook=_object)
    except (ValueError, UnicodeError) as error:
        raise TraceExportRejected('invalid OTLP JSON response') from error
    if type(receipt) is not dict or set(receipt) - {'partialSuccess'}:
        raise TraceExportRejected('invalid OTLP response fields')
    partial = receipt.get('partialSuccess', {})
    if partial is None:
        partial = {}
    if type(partial) is not dict or set(partial) - {'rejectedSpans', 'errorMessage'}:
        raise TraceExportRejected('invalid partial success fields')
    rejected = partial.get('rejectedSpans', '0')
    if rejected is None:
        rejected = '0'
    if type(rejected) is str and rejected.isascii() and rejected.isdecimal() and len(rejected) <= 19:
        rejected = int(rejected)
    if type(rejected) is not int or not 0 <= rejected <= count:
        raise TraceExportRejected('invalid rejected span count')
    if partial.get('errorMessage') is not None and type(partial['errorMessage']) is not str:
        raise TraceExportRejected('invalid partial success message')
    if partial.get('errorMessage') is not None:
        try:
            partial['errorMessage'].encode('utf-8')
        except UnicodeError as error:
            raise TraceExportRejected('invalid partial success Unicode') from error
    if rejected:
        # The response does not identify which spans were rejected. Keep the
        # entire batch; retries can duplicate already accepted identities.
        raise TraceExportRejected('collector partially rejected batch')


def _send(config, snapshot):
    address = urlsplit(config.endpoint)
    target = address.path or '/v1/traces'
    if address.query:
        target += '?' + address.query
    options = dict(host=address.hostname, port=address.port, timeout=config.request_timeout)
    if address.scheme == 'https':
        connection = http.client.HTTPSConnection(**options, context=ssl.create_default_context(cafile=config.ca_file))
    else:
        connection = http.client.HTTPConnection(**options)
    try:
        document = json.dumps(snapshot_otlp(snapshot), separators=(',', ':'), ensure_ascii=True).encode()
        if len(document) > 4 * 1024 * 1024:
            raise TraceExportRejected('trace batch exceeds byte bound')
        connection.request('POST', target, document, {'Content-Type': 'application/json', 'Accept': 'application/json'})
        # A socket inactivity timeout alone permits indefinitely slow receipt
        # delivery. Header, interim, body and trailer reads share this deadline.
        end = time.monotonic() + config.request_timeout
        connection.response_class = lambda sock, **kwargs: _ReceiptResponse(_ReceiptSocket(sock, end), **kwargs)
        with connection.getresponse() as response:
            if response.status != 200:
                raise TraceExportRejected('collector HTTP status ' + str(response.status))
            if response.getheader('Content-Type', '').split(';', 1)[0].strip().lower() != 'application/json':
                raise TraceExportRejected('collector did not return OTLP JSON')
            body = _read_receipt(response)
            _accepted(body, len(snapshot['records']))
            if time.monotonic() >= end:
                raise socket.timeout('collector response exceeded deadline')
    finally:
        connection.close()


def _deadline(timeout):
    if type(timeout) not in (int, float) or not math.isfinite(timeout) or not 0 < timeout <= 60:
        raise ValueError('export wait must be finite and in (0, 60] seconds')
    return time.monotonic() + timeout


class TraceExporter:
    def __init__(self, recorder, config):
        self._recorder = recorder
        self._config = config
        self._lock = threading.Lock()
        self._changed = threading.Event()
        self._thread = None
        self._error = None
        self._attempts = 0
        self._exported = 0

    def wake(self):
        self._changed.set()

    def start(self):
        with self._lock:
            if self._thread is not None and self._thread.is_alive():
                return
            self._error = None
            self._thread = threading.Thread(target=self._run, name='parrot-trace-export', daemon=False)
            self._thread.start()

    def _run(self):
        try:
            while True:
                self._changed.clear()
                snapshot = self._recorder.snapshot()
                snapshot['records'] = snapshot['records'][:self._config.batch_size]
                if not snapshot['records']:
                    if snapshot['closed'] and snapshot['active'] == 0:
                        return
                    self._changed.wait(self._config.interval)
                    continue
                for attempt in range(self._config.max_attempts):
                    with self._lock:
                        self._attempts += 1
                    try:
                        _send(self._config, snapshot)
                        break
                    except Exception:
                        if attempt + 1 == self._config.max_attempts:
                            raise
                        # New handler arrivals must not collapse retry backoff.
                        time.sleep(min(0.05 * 2 ** attempt, 1.0))
                ids = [record['span_id'] for record in snapshot['records']]
                self._recorder.acknowledge(ids, _owner=self)
                with self._lock:
                    self._exported += len(ids)
        except BaseException as error:
            with self._lock:
                # Do not leak endpoints, response bodies or application data.
                self._error = type(error).__name__[:128]
        finally:
            self.wake()

    def status(self):
        snapshot = self._recorder.snapshot()
        with self._lock:
            return dict(enabled=True, running=self._thread is not None and self._thread.is_alive(),
                        error=self._error, attempts=self._attempts, exported=self._exported,
                        pending=len(snapshot['records']), active=snapshot['active'],
                        dropped=snapshot['dropped'], closed=snapshot['closed'])

    def flush(self, timeout=5.0, path=None):
        end = _deadline(timeout)
        self.start()  # Explicit retry after a terminal worker failure, never a second live worker.
        self.wake()
        while True:
            snapshot = self._recorder.snapshot(path)
            status = self.status()
            ok = not snapshot['records'] and snapshot['active'] == 0
            if ok or status['error'] is not None or time.monotonic() >= end:
                return dict(status, ok=ok)
            time.sleep(min(0.01, max(0, end - time.monotonic())))

    def finish(self, timeout=5.0):
        end = _deadline(timeout)
        self._recorder.close()
        self.start()
        self.wake()
        with self._lock:
            thread = self._thread
        thread.join(max(0, end - time.monotonic()))
        status = self.status()
        return dict(status, ok=not status['running'] and status['pending'] == 0 and status['active'] == 0
                    and status['error'] is None)
