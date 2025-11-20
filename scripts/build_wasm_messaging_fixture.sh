#!/usr/bin/env bash
set -euo pipefail
root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$root/crates/parrot-wasm/tests/messaging-fixture"
command -v cargo >/dev/null
command -v wasm-tools >/dev/null
cargo build --locked --manifest-path "$fixture/Cargo.toml" \
  --target-dir "${CARGO_TARGET_DIR:-$fixture/target}" --target wasm32-unknown-unknown --release
output="$root/crates/parrot-wasm/tests/fixtures/messaging-component.wasm"
temporary="$(mktemp "${output}.XXXXXX")"
trap 'rm -f -- "$temporary"' EXIT
wasm-tools component new \
  "${CARGO_TARGET_DIR:-$fixture/target}/wasm32-unknown-unknown/release/parrot_wasm_messaging_fixture.wasm" \
  -o "$temporary"
wasm-tools validate "$temporary"
chmod 644 "$temporary"
mv -- "$temporary" "$output"
