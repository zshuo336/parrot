#!/usr/bin/env python3
"""A private Ray runtime for one launcher, never an ambient shared cluster."""
import signal
import sys

import ray
from parrot_protocol.ray_gw import serve


def terminate(_signum, _frame):
    raise KeyboardInterrupt


signal.signal(signal.SIGTERM, terminate)
try:
    serve(0, ray_kwargs={'address': 'local', 'num_cpus': 2, 'include_dashboard': False,
                         'log_to_driver': False, '_temp_dir': sys.argv[1]},
          parrot_addr=sys.argv[2] or None)
except KeyboardInterrupt:
    pass
finally:
    ray.shutdown()
