#!/usr/bin/env bash
# Deterministic host integration suite. See tests/README.md for coverage.
set -euo pipefail
exec python3 "$(dirname "$0")/run_integration.py" "$@"
