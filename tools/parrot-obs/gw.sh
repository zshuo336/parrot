#!/usr/bin/env bash
# Owned Erlang/JVM/Ray gateway lifecycle; see tools/parrot-obs/README.md.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
exec "${PYTHON:-python3}" "$ROOT/tools/parrot-obs/gateway_manager.py" "$@"
