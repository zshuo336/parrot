#!/usr/bin/env bash
# cargo run honors CARGO_TARGET_DIR and runs only the newly successful build.
set -euo pipefail
cd -- "$(dirname -- "$0")"
exec cargo run --locked --release --bin actix-bench -- "$@"
