"""Real exec/parent-death contracts for the private Linux node bootstrap."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time

import pytest
import ray
from parrot_protocol import ray_owned_worker
from parrot_protocol.ray_startup import guarded_ports


pytestmark = pytest.mark.skipif(sys.platform != 'linux', reason='Linux kernel parent-death contract')


def wait_until(predicate, timeout, detail):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError(detail)
        time.sleep(.01)


def can_bind(port):
    with socket.socket() as listener:
        try:
            listener.bind(('127.0.0.1', port))
            return True
        except OSError:
            return False


@pytest.mark.parametrize('runtime_env', [{}, {'env_vars': {'PARROT_WORKER_LIFETIME_TEST': 'actual-exec'}}])
def test_separate_group_worker_retires_after_parent_sigkill_across_actual_ray_exec(tmp_path, runtime_env):
    worker = tmp_path / 'worker.py'
    record = tmp_path / 'record.json'
    worker.write_text(
        'import json,os,socket,sys,time\n'
        'from pathlib import Path\n'
        'listener=socket.socket();listener.bind(("127.0.0.1",0));listener.listen()\n'
        'record=Path(sys.argv[1]);temporary=record.with_suffix(".tmp")\n'
        'temporary.write_text(json.dumps(dict(pid=os.getpid(),parent=os.getppid(),'
        'group=os.getpgrp(),port=listener.getsockname()[1],'
        'value=os.environ.get("PARROT_WORKER_LIFETIME_TEST"))))\n'
        'temporary.replace(record)\n'
        # A broken baseline exits on its own so failed assertions leak no worker.
        'time.sleep(8)\n'
    )
    launcher = (
        'import os,subprocess,sys\n'
        'child=subprocess.Popen(sys.argv[1:],preexec_fn=os.setpgrp)\n'
        'try: sys.stdin.read(1)\n'
        'finally: child.kill();child.wait()\n'
    )
    parent = subprocess.Popen(
        [sys.executable, '-c', launcher, sys.executable, ray_owned_worker.__file__,
         '--language', 'PYTHON', '--serialized-runtime-env-context', json.dumps(runtime_env),
         str(worker), str(record)], stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    receipt = None
    try:
        wait_until(lambda: record.exists() or parent.poll() is not None, 10, 'worker bootstrap did not finish')
        assert record.exists(), parent.stderr.read().decode(errors='replace')
        receipt = json.loads(record.read_text())
        assert receipt['parent'] == parent.pid
        assert receipt['group'] == receipt['pid'] != os.getpgrp()
        assert receipt['value'] == runtime_env.get('env_vars', {}).get('PARROT_WORKER_LIFETIME_TEST')
        assert not can_bind(receipt['port']), 'worker must actually hold a listening socket'
        parent.kill()
        parent.wait(timeout=5)
        wait_until(lambda: can_bind(receipt['port']), 3,
                   'worker survived parent SIGKILL after the actual Ray runtime-env exec')
    finally:
        if parent.poll() is None:
            parent.stdin.close()
        parent.wait(timeout=12)
        if receipt is not None:
            wait_until(lambda: can_bind(receipt['port']), 10, 'fixture worker did not self-retire')
        parent.stdin.close()
        parent.stderr.close()


@pytest.mark.parametrize('fail_start', [False, True])
def test_private_node_selects_bootstrap_before_start_and_restores_parameters(fail_start):
    from ray._private.parameter import RayParams
    standard = str(Path(ray.__file__).parent / '_private/workers/setup_worker.py')

    class Node:
        def __init__(self):
            self._ray_params = RayParams(setup_worker_path=standard,
                                         object_manager_port=32001, node_manager_port=32002)

        def _get_unused_port(self, allocated_ports=None):
            raise AssertionError('explicit ports must be preserved')

        def start_raylet(self):
            assert self._ray_params.setup_worker_path == ray_owned_worker.__file__
            if fail_start:
                raise ValueError('actual start failed')
            return 'started'

    node = Node()
    original = Node.start_raylet
    if fail_start:
        with pytest.raises(ValueError, match='actual start failed'), guarded_ports(Node, owned_workers=True):
            node.start_raylet()
    else:
        with guarded_ports(Node, owned_workers=True):
            assert node.start_raylet() == 'started'
    assert node._ray_params.setup_worker_path == standard
    assert Node.start_raylet is original


def test_embedding_node_keeps_custom_bootstrap_unmodified():
    from ray._private.parameter import RayParams

    class Node:
        def __init__(self):
            self._ray_params = RayParams(setup_worker_path='/custom/bootstrap.py',
                                         object_manager_port=32001, node_manager_port=32002)

        def _get_unused_port(self, allocated_ports=None):
            raise AssertionError('explicit ports must be preserved')

        def start_raylet(self):
            return self._ray_params.setup_worker_path

    with guarded_ports(Node):
        assert Node().start_raylet() == '/custom/bootstrap.py'
    with guarded_ports(Node, owned_workers=True), pytest.raises(RuntimeError, match='standard runtime'):
        Node().start_raylet()
