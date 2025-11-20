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

`trace-component.wasm` separately exercises optional identity propagation;
rebuild it with `bash scripts/build_wasm_trace_fixture.sh`.
`recording-component.wasm` exercises the additional optional recording world,
including guest span nesting, stale handles, bounded admission, explicit
startup/drain recording, and abnormal cleanup. Lifecycle identities are returned
by `start-identity`/`drain-identity`; test-only `recording-on-start-mode` and
`recording-on-drain-mode` configure unfinished/trap/fuel/epoch/host-panic/budget
paths when a recording capability is supplied. Plain lifecycle calls remain
valid without recording. Its independent sources/lockfile are in `../recording-fixture`;
rebuild with `bash scripts/build_wasm_recording_fixture.sh`. These additions
do not require replacing either earlier fixture.

`messaging-component.wasm` implements the optional messaging world, including
real ask/tell imports, lifecycle calls, bounded request admission and cancellation
probes. Its independent sources/lockfile are in `../messaging-fixture`; rebuild
with `bash scripts/build_wasm_messaging_fixture.sh`. This script honors
`CARGO_TARGET_DIR`. The runtime and application tests separately verify Store
retirement and actual native actor dispatch. See `docs/WASM_MESSAGING.md` for
the host permission and delivery contract.

`services-component.wasm` adds the optional services world. Its independent
source and lockfile are in `../services-fixture`; rebuild with
`bash scripts/build_wasm_services_fixture.sh` (honors `CARGO_TARGET_DIR`). Commands
cover register/deregister, subscribe/next/poll/close, invalid requests, shared
messaging/discovery quotas, trap and real discovery followed by guest ASK.
`next-then-write` attempts a registration and TELL after `next`, even for a WIT
error or EOF. `next-on-start` and `next-on-drain` run that sequence in lifecycle
hooks; startup uses the handle from `subscribe-on-start`. These probes distinguish
an immediate host cleanup trap from an error noticed only at invocation exit.
The `key` business setting defaults to `edge/guest-service`. Startup registration,
subscription and trap probes are explicit fixture settings, never authority.
Runtime tests inspect actual receiver retirement and handle identities. App and
Node tests inspect real registries and business replies, including standard
processes with TCP/QUIC and operator-owned policies. See `docs/WASM_SERVICES.md`.
