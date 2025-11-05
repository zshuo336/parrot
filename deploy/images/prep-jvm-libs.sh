#!/usr/bin/env bash
# Prepare every current cp.txt dependency; Python 3.9+ is required.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
exec "${PYTHON:-python3}" "$ROOT/deploy/images/prepare_jvm_libs.py"
