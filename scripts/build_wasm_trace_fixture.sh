#!/usr/bin/env bash
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$root/crates/parrot-wasm/tests/trace-fixture"
command -v cargo >/dev/null
command -v wasm-tools >/dev/null

cargo build --locked --manifest-path "$fixture/Cargo.toml" \
  --target-dir "$fixture/target" --target wasm32-unknown-unknown --release
output="$root/crates/parrot-wasm/tests/fixtures/trace-component.wasm"
temporary="$(mktemp "${output}.XXXXXX")"
trap 'rm -f -- "$temporary"' EXIT
wasm-tools component new \
  "$fixture/target/wasm32-unknown-unknown/release/parrot_wasm_trace_fixture.wasm" \
  -o "$temporary"
wasm-tools validate "$temporary"
chmod 644 "$temporary"
mv -- "$temporary" "$output"
