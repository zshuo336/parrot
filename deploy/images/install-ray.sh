#!/bin/sh
# Install in the caller's CPython 3.11 environment; use an isolated venv locally.
set -eu
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PYTHON=${PYTHON:-python3}
"$PYTHON" -c 'import sys; sys.exit(0 if sys.implementation.name == "cpython" and sys.version_info[:2] == (3, 11) else "Ray image locks require CPython 3.11")'
# jieba is an sdist. Lock its build tools first and disable isolated build-time
# resolution, otherwise --require-hashes would still permit unpinned tools.
"$PYTHON" -m pip install --no-cache-dir --require-hashes --only-binary=:all: \
  -r "$HERE/requirements-ray-build.lock"
"$PYTHON" -m pip install --no-cache-dir --require-hashes --no-build-isolation \
  --only-binary=:all: --no-binary=jieba -r "$HERE/requirements-ray.lock"
"$PYTHON" -m pip check
