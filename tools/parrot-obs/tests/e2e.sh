#!/usr/bin/env bash
# Compatibility entrypoint for the owned, dynamically bound gateway suite.
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
cd "$REPO"
exec "${PYTHON:-python3}" tools/parrot-obs/tests/run_gateways.py
