"""Bounded, process-local handler recording and OTLP/JSON snapshots.

Configuration belongs to the embedding operator, never to a wire envelope.
Snapshots are non-destructive: exporting one does not acknowledge persistence.
"""
from contextlib import contextmanager
from dataclasses import dataclass
import asyncio
import math
import secrets
import threading
import time
from urllib.parse import urlsplit

from .trace import TraceParent, current_parent, parent_scope


@dataclass(frozen=True)
class TraceExportConfig:
    endpoint: str
    interval: float = 0.2
    request_timeout: float = 2.0
    max_attempts: int = 3
    batch_size: int = 64
    ca_file: str = None

    def __post_init__(self):
        if type(self.endpoint) is not str or len(self.endpoint) > 2048 or any(ord(c) <= 32 or ord(c) >= 127 for c in self.endpoint):
            raise ValueError('expected bounded ASCII collector URL')
        address = urlsplit(self.endpoint)
        if address.scheme not in ('http', 'https') or not address.hostname or address.username is not None or address.password is not None or address.fragment:
            raise ValueError('collector URL must be HTTP(S), without credentials or fragment')
        if address.port is not None and not 1 <= address.port <= 65535:
            raise ValueError('invalid collector port')
        for name, value, minimum, maximum in [('interval', self.interval, 0.01, 60),
                                               ('request_timeout', self.request_timeout, 0.01, 30)]:
            if type(value) not in (int, float) or not math.isfinite(value) or not minimum <= value <= maximum:
                raise ValueError('invalid trace export ' + name)
        for name, value, maximum in [('max_attempts', self.max_attempts, 10), ('batch_size', self.batch_size, 128)]:
            if type(value) is not int or not 1 <= value <= maximum:
                raise ValueError('invalid trace export ' + name)
        if self.ca_file is not None and (type(self.ca_file) is not str or not self.ca_file or '\x00' in self.ca_file):
            raise ValueError('invalid collector CA file')


@dataclass(frozen=True)
class TraceConfig:
    service_name: str = "parrot-python"
    capacity: int = 256
    export: TraceExportConfig = None

    def __post_init__(self):
        if type(self.service_name) is not str or not self.service_name or len(self.service_name.encode('utf-8')) > 256:
            raise ValueError('service_name must contain 1..256 UTF-8 bytes')
        if type(self.capacity) is not int or not 1 <= self.capacity <= 4096:
            raise ValueError('trace capacity must be an integer in 1..4096')
        if self.export is not None and not isinstance(self.export, TraceExportConfig):
            raise TypeError('expected TraceExportConfig')


def _identity(size):
    while True:
        value = secrets.token_bytes(size)
        if any(value):
            return value


def _bounded(value, limit=256):
    return str(value).encode('utf-8', errors='replace')[:limit].decode('utf-8', errors='ignore')


class TraceRecorder:
    def __init__(self, config):
        if not isinstance(config, TraceConfig):
            raise TypeError('expected TraceConfig')
        self.config = config
        self._lock = threading.Lock()
        self._records = []
        self._active = 0
        self._recording = 0
        self._dropped = 0
        self._acknowledged = 0
        self._closed = False
        self._exporter = None
        if config.export is not None:
            from .trace_export import TraceExporter
            self._exporter = TraceExporter(self, config.export)
            self._exporter.start()

    def __getstate__(self):
        raise TypeError('transfer TraceConfig; construct TraceRecorder inside its owning process')

    def close(self):
        """Stop admissions; admitted handlers still finish and remain visible."""
        with self._lock:
            self._closed = True
        if self._exporter is not None:
            self._exporter.wake()

    @property
    def closed(self):
        with self._lock:
            return self._closed

    def export_status(self):
        return dict(enabled=False) if self._exporter is None else self._exporter.status()

    def finish_export(self, timeout=5.0):
        if self._exporter is None:
            return dict(ok=True, enabled=False)
        return self._exporter.finish(timeout)

    def flush_export(self, timeout=5.0, path=None):
        if self._exporter is None:
            return dict(ok=True, enabled=False)
        return self._exporter.flush(timeout, path)

    @contextmanager
    def handler(self, path, key, operation):
        parent = current_parent()
        with self._lock:
            if self._closed and self.config.export is not None:
                raise RuntimeError('trace owner is retiring')
            admitted = not self._closed and len(self._records) + self._recording < self.config.capacity
            # Capacity controls retained spans, not responsibility for running
            # business. Overflow handlers still belong to this owner until
            # their scopes exit, even though they do not allocate a record.
            self._active += 1
            if admitted:
                self._recording += 1
            else:
                self._dropped += 1
        if not admitted:
            # Do not manufacture an unrecorded child identity on overflow.
            try:
                yield parent
            finally:
                with self._lock:
                    self._active -= 1
                if self._exporter is not None:
                    self._exporter.wake()
            return
        record = None
        try:
            identity = TraceParent(parent.trace_id if parent else _identity(16), _identity(8))
            start = time.time_ns()
            clock = time.monotonic_ns()
            outcome, error_type = 'ok', None
            try:
                with parent_scope(identity):
                    yield identity
            except BaseException as error:
                outcome = 'cancelled' if isinstance(error, asyncio.CancelledError) else 'error'
                error_type = _bounded(type(error).__name__)
                raise
            finally:
                record = dict(trace_id=identity.trace_id.hex(), span_id=identity.span_id.hex(),
                              parent_span_id=None if parent is None else parent.span_id.hex(),
                              start=start, end=start + max(0, time.monotonic_ns() - clock),
                              path=_bounded(path, 2048), key=_bounded(key), operation=_bounded(operation),
                              outcome=outcome, error_type=error_type)
        finally:
            with self._lock:
                self._active -= 1
                self._recording -= 1
                if record is None:
                    self._dropped += 1
                else:
                    self._records.append(record)
            if self._exporter is not None:
                self._exporter.wake()

    def snapshot(self, path=None):
        with self._lock:
            records = [dict(record) for record in self._records if path is None or record['path'] == path]
            return dict(enabled=True, records=records, active=self._active,
                        dropped=self._dropped, closed=self._closed, capacity=self.config.capacity,
                        acknowledged=self._acknowledged, counter_scope='recorder', path_filter=path,
                        service_name=self.config.service_name)

    def acknowledge(self, span_ids, path=None, *, _owner=None):
        """Release only explicitly persisted completed spans; validation is atomic."""
        if self._exporter is not None and _owner is not self._exporter:
            raise RuntimeError('continuous exporter owns acknowledgements')
        if type(span_ids) is not list or len(span_ids) > self.config.capacity or any(type(v) is not str for v in span_ids):
            raise ValueError('expected bounded list of completed span IDs')
        selected = set(span_ids)
        if len(selected) != len(span_ids):
            raise ValueError('duplicate span acknowledgement')
        with self._lock:
            available = {r['span_id'] for r in self._records if path is None or r['path'] == path}
            if not selected <= available:
                raise ValueError('span acknowledgement does not belong to completed snapshot')
            self._records = [r for r in self._records if r['span_id'] not in selected]
            self._acknowledged += len(selected)
            return len(selected)

    def otlp(self):
        return snapshot_otlp(self.snapshot())


def snapshot_otlp(snapshot):
    """Return an OTLP ExportTraceServiceRequest JSON object, not an upload receipt."""
    spans = []
    for record in snapshot['records']:
        attributes = [dict(key='parrot.' + key, value=dict(stringValue=record[key]))
                      for key in ('path', 'key', 'operation', 'outcome')]
        if record['error_type'] is not None:
            attributes.append(dict(key='error.type', value=dict(stringValue=record['error_type'])))
        span = dict(traceId=record['trace_id'], spanId=record['span_id'],
                    name='parrot.handler.' + record['operation'], kind=2,
                    startTimeUnixNano=str(record['start']), endTimeUnixNano=str(record['end']),
                    attributes=attributes, status=dict(code=1 if record['outcome'] == 'ok' else 2))
        if record['parent_span_id'] is not None:
            span['parentSpanId'] = record['parent_span_id']
        spans.append(span)
    return dict(resourceSpans=[dict(resource=dict(attributes=[dict(key='service.name',
                value=dict(stringValue=snapshot['service_name']))]),
                scopeSpans=[dict(scope=dict(name='parrot.python'), spans=spans)])])


@contextmanager
def handler_scope(recorder, path, key, operation):
    if recorder is None:
        yield current_parent()
    else:
        with recorder.handler(path, key, operation) as parent:
            yield parent
