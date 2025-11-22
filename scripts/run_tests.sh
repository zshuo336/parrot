#!/bin/bash

# Run all tests in the workspace but exclude doctests
# This script runs all the tests for all projects in the workspace

set -euo pipefail

# Keep the all-workspace entrypoint usable on developer machines and CI
# runners with bounded disk. Callers may override these defaults explicitly.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
export CARGO_PROFILE_TEST_DEBUG="${CARGO_PROFILE_TEST_DEBUG:-0}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-never}"

# The workspace contains an integration target that loads the generated
# crawler Dylib/Wasm artifacts. Prepare a current-source receipt before Cargo
# starts tests; set PARROT_PREPARE_CRAWLER=0 for intentionally core-only runs.
if [[ "${PARROT_PREPARE_CRAWLER:-1}" != "0" ]]; then
    make --no-print-directory build-crawler-business
fi

echo "Running all tests (excluding doctests)..."

# Change to workspace root directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$WORKSPACE_ROOT"

# Run tests for all workspace members with doctests disabled
cargo test --locked --workspace --all-targets --lib --bins --tests --benches

echo "All tests completed successfully!"
