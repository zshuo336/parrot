"""Explicit causal parent transfer across Python threads and Ray processes.

Parent transfer is independent of recording. Explicitly configured native
actors additionally record real handlers through trace_recording; disabled
actors preserve the transferred identity without creating a span.
"""
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass
import inspect


@dataclass(frozen=True)
class TraceParent:
    trace_id: bytes
    span_id: bytes

    def __post_init__(self):
        for name, value, size in (("trace", self.trace_id, 16), ("span", self.span_id, 8)):
            if type(value) is not bytes or len(value) != size or not any(value):
                raise ValueError(f"invalid {name} identity")

    def encode(self):
        return b"\x01" + self.trace_id + self.span_id

    @classmethod
    def decode(cls, value):
        if type(value) is not bytes or len(value) != 25 or value[0] != 1:
            raise ValueError("invalid causal parent envelope")
        return cls(value[1:17], value[17:25])


_current = ContextVar("parrot_causal_parent", default=None)


def current_parent():
    return _current.get()


def capture_parent():
    parent = current_parent()
    return None if parent is None else parent.encode()


@contextmanager
def parent_scope(parent):
    if parent is not None and not isinstance(parent, TraceParent):
        raise TypeError("expected TraceParent or None")
    token = _current.set(parent)
    try:
        yield parent
    finally:
        _current.reset(token)


@contextmanager
def transferred_scope(encoded):
    # None is an explicit empty scope, including on a reused thread or actor.
    with parent_scope(None if encoded is None else TraceParent.decode(encoded)) as parent:
        yield parent




class RecordedActor:
    """Control methods inherited by the native causal mixins.

    Called by the owning runtime before publishing the actor. Reconfiguration
    is rejected so it cannot silently discard already recorded operations.
    """
    def parrot_trace_configure(self, config, path):
        if hasattr(self, '_parrot_recorder'):
            raise RuntimeError('actor recording already configured')
        from .trace_recording import TraceRecorder
        recorder = TraceRecorder(config)
        self._parrot_trace_path = path
        self._parrot_recorder = recorder
        return True

    def parrot_trace_snapshot(self):
        recorder = getattr(self, '_parrot_recorder', None)
        return dict(enabled=False) if recorder is None else recorder.snapshot()

    def parrot_trace_acknowledge(self, span_ids):
        recorder = getattr(self, '_parrot_recorder', None)
        if recorder is None:
            raise RuntimeError('recording disabled')
        return recorder.acknowledge(span_ids)

    def parrot_trace_export_status(self):
        recorder = getattr(self, '_parrot_recorder', None)
        return dict(enabled=False) if recorder is None else recorder.export_status()

    def parrot_trace_finish(self, timeout=5.0):
        recorder = getattr(self, '_parrot_recorder', None)
        return dict(ok=True, enabled=False) if recorder is None else recorder.finish_export(timeout)

    def parrot_trace_flush(self, timeout=5.0):
        recorder = getattr(self, '_parrot_recorder', None)
        return dict(ok=True, enabled=False) if recorder is None else recorder.flush_export(timeout)

    def _handler_scope(self, key, operation):
        from .trace_recording import handler_scope
        recorder = getattr(self, '_parrot_recorder', None)
        if recorder is not None and recorder.config.export is not None and recorder.closed:
            raise RuntimeError('native trace owner is retiring')
        return handler_scope(recorder,
                             getattr(self, '_parrot_trace_path', ''), key, operation)


class TraceActor(RecordedActor):
    """Mixin for native Ray actors exposing ask(key, payload) and optional deliver.

    Ray transports only the fixed-size validated parent bytes. User business
    signatures and payloads stay unchanged; exceptions still unwind the scope.
    """
    def ask_traced(self, key, payload, parent):
        with transferred_scope(parent), self._handler_scope(key, 'ask'):
            return self._trace_sync_result(self.ask(key, payload))

    def deliver_traced(self, key, payload, parent):
        with transferred_scope(parent), self._handler_scope(key, 'tell'):
            deliver = getattr(self, "deliver", None)
            return self._trace_sync_result(self.ask(key, payload) if deliver is None else deliver(key, payload))

    @staticmethod
    def _trace_sync_result(result):
        if inspect.isawaitable(result):
            if inspect.iscoroutine(result):
                result.close()
            raise TypeError("async Ray actors must use AsyncTraceActor")
        return result


class AsyncTraceActor(RecordedActor):
    """Async native actor counterpart; context remains scoped across awaits."""
    async def parrot_trace_finish(self, timeout=5.0):
        import asyncio
        # Export waits must not block the event loop needed by active handlers.
        return await asyncio.to_thread(super().parrot_trace_finish, timeout)

    async def parrot_trace_flush(self, timeout=5.0):
        import asyncio
        return await asyncio.to_thread(super().parrot_trace_flush, timeout)

    async def ask_traced(self, key, payload, parent):
        with transferred_scope(parent), self._handler_scope(key, 'ask'):
            result = self.ask(key, payload)
            return await result if inspect.isawaitable(result) else result

    async def deliver_traced(self, key, payload, parent):
        with transferred_scope(parent), self._handler_scope(key, 'tell'):
            deliver = getattr(self, "deliver", None)
            result = self.ask(key, payload) if deliver is None else deliver(key, payload)
            return await result if inspect.isawaitable(result) else result
