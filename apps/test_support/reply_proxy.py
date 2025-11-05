"""Owned fault proxy for real application tests; never changes handshake/admin frames."""
import socket
import struct
import threading

class CorruptFirstReply:
    """Real TCP proxy changes only the first matching application reply."""
    def __init__(self, upstream, key=b'bin:crawl/FrontierBatch', payload=struct.pack('<I', 1), replacement_key=None):
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.listener.listen()
        self.listener.settimeout(.2)
        self.port = self.listener.getsockname()[1]
        self.upstream = upstream
        self.key = key
        self.payload = payload
        self.replacement_key = replacement_key
        self.sockets = []
        self.threads = []
        self.lock = threading.Lock()
        self.stopping = threading.Event()
        self.corrupted = 0
        self.errors = []
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    @staticmethod
    def exact(sock, size):
        data = bytearray()
        while len(data) < size:
            part = sock.recv(size-len(data))
            if not part:
                raise EOFError()
            data.extend(part)
        return bytes(data)

    def pump(self, source, target, corrupt):
        try:
            while not self.stopping.is_set():
                prefix = self.exact(source, 4)
                size, = struct.unpack('<I', prefix)
                if not 28 <= size <= 16*1024*1024:
                    raise ValueError(f'bad frame size {size}')
                body = self.exact(source, size)
                if corrupt and body[1] == 0x11:
                    path_size, = struct.unpack_from('<I', body, 20)
                    key_size, = struct.unpack_from('<I', body, 24+path_size)
                    key_start = 28+path_size
                    payload_start = key_start+key_size
                    if body[key_start:payload_start] == self.key and self.corrupted == 0:
                        key = self.replacement_key or self.key
                        body = body[:24+path_size] + struct.pack('<I', len(key)) + key + self.payload
                        prefix = struct.pack('<I', len(body))
                        self.corrupted += 1
                target.sendall(prefix+body)
        except (EOFError, OSError):
            pass
        except Exception as error:
            self.errors.append(str(error))
        finally:
            for sock in (source, target):
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def accept(self):
        try:
            while not self.stopping.is_set():
                try:
                    client, _ = self.listener.accept()
                except socket.timeout:
                    continue
                server = socket.create_connection(('127.0.0.1', self.upstream), timeout=5)
                server.settimeout(None)
                with self.lock:
                    self.sockets.extend([client, server])
                    for args in [(client, server, False), (server, client, True)]:
                        worker = threading.Thread(target=self.pump, args=args, daemon=True)
                        self.threads.append(worker)
                        worker.start()
        except OSError:
            if not self.stopping.is_set():
                self.errors.append('proxy accept failed')

    def close(self):
        self.stopping.set()
        self.listener.close()
        self.thread.join(timeout=6)
        with self.lock:
            for sock in self.sockets:
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                sock.close()
            workers = list(self.threads)
        for worker in workers:
            worker.join(timeout=3)
        if self.thread.is_alive() or any(worker.is_alive() for worker in workers):
            raise AssertionError('proxy did not retire')
        if self.errors:
            raise AssertionError(self.errors)

