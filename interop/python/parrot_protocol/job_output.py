"""Bounded diagnostic output for an owned subprocess, without a disk spool.

One reader thread drains stdout/stderr while the child runs. It retains only the
last 64 KiB and can stop even when an inherited writer never closes its pipe.
"""
import os
import selectors
import threading
import time

MAX_OUTPUT_BYTES = 64 * 1024
READ_BYTES = 64 * 1024


class BoundedOutput:
    def __init__(self):
        self._tail = bytearray()
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._drain_deadline = None
        self._error = None
        self.bytes_received = 0
        self._reader, writer = os.pipe()
        try:
            self._writer = os.fdopen(writer, 'wb', buffering=0)
        except BaseException:
            os.close(writer)
            os.close(self._reader)
            raise
        try:
            os.set_blocking(self._reader, False)
            self._thread = threading.Thread(target=self._drain, name='parrot-deployment-log', daemon=True)
            self._thread.start()
        except BaseException:
            self._writer.close()
            os.close(self._reader)
            raise

    def fileno(self):
        return self._writer.fileno()

    def close_writer(self):
        # Popen has duplicated this endpoint into the child's stdout/stderr.
        # The parent must not keep EOF hidden after that child exits.
        self._writer.close()

    def _drain(self):
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(self._reader, selectors.EVENT_READ)
                while True:
                    closing = self._stop.is_set()
                    if closing and time.monotonic() >= self._drain_deadline:
                        return
                    if not closing and not selector.select(timeout=.1):
                        continue
                    try:
                        data = os.read(self._reader, READ_BYTES)
                    except BlockingIOError:
                        if closing:
                            return
                        continue
                    except InterruptedError:
                        continue
                    if not data:
                        return
                    with self._lock:
                        self.bytes_received += len(data)
                        self._tail.extend(data)
                        del self._tail[:-MAX_OUTPUT_BYTES]
        except (OSError, ValueError) as error:
            # Keep the diagnostic failure visible without an unhandled thread
            # exception or leaving a pipe/reader that can never be retired.
            with self._lock:
                self._error = str(error)[:1024]
        finally:
            os.close(self._reader)

    def tail(self, size=8192):
        if not isinstance(size, int) or not 0 <= size <= MAX_OUTPUT_BYTES:
            raise ValueError('invalid diagnostic tail size')
        with self._lock:
            result = bytes(self._tail[-size:]) if size else b''
            error = self._error
        text = result.decode('utf-8', errors='replace')
        return (('[log capture failed: ' + error + ']\n') if error else '') + text

    @property
    def retained_bytes(self):
        with self._lock:
            return len(self._tail)

    @property
    def closed(self):
        return self._writer.closed and not self._thread.is_alive()

    def close(self, deadline=None):
        self.close_writer()
        end = time.monotonic() + 1 if deadline is None else deadline
        # Drain bytes already available, without waiting for an inherited writer
        # to close or letting continuous output overrun the caller's deadline.
        self._drain_deadline = end
        self._stop.set()
        self._thread.join(timeout=max(0, end-time.monotonic()))
        if self._thread.is_alive():
            raise TimeoutError('deployment log reader has not exited')
