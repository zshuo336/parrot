"""Execute the real module CLI inside a test-owned local Ray cluster."""
from pathlib import Path
import runpy
import sys
import ray

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from ray_test_cluster import owned_ray_cluster
from parrot_protocol.ray_startup import graceful_sigterm

root=Path(sys.argv[1])
sys.argv=['parrot_protocol.ray_gw']+sys.argv[2:]
with graceful_sigterm(),owned_ray_cluster(ray,evidence=root/'cluster'):
    runpy.run_module('parrot_protocol.ray_gw',run_name='__main__')
(root/'finished.txt').write_text('retired\n')
