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

echo "Running all tests (excluding doctests)..."

# Change to workspace root directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$WORKSPACE_ROOT"

# Run tests for all workspace members with doctests disabled
cargo test --locked --workspace --all-targets --lib --bins --tests --benches

echo "All tests completed successfully!"
