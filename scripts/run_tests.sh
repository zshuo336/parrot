#!/bin/bash

# Run all tests in the workspace but exclude doctests
# This script runs all the tests for all projects in the workspace

set -e # Exit on error

echo "Running all tests (excluding doctests)..."

# Change to workspace root directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$WORKSPACE_ROOT"

# Run tests for all workspace members with doctests disabled
cargo test --workspace --all-targets --lib --bins --tests --benches

echo "All tests completed successfully!" 