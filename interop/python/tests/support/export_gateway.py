"""Own one real serve() gateway and its Ray cluster for process-level testing."""
import json
from pathlib import Path
import sys
import ray

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ray_test_cluster import owned_ray_cluster
from parrot_protocol.ray_gw import ParrotDispatcher, serve
from parrot_protocol.trace import capture_parent
from parrot_protocol.trace_recording import TraceConfig, TraceExportConfig
from parrot_protocol.ray_startup import graceful_sigterm


print('PARROT_TEST_RUNTIME=' + json.dumps(dict(executable=sys.executable,
    version=list(sys.version_info[:3]), optimize=sys.flags.optimize)), flush=True)
root, endpoint = Path(sys.argv[1]), sys.argv[2]
dispatcher = ParrotDispatcher()
@dispatcher.handler('bin:context')
def echo(key, payload):
    return key, capture_parent() + payload

with graceful_sigterm(), owned_ray_cluster(ray, evidence=root/'cluster'):
    try:
        serve(0, dispatcher, trace_config=TraceConfig(export=TraceExportConfig(endpoint)))
    except KeyboardInterrupt:
        pass
(root/'finished.txt').write_text('retired\n')
