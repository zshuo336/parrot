#!/usr/bin/env bash
# Portable process-group timeout for Linux and macOS. Expired deadlines return
# 124; ordinary child exits and signals retain their shell exit status.
set -eu
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec python3 "$SCRIPT_DIR/with_timeout.py" "$@"
