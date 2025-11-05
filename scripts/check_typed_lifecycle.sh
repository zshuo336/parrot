#!/bin/sh
# Run from any directory. Keep the historical debug/legacy defaults.
# Requires Node + npm ci in interop/typescript-lite, Python 3 and a C++ compiler.
set -eu
cd "$(dirname "$0")/.."
if [ "$#" -gt 2 ]; then
  echo 'Usage: check_typed_lifecycle.sh [debug|release] [default|legacy]' >&2
  exit 2
fi
profile=${1-debug}
envelope=${2-legacy}
case "$profile" in debug|release) ;; *) echo "Invalid profile: $profile" >&2; exit 2 ;; esac
case "$envelope" in default|legacy) ;; *) echo "Invalid envelope: $envelope" >&2; exit 2 ;; esac
set --
if [ "$profile" = release ]; then set -- "$@" --release; fi
if [ "$envelope" = legacy ]; then set -- "$@" --features parrot_envelope_legacy; fi

# Select the whole package, including library, integration and documentation
# tests. A new contract must enter this gate without editing a target allowlist.
# Ignored performance/scale tests keep their separate, explicit resource gates.
cargo test --locked -p parrot --no-fail-fast "$@"
cargo clippy --locked -p parrot --all-targets "$@" -- -D warnings
