"""Local POC wire gateway backed by a real, privately owned Ray worker."""
import signal
import socket
import struct
import sys
from pathlib import Path
import ray

ASK, REPLY, REPLY_ERR, TELL = 0x10, 0x11, 0x12, 0x13
MAX_BODY = 1024 * 1024


@ray.remote
class EchoWorker:
    def __init__(self):
        self.tells = 0

    def handle(self, key, payload):
        if key == 'bin:u:TellCount' and payload == b'':
            return 'bin:u:AddR', struct.pack('<Q', self.tells)
        if key == 'bin:u:Ping':
            n, = struct.unpack('<Q', payload)
            return 'bin:u:Pong', struct.pack('<Q', (n + 2) % (1 << 64))
        if key == 'bin:u:Add':
            a, b = struct.unpack('<QQ', payload)
            return 'bin:u:AddR', struct.pack('<Q', (a + b + 1000) % (1 << 64))
        raise ValueError(f'unknown_service: {key}')

    def tell(self, key, payload):
        if key not in ('bin:u:Ping', 'bin:u:Add'):
            raise ValueError('unknown_service')
        self.handle(key, payload)
        self.tells += 1


def build_frame(ft, cid, path, key, payload):
    body_len = 28 + len(path) + len(key) + len(payload)
    if not 28 <= body_len <= MAX_BODY:
        raise ValueError('invalid frame length')
    return (struct.pack('<IBBHQQI', body_len, 1, ft, 0, cid, 0, len(path)) + path
            + struct.pack('<I', len(key)) + key + payload)


def recv_exact(sock, size):
    buf = bytearray()
    while len(buf) < size:
        data = sock.recv(size - len(buf))
        if not data:
            raise EOFError('connection closed')
        buf.extend(data)
    return bytes(buf)


def read_frame(sock):
    size, = struct.unpack('<I', recv_exact(sock, 4))
    if not 28 <= size <= MAX_BODY:
        raise ValueError('invalid frame length')
    body = recv_exact(sock, size)
    ver, ft, flags, cid, reserved, path_size = struct.unpack_from('<BBHQQI', body)
    if ver != 1 or not ASK <= ft <= 0x14 or flags or reserved:
        raise ValueError('unsupported frame header')
    if path_size > size - 28:
        raise ValueError('invalid path length')
    off = 24 + path_size
    key_size, = struct.unpack_from('<I', body, off)
    if key_size > size - off - 4:
        raise ValueError('invalid key length')
    return (ft, cid, body[24:off].decode('utf8'),
            body[off+4:off+4+key_size].decode('utf8'), body[off+4+key_size:])


def main(port, runtime):
    runtime = str(Path(runtime).resolve())
    def stop(*_):
        raise SystemExit(0)
    try:
        ray.init(address='local', _temp_dir=runtime, num_cpus=2,
                 include_dashboard=False, log_to_driver=False)
        signal.signal(signal.SIGTERM, stop)
        signal.signal(signal.SIGINT, stop)
        worker = EchoWorker.remote()
        # Actual worker round trip before advertising the bound listener.
        ray.get(worker.handle.remote('bin:u:TellCount', b''), timeout=30)
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', port))
            listener.listen(1)
            print(f'POC_PORT {listener.getsockname()[1]}', flush=True)
            sock, _ = listener.accept()
            with sock:
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                pending = {}
                next_cid = 1
                def send(ft, cid, key, payload, path=b''):
                    sock.sendall(build_frame(ft, cid, path, key, payload))
                while True:
                    try:
                        ft, cid, _, key, payload = read_frame(sock)
                    except EOFError:
                        break
                    if ft == ASK:
                        try:
                            if key == 'bin:u:Probe' and payload == b'':
                                if len(pending) >= 128:
                                    raise ValueError('pending limit')
                                pending[next_cid] = cid
                                send(ASK, next_cid, b'bin:u:Ping', struct.pack('<Q', 100),
                                     b'/user/rust_service')
                                next_cid += 1
                            else:
                                reply_key, reply = ray.get(worker.handle.remote(key, payload), timeout=10)
                                send(REPLY, cid, reply_key.encode(), reply)
                        except Exception as error:
                            send(REPLY_ERR, cid, b'', str(error).encode()[:4096])
                    elif ft == TELL:
                        try:
                            # Wait for real worker completion. The next request
                            # is a barrier and cannot overtake this operation.
                            ray.get(worker.tell.remote(key, payload), timeout=10)
                        except Exception:
                            pass
                    elif ft in (REPLY, REPLY_ERR):
                        original = pending.pop(cid, None)
                        if original is not None:
                            if ft == REPLY and key == 'bin:u:Pong' and len(payload) == 8:
                                send(REPLY, original, b'bin:u:AddR', payload)
                            else:
                                send(REPLY_ERR, original, b'', b'invalid reverse reply')
    finally:
        ray.shutdown()


if __name__ == '__main__':
    main(int(sys.argv[1]), sys.argv[2])
