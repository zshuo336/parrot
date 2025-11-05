"""Only launcher contracts use these tools; never a business/performance gate."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
name = Path(sys.argv[0]).name
stage = ('contracts' if 'BenchContracts' in sys.argv else 'keys' if 'keys' in sys.argv else 'runtime') if name == 'java' else name
with open(os.environ['BENCH_EVENTS'], 'a') as out:
    out.write(json.dumps(dict(stage=stage, args=sys.argv[1:])) + '\n')
if os.environ.get('BENCH_FAIL') == stage:
    sys.exit(17)
if os.environ.get('BENCH_BLOCK') == stage:
    child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(300)'])
    Path(os.environ['BENCH_PIDS']).write_text(json.dumps([os.getpid(), child.pid]))
    child.wait()
if name == 'rustc':
    output = Path(sys.argv[sys.argv.index('-o') + 1])
    output.write_text('#!' + sys.executable + '\n' + Path(os.environ['BENCH_FIXTURE']).read_text())
    output.chmod(0o755)
elif name == 'oracle':
    Path(sys.argv[2]).write_text('1 1 7806831264735756412\n')
elif stage == 'keys':
    Path(sys.argv[-1]).write_text('1 1\n')
