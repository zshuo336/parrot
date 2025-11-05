"""Real pipes/processes verify bounded output, useful tails and reader retirement."""
import os
import subprocess
import sys
import threading
import time
import types

import pytest

from parrot_protocol import job_output, ray_job
from parrot_protocol.job_output import BoundedOutput, MAX_OUTPUT_BYTES
from test_ray_job import job


def wait_for(predicate, message):
    deadline = time.monotonic()+5
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError(message)
        time.sleep(.005)


def test_real_child_can_write_far_beyond_capacity_without_disk_or_backpressure_deadlock():
    output = BoundedOutput()
    script = "import os\nfor _ in range(64):os.write(1,b'a'*65536)\nos.write(2,b'FINAL-ERROR\\n')\n"
    try:
        child = subprocess.Popen([sys.executable, '-c', script], stdout=output, stderr=subprocess.STDOUT)
        output.close_writer()
        assert child.wait(timeout=10) == 0
        wait_for(lambda: output.closed, 'log pipe did not reach EOF')
        assert output.retained_bytes == MAX_OUTPUT_BYTES
        assert output.bytes_received == 64*65536+12
        assert output.tail(MAX_OUTPUT_BYTES) == 'a'*(MAX_OUTPUT_BYTES-12)+'FINAL-ERROR\n'
        assert output.tail(0) == ''
    finally:
        if 'child' in locals() and child.poll() is None:
            child.kill(); child.wait(timeout=5)
        output.close()
    assert output.closed and not output._thread.is_alive()


def test_inherited_writer_cannot_hold_reader_retirement_open():
    output = BoundedOutput(); inherited = os.dup(output.fileno())
    reader = output._reader
    try:
        output.close(deadline=time.monotonic()+1)
        assert output.closed
        with pytest.raises(OSError): os.fstat(reader)
        with pytest.raises(BrokenPipeError): os.write(inherited, b'orphan output')
        output.close()
    finally:
        os.close(inherited); output.close()


def test_expired_reader_deadline_is_retryable(monkeypatch):
    output = BoundedOutput()
    owned = job()
    owned.log = output
    owned.process = types.SimpleNamespace(poll=lambda: 0)
    entered, release = threading.Event(), threading.Event()
    original = os.read
    def stalled_read(descriptor, size):
        data = original(descriptor, size)
        if descriptor == output._reader:
            entered.set()
            if not release.wait(5): raise OSError('fixture release missing')
        return data
    monkeypatch.setattr(job_output.os, 'read', stalled_read)
    try:
        os.write(output.fileno(), b'pending')
        assert entered.wait(5)
        with pytest.raises(TimeoutError, match='reader'):
            owned.close(deadline=time.monotonic()-1)
        assert not output.closed and owned.log is output
    finally:
        release.set()
        owned.close()
    assert output.closed and owned.log is None


def test_thread_start_failure_closes_both_new_descriptors(monkeypatch):
    descriptors = []
    original = os.pipe
    def pipe():
        descriptors.extend(original()); return tuple(descriptors)
    def fail(self): raise RuntimeError('reader start failed')
    monkeypatch.setattr(job_output.os, 'pipe', pipe)
    monkeypatch.setattr(job_output.threading.Thread, 'start', fail)
    with pytest.raises(RuntimeError, match='reader start'): BoundedOutput()
    for descriptor in descriptors:
        with pytest.raises(OSError): os.fstat(descriptor)


def test_capture_failure_is_diagnostic_and_still_retires_pipe(monkeypatch):
    def fail(): raise OSError('selector unavailable')
    monkeypatch.setattr(job_output.selectors, 'DefaultSelector', fail)
    output = BoundedOutput()
    try:
        wait_for(lambda: not output._thread.is_alive(), 'reader failure did not terminate')
        assert 'log capture failed: selector unavailable' in output.tail()
    finally: output.close()
    assert output.closed


@pytest.mark.parametrize('size', [-1, MAX_OUTPUT_BYTES+1, 1.5])
def test_invalid_tail_size_is_rejected(size):
    output = BoundedOutput()
    try:
        with pytest.raises(ValueError, match='tail size'): output.tail(size)
    finally: output.close()


CHILD = '''import os,socket,sys
from parrot_protocol.ray_job import receive,send
c=socket.socket(fileno=int(sys.argv[1]));config=receive(c)
for _ in range(32):os.write(1,b'x'*65536)
os.write(2,b'FINAL-DIAGNOSTIC\\n')
if sys.argv[2]=='success':
    send(c,dict(job='02000000',namespace=config['namespace']))
    assert receive(c) is None
'''


@pytest.mark.parametrize('mode', ['success', 'startup-error'])
def test_actual_deployment_job_captures_bounded_output_and_retires_reader(monkeypatch, mode):
    original = subprocess.Popen
    def launch(command, **kwargs):
        return original([sys.executable, '-c', CHILD, command[3], mode], **kwargs)
    monkeypatch.setattr(ray_job.subprocess, 'Popen', launch)
    owned = job()
    try:
        if mode == 'success':
            assert owned.start() == 'host'
        else:
            with pytest.raises(RuntimeError, match='FINAL-DIAGNOSTIC'):
                owned.start()
        output = owned.log
        if mode == 'startup-error':
            wait_for(lambda: output.closed, 'parent writer masked driver EOF')
        wait_for(lambda: output.bytes_received == 32*65536+17, 'driver output was not drained')
        assert output.tail(MAX_OUTPUT_BYTES) == 'x'*(MAX_OUTPUT_BYTES-17)+'FINAL-DIAGNOSTIC\n'
    finally:
        owned.close()
    assert owned.process.poll() == 0 and owned.log is None and output.closed
