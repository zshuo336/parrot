#!/bin/sh
set -eu
exec "${PYTHON:-python3}" "$(dirname "$0")/run_bench.py" "$@"
