#!/usr/bin/env bash
# Miri checks the actual production envelope source and shared unwind contracts.
# Usage: ./scripts/miri.sh [test_filter]
set -euo pipefail
cd "$(dirname "$0")/.."
if (( $# > 1 )); then
    echo "Usage: $0 [test_filter]" >&2
    exit 2
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
if ! rustup toolchain list | grep -q nightly; then
    echo "需要 nightly 工具链与 miri/rust-src 组件" >&2
    exit 1
fi
export MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-strict-provenance"
miri_command=(cargo +nightly miri test --locked --manifest-path poc/envelope-safety/Cargo.toml --all-targets)
listing="$("${miri_command[@]}" -- --list "$@")"
if ! printf '%s\n' "$listing" | grep -q ': test$'; then
    echo "ERROR: Miri filter selected no tests" >&2
    exit 1
fi
"${miri_command[@]}" -- "$@"
