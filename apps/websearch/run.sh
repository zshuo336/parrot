#!/usr/bin/env bash
# Local direct mode with owned gateways and isolated logs.
set -euo pipefail
exec "${PYTHON:-python3}" -u "$(cd "$(dirname "$0")" && pwd)/deploy/launcher.py" direct "$@"
