"""A deployment-owned Ray driver job on the caller's existing local node.

The private socket is the driver's lifetime owner. EOF retires its job even when
its coordinator process dies. User code runs only after the coordinator has
recorded this owner and obtained the named host's handle.
"""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import time
import uuid

from .job_output import BoundedOutput

MAX_CONTROL_BYTES = 1024 * 1024


def encode(value):
    data = json.dumps(value, ensure_ascii=False).encode('utf-8')
    if len(data) > MAX_CONTROL_BYTES:
        raise ValueError('deployment job control message exceeds limit')
    return struct.pack('!I', len(data)) + data


def send(connection, value):
    connection.sendall(encode(value))


def _read(connection, length, allow_eof=False):
    data = bytearray()
    while len(data) < length:
        chunk = connection.recv(length-len(data))
        if not chunk:
            if allow_eof and not data:
                return None
            raise EOFError('incomplete deployment job control message')
        data.extend(chunk)
    return bytes(data)


def receive(connection):
    header = _read(connection, 4, allow_eof=True)
    if header is None:
        return None
    length, = struct.unpack('!I', header)
    if not 0 < length <= MAX_CONTROL_BYTES:
        raise ValueError('invalid deployment job control length')
    return json.loads(_read(connection, length))


class DeploymentJob:
    def __init__(self, ray_module, context, root, runtime_env):
        self.ray, self.context = ray_module, context
        self.root, self.runtime_env = root, runtime_env
        self.namespace = 'parrot-deployment-' + uuid.uuid4().hex
        self.process = self.connection = self.log = None
        self.close_sent = False
        self.job_id = None

    def start(self):
        if self.process is not None:
            raise RuntimeError('deployment job already started')
        config = dict(address=self.ray._private.worker.global_worker.node.address_info['gcs_address'],
                      namespace=self.namespace, node=self.context.get_node_id(),
                      root=self.root, runtime_env=self.runtime_env)
        # Validate the bounded envelope before allocating any process resources.
        if len(json.dumps(config, ensure_ascii=False).encode('utf-8')) > MAX_CONTROL_BYTES:
            raise ValueError('deployment job configuration exceeds limit')
        connection, child = socket.socketpair()
        self.connection = connection
        try:
            timeout = self.runtime_env.get('config', {}).get('setup_timeout_seconds', 600) + 30
            connection.settimeout(timeout)
            self.log = BoundedOutput()
            protocol = str(Path(__file__).resolve().parent.parent)
            env = dict(os.environ)
            env['PYTHONPATH'] = os.pathsep.join(filter(None, [protocol, env.get('PYTHONPATH', '')]))
            self.process = subprocess.Popen(
                [sys.executable, '-m', 'parrot_protocol.ray_job', str(child.fileno())],
                pass_fds=(child.fileno(),), env=env, stdin=subprocess.DEVNULL,
                stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True,
            )
        finally:
            child.close()
            if self.log is not None:
                self.log.close_writer()
        send(connection, config)
        receipt = receive(connection)
        if receipt is None:
            detail = self.log.tail()
            raise RuntimeError('deployment job exited before startup receipt: ' + detail)
        if (not isinstance(receipt, dict) or set(receipt) != {'job', 'namespace'} or
                receipt['namespace'] != self.namespace or not isinstance(receipt['job'], str) or
                len(receipt['job']) != 8 or any(c not in '0123456789abcdef' for c in receipt['job']) or
                receipt['job'] == self.context.get_job_id()):
            raise RuntimeError('invalid deployment job startup receipt')
        self.job_id = receipt['job']
        return self.ray.get_actor('host', namespace=self.namespace)

    def close(self, deadline=None):
        end = time.monotonic() + 5 if deadline is None else deadline
        if self.connection is not None and not self.close_sent:
            # EOF is sufficient and cannot block on a full send buffer. Closing
            # our only endpoint is also the coordinator-crash cleanup protocol.
            self.close_sent = True
            self.connection.close()
            self.connection = None
        if self.process is not None and self.process.poll() is None:
            remaining = end-time.monotonic()
            if remaining <= 0:
                raise TimeoutError('deployment job retirement deadline elapsed')
            try:
                self.process.wait(timeout=remaining)
            except subprocess.TimeoutExpired as error:
                raise TimeoutError('deployment job driver has not exited') from error
        # Process death is not a claim that user work succeeded. Snapshot.close
        # must still acquire exclusivity against every remaining worker lease.
        if self.log is not None:
            self.log.close(deadline=end)
            self.log = None


def run_driver(descriptor):
    import ray
    from ray.util.scheduling_strategies import NodeAffinitySchedulingStrategy
    from .ray_hosts import _DeploymentHost
    from .ray_snapshot_lease import hold_snapshot_lease
    connection = socket.socket(fileno=descriptor)
    os.set_inheritable(descriptor, False)
    try:
        config = receive(connection)
        if not isinstance(config, dict) or set(config) != {'address', 'namespace', 'node', 'root', 'runtime_env'}:
            raise ValueError('invalid deployment job configuration')
        # Protect source while connecting/starting workers, including after a
        # coordinator crash. This lease ends only when this driver exits.
        hold_snapshot_lease(config['runtime_env']['env_vars']['PARROT_SNAPSHOT_CONTROL'])
        # Explicit address: this driver must never start or own a new cluster.
        ray.init(address=config['address'], namespace=config['namespace'], log_to_driver=False)
        job = ray.get_runtime_context().get_job_id()
        runtime_env = config['runtime_env']
        runtime_env.setdefault('env_vars', {})['PARROT_DEPLOYMENT_JOB'] = job
        host = ray.remote(_DeploymentHost).options(
            num_cpus=0, name='host', runtime_env=runtime_env,
            scheduling_strategy=NodeAffinitySchedulingStrategy(config['node'], soft=False),
        ).remote(config['root'])
        # Keep the owning handle until EOF. Readiness/user factory execution
        # belongs to the coordinator's ordinary, retryable mounting protocol.
        send(connection, dict(job=job, namespace=config['namespace']))
        if receive(connection) is not None:
            raise ValueError('unexpected deployment job command')
    finally:
        ray.shutdown()
        connection.close()


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[2] == '--driver':
        run_driver(int(sys.argv[1]))
    else:
        from .ray_job_guard import supervise
        supervise(int(sys.argv[1]))
