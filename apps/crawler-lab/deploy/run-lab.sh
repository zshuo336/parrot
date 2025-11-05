#!/usr/bin/env bash
# An isolated local demonstration. The Python owner reaps only this run's groups.
set -euo pipefail
exec "${PYTHON:-python3}" -u "$(cd "$(dirname "$0")" && pwd)/launcher.py" "$@"
