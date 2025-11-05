#!/usr/bin/env python3
"""Private gateway build/runtime; stdin EOF retires only its owned groups."""
import os
from pathlib import Path
import select
import signal
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'tools/parrot-obs'))
from process_owner import OwnedChild


def run(kind, relay=None):
    children = []
    interrupted = False

    def interrupt(*_):
        nonlocal interrupted
        interrupted = True

    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, interrupt)

    def parent_closed():
        if interrupted:
            return True
        if select.select([sys.stdin], [], [], 0)[0]:
            os.read(sys.stdin.fileno(), 1024)
            return True
        return False

    with tempfile.TemporaryDirectory(prefix='poc-gw-', dir='/tmp') as tmp:
        directory = Path(tmp)

        def spawn(name, command, **options):
            child = OwnedChild(name, command, directory / (name + '.log'), **options)
            children.append(child)
            return child

        def wait(child, ready=False):
            end = time.monotonic() + 90
            while time.monotonic() < end:
                if parent_closed():
                    raise RuntimeError('gateway owner closed during startup')
                child.poll()
                if child.failure or child.code not in (None, 0):
                    raise RuntimeError(f'{child.state()}\n{child.text()[-16384:]}')
                if ready and child.port is not None and child.code is None:
                    return child.port
                if child.code is not None:
                    if ready:
                        raise RuntimeError('gateway exited before readiness')
                    return
                time.sleep(.01)
            raise TimeoutError(f'{child.name}: startup deadline\n{child.text()[-16384:]}')

        try:
            base = ROOT / 'poc/remote-poc'
            if kind == 'jvm':
                wait(spawn('build', ['javac', '-d', tmp, base / 'akka-gw/AkkaGw.java']))
                command = ['java', '-cp', tmp, 'AkkaGw', '0']
                if relay is not None:
                    command.append(relay)
                gateway = spawn('jvm', command, marker='POC_PORT ')
            elif kind == 'erlang':
                wait(spawn('build', ['erlc', '-Werror', '-o', tmp, base / 'erlang-gw/erlang_gw.erl']))
                gateway = spawn('erlang', ['erl', '-noshell', '-noinput', '-pa', tmp,
                                        '-eval', 'erlang_gw:main([0]).'], marker='POC_PORT ')
            elif kind == 'ray':
                gateway = spawn('ray', [sys.executable, base / 'ray-adapter/ray_gw.py', '0',
                                       directory / 'ray'], marker='POC_PORT ', python_entrypoint=True)
            else:
                raise ValueError('unknown gateway kind')
            port = wait(gateway, ready=True)
            # This reports the actual bound socket, not business readiness. The
            # Rust tests must complete a real protocol round trip on that socket.
            print(f'PORT {port}', flush=True)
            while not parent_closed():
                gateway.poll()
                if gateway.failure or gateway.code is not None:
                    raise RuntimeError(f'{gateway.state()}\n{gateway.text()[-16384:]}')
                time.sleep(.02)
        finally:
            for child in children:
                child.begin_stop()
            time.sleep(2.2)
            errors = []
            for child in reversed(children):
                try:
                    child.finish_stop()
                except Exception as error:
                    errors.append(str(error))
            if errors:
                raise RuntimeError('; '.join(errors))


if __name__ == '__main__':
    run(*sys.argv[1:])
