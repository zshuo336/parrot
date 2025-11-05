# Wasm runtime fixture

`fixture-component.wasm` implements the frozen `wit/parrot-actor.wit` world.
The source is in `../fixture/src/lib.rs`, with an independent, versioned
`../fixture/Cargo.lock`. Operations include echo/up/self/config/clock/log/err/spin,
state/counter/started/drained, tell bump, and memory-pages/grow. `grow` accepts a
little-endian u32 page count and returns the previous page count as a u64, or
`u32::MAX` when Wasm rejects the request. One page is 64 KiB.

From the repository root, rebuild and validate the component before atomically
replacing the checked-in fixture:

```sh
bash scripts/build_wasm_fixture.sh
```

Requires the `wasm32-unknown-unknown` Rust target and `wasm-tools`. Validated here
with rustc 1.98.1 and wasm-tools 1.261.0. The script uses `cargo build --locked`
and an explicit fixture target directory, independent of CARGO_TARGET_DIR.
The fixture is executable test data; tests must allow normal guest operations
enough fuel and keep quota-exhaustion assertions separate from business replies.
