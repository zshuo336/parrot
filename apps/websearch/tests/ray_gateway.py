"""Run an owned, isolated Ray gateway and close its local runtime on termination."""
import signal
import sys

import ray
from parrot_protocol.ray_gw import serve, ParrotDispatcher


def terminate(_signum, _frame):
    raise KeyboardInterrupt


signal.signal(signal.SIGTERM, terminate)
try:
    serve(
        int(sys.argv[1]),
        ParrotDispatcher(),
        ray_kwargs={
            "address": "local",
            "num_cpus": 2,
            "include_dashboard": False,
            "log_to_driver": False,
            "_temp_dir": sys.argv[2],
        },
        parrot_addr=sys.argv[3] if len(sys.argv) > 3 else None,
    )
except KeyboardInterrupt:
    pass
finally:
    ray.shutdown()
