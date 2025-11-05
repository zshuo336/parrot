"""Connection ownership without requiring a Ray installation."""
import socket
import threading

from parrot_protocol.ray_gw import _ConnectionOutbox


def test_idle_writer_is_joined_and_late_replies_are_discarded():
    local, peer = socket.socketpair()
    out = _ConnectionOutbox(local)
    try:
        out.put(b"ready")
        peer.settimeout(2)
        assert peer.recv(5) == b"ready"
        out.close()
        assert not out._thread.is_alive()
        for _ in range(100):
            out.put(b"late")
        assert out._queue.qsize() <= 1  # At most the terminal sentinel.
        assert peer.recv(1) == b""
        out.close()  # Idempotent cleanup.
    finally:
        out.close()
        local.close()
        peer.close()


def test_failed_writer_ends_session_without_thread_exception():
    local, peer = socket.socketpair()
    out = _ConnectionOutbox(local)
    try:
        peer.close()
        out.put(b"reply")
        out._thread.join(2)
        assert not out._thread.is_alive()
        assert out._closed
        out.put(b"late reply")
        assert out._queue.qsize() <= 1
    finally:
        out.close()
        local.close()


def test_shutdown_unblocks_writer_when_peer_is_not_reading():
    local, peer = socket.socketpair()
    local.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
    out = _ConnectionOutbox(local)
    try:
        out.put(b"x" * (4 * 1024 * 1024))
        cleanup = threading.Thread(target=out.close, daemon=True)
        cleanup.start()
        cleanup.join(2)
        assert not cleanup.is_alive(), "shutdown must unblock a saturated socket writer"
        assert not out._thread.is_alive()
    finally:
        peer.close()
        out.close()
        local.close()


class HeldSocket:
    """Hold the real write at entry; shutdown releases it into the closed socket."""
    def __init__(self, local):
        self.local = local
        self.entered = threading.Event()
        self.release = threading.Event()

    def sendall(self, frame):
        self.entered.set()
        assert self.release.wait(5), 'test did not release owned writer'
        self.local.sendall(frame)

    def shutdown(self, how):
        try:
            self.local.shutdown(how)
        finally:
            self.release.set()


def test_pending_frame_limit_includes_the_write_already_in_progress():
    from parrot_protocol import wire
    local, peer = socket.socketpair()
    held = HeldSocket(local)
    out = _ConnectionOutbox(held)
    frame = wire.build_frame(wire.FT_REPLY, 1, '', 'bin:reply', b'x')
    try:
        out.put(frame)
        assert held.entered.wait(2)
        for _ in range(255):
            out.put(frame)
        assert not out._closed, '256 outstanding frames must fit'
        out.put(frame)
        assert out._closed, 'slow peer retained more than 256 owned output frames'
        out.close()
        assert not out._thread.is_alive()
        assert out._queue.qsize() <= 1
        assert peer.recv(1) == b''
    finally:
        out.close(); local.close(); peer.close()


def test_pending_byte_limit_includes_the_write_already_in_progress():
    from parrot_protocol import wire
    local, peer = socket.socketpair()
    held = HeldSocket(local)
    out = _ConnectionOutbox(held)
    # Two independently encoded 8 MiB wire frames exactly fill 16 MiB.
    frame = wire.build_frame(wire.FT_REPLY, 1, '', '', b'x' * (8 * 1024 * 1024 - 32))
    assert len(frame) == 8 * 1024 * 1024
    try:
        out.put(frame)
        assert held.entered.wait(2)
        out.put(frame)
        assert not out._closed, '16 MiB of outstanding wire bytes must fit'
        out.put(wire.build_frame(wire.FT_REPLY, 2, '', '', b'x'))
        assert out._closed, 'slow peer retained more than 16 MiB of owned output bytes'
        out.close()
        assert not out._thread.is_alive()
        assert out._queue.qsize() <= 1
        assert peer.recv(1) == b''
    finally:
        out.close(); local.close(); peer.close()


def test_successful_writes_release_capacity_for_later_full_batches():
    import time
    from parrot_protocol import wire
    local, peer = socket.socketpair()
    peer.settimeout(2)
    held = HeldSocket(local)
    out = _ConnectionOutbox(held)
    try:
        for batch in range(2):
            held.entered.clear(); held.release.clear()
            frames = [wire.build_frame(wire.FT_REPLY, batch * 256 + n + 1, '', '', b'x') for n in range(256)]
            assert out.put(frames[0])
            assert held.entered.wait(2)
            for frame in frames[1:]:
                assert out.put(frame)
            held.release.set()
            expected = b''.join(frames)
            received = bytearray()
            while len(received) < len(expected):
                chunk = peer.recv(len(expected) - len(received))
                assert chunk, 'connection closed despite available recycled capacity'
                received.extend(chunk)
            assert received == expected
            deadline = time.monotonic() + 2
            while out._pending_frames and time.monotonic() < deadline:
                threading.Event().wait(0.001)
            assert out._pending_frames == out._pending_bytes == 0
            assert not out._closed
    finally:
        out.close(); local.close(); peer.close()


def test_concurrent_producers_admit_only_one_shared_capacity_and_cleanup_once():
    from concurrent.futures import ThreadPoolExecutor
    from parrot_protocol import wire
    local, peer = socket.socketpair()
    held = HeldSocket(local)
    out = _ConnectionOutbox(held)
    frame = wire.build_frame(wire.FT_REPLY, 1, '', '', b'x')
    try:
        assert out.put(frame)
        assert held.entered.wait(2)
        barrier = threading.Barrier(16)

        def produce(_):
            barrier.wait(timeout=2)
            return sum(out.put(frame) for _ in range(32))

        with ThreadPoolExecutor(max_workers=16) as pool:
            admitted = sum(pool.map(produce, range(16)))
        assert admitted == 255
        assert out._closed
        out.close()
        assert out._pending_frames == out._pending_bytes == 0
        assert not out.put(frame), 'retired connection accepted a late reply'
        assert peer.recv(1) == b''
    finally:
        out.close(); local.close(); peer.close()


def test_peer_frame_limit_rejection_discards_owned_output_without_accounting_leak():
    from parrot_protocol import wire
    local, peer = socket.socketpair()
    held = HeldSocket(local)
    out = _ConnectionOutbox(held)
    out.max_frame_len = wire.BODY_FIXED_OVERHEAD
    empty = wire.build_frame(wire.FT_REPLY, 1, '', '', b'')
    try:
        assert out.put(empty)
        assert held.entered.wait(2)
        assert out.put(empty)
        assert not out.put(wire.build_frame(wire.FT_REPLY, 2, '', '', b'x'))
        out.close()
        assert out._pending_frames == out._pending_bytes == 0
        assert peer.recv(1) == b''
    finally:
        out.close(); local.close(); peer.close()
