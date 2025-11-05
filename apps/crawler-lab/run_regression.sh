#!/usr/bin/env bash
# Private process ownership and logs are shared with the production launcher.
set -euo pipefail
exec "${PYTHON:-python3}" -u "$(cd "$(dirname "$0")" && pwd)/deploy/regression.py" "$@"
