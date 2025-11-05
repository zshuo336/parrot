#!/usr/bin/env python3
"""Trusted inline entrypoint: one private local Ray cluster per owned session.

Execute through gateway_worker with python_entrypoint=True so Ray's setpgrp
cannot move the runtime out of its guardian's session leader process group.
"""
import sys
import tempfile

import ray
from parrot_protocol.ray_gw import main

# Keep the path short for macOS AF_UNIX names, independent of GW_RUN_DIR length.
# Retain runtime logs after shutdown; do not reuse /tmp/ray or ambient clusters.
directory = tempfile.mkdtemp(prefix='pg-ray-', dir='/tmp')
print('PARROT_RAY_DIRECTORY=' + directory, flush=True)
try:
    main(sys.argv, ray_kwargs={
        'address': 'local', 'num_cpus': 2, 'include_dashboard': False,
        'log_to_driver': False, '_temp_dir': directory,
    })
finally:
    ray.shutdown()
