#!/usr/bin/env bash
# Build a complete release and install it into a new root on each SSH host.
set -euo pipefail
exec "${PYTHON:-python3}" "$(cd "$(dirname "$0")" && pwd)/distribute.py" "$@"
