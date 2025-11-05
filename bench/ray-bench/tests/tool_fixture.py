"""Launcher-only tools. No actor or performance claim uses this fixture."""

import json
import os
from pathlib import Path
import subprocess
import sys

name = Path(sys.argv[0]).name
stage = (
    ("keys" if "keys" in sys.argv else "runtime") if name == "ray_bench.py" else name
)
with open(os.environ["BENCH_EVENTS"], "a") as out:
    out.write(
        json.dumps(
            dict(
                stage=stage,
                args=sys.argv[1:],
                cwd=os.getcwd(),
                auth_mode=os.environ.get("RAY_AUTH_MODE"),
                auth_path=os.environ.get("RAY_AUTH_TOKEN_PATH"),
                auth_token_present="RAY_AUTH_TOKEN" in os.environ,
                auth_permissions=Path(os.environ["RAY_AUTH_TOKEN_PATH"]).stat().st_mode
                & 0o777,
            )
        )
        + "\n"
    )
if os.environ.get("BENCH_FAIL") == stage:
    print("control failure", flush=True)
    sys.exit(17)
if os.environ.get("BENCH_BLOCK") == stage or os.environ.get("BENCH_ORPHAN") == stage:
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(300)"])
    Path(os.environ["BENCH_PIDS"]).write_text(json.dumps([os.getpid(), child.pid]))
    if os.environ.get("BENCH_ORPHAN") == stage:
        sys.exit(0)
    child.wait()
if name == "rustc":
    target = Path(sys.argv[sys.argv.index("-o") + 1])
    target.write_text(
        "#!" + sys.executable + "\n" + Path(os.environ["BENCH_FIXTURE"]).read_text()
    )
    target.chmod(0o755)
elif name == "oracle":
    Path(sys.argv[2]).write_text("1 1 7806831264735756412\n")
elif stage == "keys":
    Path(sys.argv[-1]).write_text("1 1\n")
