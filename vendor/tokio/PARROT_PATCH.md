# Parrot runtime progress patch

This directory is the Tokio 1.53.3 crates.io release under its original MIT
license. `UPSTREAM.json` records the release archive SHA-256 and all 564 upstream
file hashes. Eleven upstream Rust files and the normalized `Cargo.toml` differ;
the package version and dependency versions are unchanged. `Cargo.toml.orig`
is preserved byte-for-byte as upstream provenance, not an editable build input.

On Linux GNU, a native `dlclose` finalizer holds glibc's loader lock. Registering a
Rust TLS destructor can wait for that lock. A cold parking_lot parker can perform
this registration while Tokio holds a shared runtime mutex, preventing other
workers and timers from making progress. Captured original Parrot failure stacks
and independent native probes are retained in
`docs/review/independent-2026-10-08/native-stop-diagnostics-457/`.

The patch initializes parking_lot thread data using a private mutex and a
zero-duration condition-variable wait, once per thread, before taking shared
runtime locks or polling user futures:

- `src/loom/std/parking_lot.rs`: private initialization helper with a no-drop TLS
  flag; the wait requires no other thread to release anything.
- `src/loom/std/mod.rs`: conditional helper export.
- `src/runtime/blocking/pool.rs`: initialize before the thread-start callback and
  shared pool lock, including async workers hosted by this pool.
- `src/runtime/context/runtime.rs`: initialize runtime entry callers before user
  code runs, covering a cold `block_on` caller separately from workers.
- `src/runtime/park.rs`: initialize `CachedParkThread` before publishing its
  waker or acquiring its mutex. Synchronous channel waits and runtime shutdown
  bypass `enter_runtime`; they must not register their first parking_lot TLS
  destructor while holding a mutex needed by a runtime worker's wake operation.

The first four-file repair passed worker/block_on contracts but still failed a
ready-then-pending oneshot `blocking_recv` probe. The fifth-file extension covers
that synchronous entry and is tracked separately in
`docs/review/independent-2026-10-08/native-sync-progress-458/`. Initializing only
after acquiring the parker mutex is too late; the regression suite checks this
mutation as well as the original missing initialization.

The change applies only with `parking_lot` on Linux GNU, outside Loom and Miri.
It does not make arbitrary native code safe, bound loader operations, or provide
process isolation. A newly entering thread can still wait for the loader lock;
the patch avoids holding the affected shared runtime locks during this wait.
Existing native ownership, stop deadlines and finalizer gates are unchanged.

## Deferred bounded-channel notification

The three `src/sync/mpsc/{bounded,chan,mod}.rs` files additionally expose
`Sender::try_send_deferred` and its `DeferredSendWakes` guard. Admission, queue
insertion, capacity accounting and FIFO order match `try_send`. The guard owns
this thread's receiver notification, using the existing atomic waker handoff;
drop it after releasing application locks. A concurrent receiver registering its
waker can independently observe the pending notification. Retaining or forgetting
the guard delays or loses that reserved notification, so callers must not retain
it across waits that depend on the receiver. The ordinary send API is unchanged.

Receptionist owners use this operation to commit every subscriber's queue update
before invoking any receiver callback. They also defer retired sender destruction,
which can wake a receiver. Table, membership and connection locks are released
before delivery. Callback panic isolation belongs to the receptionist owner;
the primitive normally propagates a wake panic, and suppresses a secondary panic
only when its guard is dropped during an existing unwind.

## Platform documentation

Three additional changes are documentation-only: `src/net/mod.rs` links to
Windows named-pipe documentation without requiring Windows items on Unix;
`src/runtime/mod.rs` links to the published unstable tuning APIs and states their
`tokio_unstable` requirement; `src/task/blocking.rs` lets rustdoc resolve
`AtomicBool` across compiler versions where its item kind differs. These keep
warning-denying stable Linux documentation buildable without suppressing lints.

## Consumer integration

The repository root `[patch.crates-io]` unifies direct and transitive Tokio users
(including Actix) on this copy. **Cargo does not inherit a dependency's root
patches into a consuming workspace.** External applications using Parrot as a
path or git dependency must add this to their own workspace root, adjusting the
path to their checked-out copy:

```toml
[patch.crates-io]
tokio = { path = "path/to/parrot/vendor/tokio" }
```

`parrot-abi/loader` requires the empty `tokio/parrot-native-progress` capability
feature defined by this copy. `parrot-app/dylib` and `parrot-node/dylib` inherit
that requirement. Missing the root patch now produces a Cargo feature-resolution
error instead of silently compiling with an unpatched runtime. The requirement
applies on every platform when native support is enabled; the native runtime
repair itself remains specific to Linux GNU. `parrot-remote` separately requires
`tokio/parrot-deferred-mpsc` on every platform, including non-native app/node
consumers that depend on the remote layer. Those consumers also need the root
patch. Pure ABI without the loader requires neither marker nor Tokio.

If a consumer already locks a newer registry version, merely adding the patch
may still fail resolution. Select the patched version explicitly and retain the
resulting lockfile:

```sh
CARGO_BUILD_JOBS=2 cargo update -p tokio --precise 1.53.3
CARGO_BUILD_JOBS=2 cargo tree --locked -i tokio
```

Use the tree output and Cargo metadata to verify that all runtime users resolve
to this directory. If a dependency requires a newer incompatible version, resolve
that requirement before claiming the patch is active. Recheck after dependency
updates; build and test with `--locked`. Do not substitute only a direct Tokio
path dependency: that can create two incompatible Tokio package identities.
The marker prevents accidental omission, not deliberate substitution: an empty
feature is not cryptographic evidence of the implementation. Continue verifying
the selected source and running the runtime contracts. The integration gate
below exercises real external consumers and rejects missing-patch native builds:

```sh
CARGO_BUILD_JOBS=2 python3 scripts/check_native_runtime_dependency.py \
  --output-dir target/native-consumer-contracts --target-dir target
```

Use a new output directory for each run; it retains generated consumers, logs
and intermediate lockfiles. Full execution requires Linux GNU and a C compiler.
`--resolution-only` checks dependency selection without claiming compilation or
runtime coverage; `--offline` uses an already populated Cargo cache. Three native
consumer paths each execute the eight real C-finalizer contracts. The pure ABI
consumer compiles without the root patch; three non-native app/node configurations
require the deferred-channel patch and compile with it. Missing-patch cases must
fail on a required capability, and patched consumers must resolve every required
capability. A real registry 1.53.2 lock
checks rejection and the explicit update recovery path.

Publishing independent Parrot crates needs a separate packaging decision or an
upstream release containing the fix; this patch is not automatically published.

## Maintenance and removal

Keep the upstream source and license intact. When upgrading, verify the new
archive checksum, review the five native runtime changes, three deferred-channel
changes, three documentation corrections and both capability requirements, and
run `native_runtime_progress` and `dylib_assembly` with the real workspace
feature union on Linux GNU. The former must detect both cold-thread defects in
the unpatched implementation and pass with the patch; retain its current-thread
positive control and synchronous oneshot/mpsc/shutdown cases. Check the minimal
`sync + parking_lot` feature combination without a runtime as well. Run workspace
debug/release, strict Clippy, formatting and packaging checks. Retain negative
controls for deferred notification and receptionist lock boundaries; run
`mpsc_deferred_contract` plus the actual pinned upstream mpsc, oneshot and
AsyncFd suites with `scripts/check_tokio_upstream.py`. Retain negative
controls for missing initialization and initialization after the parker mutex is
acquired. Remove this local patch only after an upstream replacement passes
those contracts; merely upgrading to 1.53.2 did not fix the independent probes.
Update the capability and external consumer gate together with any replacement;
do not remove the guard while native consumers still require this repair.
