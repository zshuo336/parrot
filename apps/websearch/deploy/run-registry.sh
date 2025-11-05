#!/usr/bin/env bash
# Local reverse registration with owned gateways and isolated logs.
set -euo pipefail
exec "${PYTHON:-python3}" -u "$(cd "$(dirname "$0")" && pwd)/launcher.py" registry "$@"
