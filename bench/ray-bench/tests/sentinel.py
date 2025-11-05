"""An independent live cluster that a benchmark launcher must leave untouched."""

from pathlib import Path
import json
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import os
import secrets

# Configure this separate cluster before importing Ray's cached configuration.
root = Path(sys.argv[1])
token = root / "auth_token"
fd = os.open(token, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as out:
    out.write(secrets.token_hex(32))
os.environ["RAY_AUTH_MODE"] = "token"
os.environ["RAY_AUTH_TOKEN_PATH"] = str(token)
os.environ.pop("RAY_AUTH_TOKEN", None)
import bench_runtime as rt

root = Path(sys.argv[1])
with rt.Session(root / "ray", 1, 80 * 1024 * 1024):
    with rt.Scope() as scope:
        actor = scope.create().actor
        sequence = 0
        while not (root / "stop").exists():
            value = rt.get(actor.echo.remote(sequence), seconds=10)
            rt.require(value == sequence, "sentinel returned corrupt value")
            partial = root / "heartbeat.new"
            partial.write_text(
                json.dumps(
                    {"sequence": sequence, "identity": rt.get(actor.identity.remote())}
                )
            )
            partial.replace(root / "heartbeat.json")
            sequence += 1
            time.sleep(0.1)
