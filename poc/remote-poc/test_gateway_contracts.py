"""Real POC gateway stream and process-ownership contracts (no skips).

Run from the repository root: python3 poc/remote-poc/test_gateway_contracts.py -v
Requires JDK, Erlang and Ray, including Ray's bundled process inspector.
"""
import contextlib
import json
import os
from pathlib import Path
import queue
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
import unittest
from ray._private.services import psutil

BASE = Path(__file__).resolve().parent
RECEIPTS = []


def wait_for(predicate, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.02)
    raise AssertionError('condition did not complete before deadline')


def frame(ft=0x10, cid=1, key=b'bin:u:Ping', payload=struct.pack('<Q', 39)):
    path = b'/user/service'
    body = struct.pack('<BBHQQI', 1, ft, 0, cid, 0, len(path)) + path
    body += struct.pack('<I', len(key)) + key + payload
    return struct.pack('<I', len(body)) + body


def exact(sock, n):
    data = bytearray()
    while len(data) < n:
        part = sock.recv(n - len(data))
        if not part:
            raise EOFError('unexpected EOF')
        data.extend(part)
    return bytes(data)


def reply(sock):
    size, = struct.unpack('<I', exact(sock, 4))
    if not 28 <= size <= 1048576:
        raise AssertionError(f'invalid reply size {size}')
    body = exact(sock, size)
    ver, ft, flags, cid, reserved, plen = struct.unpack_from('<BBHQQI', body)
    if (ver, flags, reserved) != (1,0,0):
        raise AssertionError('invalid response header')
    off = 24 + plen
    klen, = struct.unpack_from('<I', body, off)
    return ft, cid, body[off+4:off+4+klen], body[off+4+klen:]


class Owner:
    def __init__(self, kind):
        self.kind = kind
        self.observed = {}
        self.port = None
        self.output = queue.Queue()
        self.log = []
        self.child = subprocess.Popen([sys.executable, str(BASE/'owned_gateway.py'), kind],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            env=dict(os.environ, RAY_ADDRESS='127.0.0.1:1'), text=True)
        self.identity = psutil.Process(self.child.pid)
        def read():
            for line in self.child.stdout:
                self.log.append(line.rstrip())
                self.output.put(line.rstrip())
        self.reader = threading.Thread(target=read)
        self.reader.start()

    def sample(self):
        with contextlib.suppress(psutil.NoSuchProcess):
            for process in self.identity.children(recursive=True):
                with contextlib.suppress(psutil.NoSuchProcess, ProcessLookupError):
                    self.observed[(process.pid, process.create_time())] = process

    def ready(self):
        deadline = time.monotonic() + 100
        while time.monotonic() < deadline:
            self.sample()
            try:
                line = self.output.get(timeout=.025)
            except queue.Empty:
                if self.child.poll() is not None:
                    raise AssertionError('\n'.join(self.log))
                continue
            if line.startswith('PORT '):
                self.port = int(line[5:])
                if not 1 <= self.port <= 65535:
                    raise AssertionError('invalid bound port')
                self.sample()
                return self
        raise AssertionError('gateway startup timeout')

    def survivors(self):
        alive = []
        for identity, process in self.observed.items():
            with contextlib.suppress(psutil.NoSuchProcess):
                if process.is_running() and process.status() != psutil.STATUS_ZOMBIE:
                    alive.append(identity)
        return alive

    def close(self, kill=False, expect_success=True):
        self.sample()
        if kill and self.child.poll() is None:
            self.child.kill()
        if self.child.stdin is not None:
            self.child.stdin.close()
            self.child.stdin = None
        try:
            status = self.child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.child.kill()
            self.child.wait(timeout=5)
            raise AssertionError('manager did not retire on owner EOF')
        finally:
            self.reader.join(timeout=5)
            self.child.stdout.close()
        if self.reader.is_alive():
            raise AssertionError('stdout reader survived owner retirement')
        wait_for(lambda: not self.survivors())
        if self.port is not None:
            with socket.socket() as probe:
                probe.settimeout(1)
                if probe.connect_ex(('127.0.0.1', self.port)) == 0:
                    raise AssertionError('gateway listener survived')
        RECEIPTS.append(dict(kind=self.kind, manager_status=status, port=self.port,
            observed_processes=[dict(pid=p, created=c) for p,c in self.observed], survivors=self.survivors()))
        if expect_success and not kill and status != 0:
            raise AssertionError('\n'.join(self.log))


class Contracts(unittest.TestCase):
    def owned(self, kind):
        owner = Owner(kind)
        try:
            owner.ready()
        except BaseException:
            owner.close(expect_success=False)
            raise
        return owner

    def stream(self, kind):
        owner = self.owned(kind)
        try:
            with socket.create_connection(('127.0.0.1', owner.port), timeout=10) as sock:
                # A single send contains all requests; no extra byte may be
                # needed to unstick buffered frames (the original OTP defect).
                sock.sendall(b''.join(frame(cid=n) for n in range(1,33)))
                delta = dict(jvm=1, ray=2, erlang=3)[kind]
                for n in range(1,33):
                    self.assertEqual(reply(sock), (0x11,n,b'bin:u:Pong',struct.pack('<Q',39+delta)))
                # Fragment a valid request at every byte boundary in the write
                # sequence, then require exactly one response.
                for byte in frame(cid=40):
                    sock.sendall(bytes([byte]))
                self.assertEqual(reply(sock)[1], 40)
                sock.sendall(frame(0x13, 0) + frame(0x13, 0, payload=b'bad')
                    + frame(cid=41,key=b'bin:u:TellCount',payload=b''))
                self.assertEqual(reply(sock), (0x11,41,b'bin:u:AddR',struct.pack('<Q',1)))
                # Probe succeeds only if a real reverse response is consumed;
                # wrong key/length must not turn an arbitrary payload into success.
                sock.sendall(frame(cid=42,key=b'bin:u:Probe',payload=b''))
                request = reply(sock)
                self.assertEqual((request[0],request[2],request[3]),(0x10,b'bin:u:Ping',struct.pack('<Q',100)))
                sock.sendall(frame(0x11, request[1], b'wrong', struct.pack('<Q',999)))
                failure = reply(sock)
                self.assertEqual((failure[0], failure[1]), (0x12,42))
                sock.sendall(frame(cid=43,key=b'bin:u:Add',payload=b'bad'))
                failure = reply(sock)
                self.assertEqual((failure[0], failure[1]), (0x12,43))
                maximum = (1 << 64) - 1
                sock.sendall(frame(cid=44,payload=struct.pack('<Q',maximum)))
                self.assertEqual(reply(sock), (0x11,44,b'bin:u:Pong',struct.pack('<Q',delta-1)))
                sock.sendall(frame(cid=45,key=b'bin:u:Add',payload=struct.pack('<QQ',maximum,maximum)))
                expected = dict(jvm=(1 << 64)-11,ray=998,erlang=9998)[kind]
                self.assertEqual(reply(sock), (0x11,45,b'bin:u:AddR',struct.pack('<Q',expected)))
                owner.sample()
                if kind == 'ray':
                    commands = []
                    for process in owner.observed.values():
                        with contextlib.suppress(psutil.NoSuchProcess): commands.extend(process.cmdline())
                    self.assertTrue(any('raylet' in word for word in commands), 'real local Ray runtime required')
                # Retire while the socket is still connected so normal EOF from
                # the manager, not a prior peer disconnect, is what stops it.
                owner.close()
        except BaseException:
            if owner.child.poll() is None: owner.close(expect_success=False)
            raise

    def invalid(self, kind, data):
        owner = self.owned(kind)
        try:
            with socket.create_connection(('127.0.0.1',owner.port),timeout=10) as sock:
                sock.sendall(data + frame(cid=77))
                try: received = sock.recv(4096)
                except ConnectionResetError: received = b''
                self.assertEqual(received,b'', 'malformed input must close, not reach the next ASK')
        finally:
            owner.close(expect_success=False)

    def killed(self, kind):
        sentinel = subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)'])
        owner = None
        try:
            owner = self.owned(kind)
            with socket.create_connection(('127.0.0.1',owner.port),timeout=10) as sock:
                sock.sendall(frame())
                self.assertEqual(reply(sock)[0], 0x11)
                owner.sample()
                owner.close(kill=True)
                self.assertIsNone(sentinel.poll(), 'unrelated process must survive')
        finally:
            if owner is not None and owner.child.poll() is None: owner.close(expect_success=False)
            sentinel.terminate(); sentinel.wait(timeout=5)

    def test_startup_owner_cancellation(self):
        # EOF before readiness cancels startup and must reclaim compiler groups.
        owner = Owner('jvm')
        wait_for(lambda: (owner.sample(), bool(owner.observed))[1])
        owner.close(expect_success=False)
        self.assertIsNone(owner.port)

    def test_failed_build_reclaims_owned_group(self):
        # Use the real manager with a deliberately failing private compiler.
        import tempfile
        with tempfile.TemporaryDirectory() as temp:
            compiler = Path(temp)/'javac'
            compiler.write_text('#!/bin/sh\nsleep 0.2\nexit 7\n')
            compiler.chmod(0o755)
            previous = os.environ['PATH']
            os.environ['PATH'] = temp + os.pathsep + previous
            try: owner = Owner('jvm')
            finally: os.environ['PATH'] = previous
            try:
                with self.assertRaises(AssertionError): owner.ready()
            finally: owner.close(expect_success=False)
            self.assertNotEqual(owner.child.returncode, 0)
            self.assertTrue(owner.observed)


for kind in ('jvm','erlang','ray'):
    setattr(Contracts, f'test_{kind}_stream_business', lambda self, k=kind: self.stream(k))
    setattr(Contracts, f'test_{kind}_manager_sigkill', lambda self, k=kind: self.killed(k))
    invalid_cases = {'short': struct.pack('<I',0), 'oversized': struct.pack('<I',0xffffffff)}
    header = bytearray(frame()); header[6] = 1
    invalid_cases['flags'] = bytes(header)
    path = bytearray(frame()); path[24:28] = struct.pack('<I',0xffffffff)
    invalid_cases['path_length'] = bytes(path)
    utf8 = bytearray(frame()); utf8[28] = 0xff
    invalid_cases['utf8'] = bytes(utf8)
    for label, data in invalid_cases.items():
        setattr(Contracts, f'test_{kind}_invalid_{label}', lambda self,k=kind,d=data: self.invalid(k,d))

if __name__ == '__main__':
    try:
        unittest.main()
    finally:
        if os.environ.get('POC_CONTRACT_RECEIPT'):
            Path(os.environ['POC_CONTRACT_RECEIPT']).write_text(json.dumps(RECEIPTS,indent=2)+'\n')
