"""A separately owned Ray cluster controlled only through the parent's socket."""
import os
from pathlib import Path
import socket
import sys

import ray

from parrot_protocol.ray_job import send, receive
from parrot_protocol.ray_gw import _kill_and_confirm
from ray_test_cluster import owned_ray_cluster


def main():
    with socket.socket(fileno=int(sys.argv[1])) as control:
        with owned_ray_cluster(ray, evidence=Path(sys.argv[2])):
            @ray.remote
            class Sentinel:
                def __init__(self):
                    self.counter = 0
                def ping(self):
                    self.counter += 1
                    ctx = ray.get_runtime_context()
                    return dict(pid=os.getpid(), counter=self.counter,
                                job=ctx.get_job_id(), actor=ctx.get_actor_id())
            actor = Sentinel.remote()
            try:
                ready = ray.get(actor.ping.remote(), timeout=20)
                ready.update(ready=True, address=ray._private.worker.global_worker.node.address_info['gcs_address'])
                send(control, ready)
                while True:
                    command = receive(control)
                    if command is None or command == {'command': 'exit'}:
                        break
                    if command != {'command': 'ping'}:
                        raise ValueError('invalid sentinel command')
                    send(control, ray.get(actor.ping.remote(), timeout=10))
            finally:
                _kill_and_confirm(ray, actor, actor.__ray_ready__)
        try:
            send(control, dict(retired=True))
        except BrokenPipeError:
            pass  # Parent closed ownership; cluster cleanup still ran.


if __name__ == '__main__':
    main()
