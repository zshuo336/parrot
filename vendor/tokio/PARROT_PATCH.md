# Parrot runtime progress patch

This directory is the Tokio 1.53.3 crates.io release under its original MIT
license. `UPSTREAM.json` records the release archive SHA-256 and all 564 upstream
file hashes. Sixteen upstream Rust files and the normalized `Cargo.toml` differ;
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

## Oneshot value handoff and waker retirement

`src/sync/oneshot.rs` keeps values detached by receive or rejected send behind a
private unwind guard until the last channel owner has been released. A user
waker destructor can panic at that release point. The guard disposes the detached
value independently during unwind, preserving the original waker panic instead
of overlapping it with a value destructor panic. Normal values and send/receive
signatures are unchanged; this adds no heap allocation or callback indirection.

Final channel disposal attempts both initialized receive/send wakers separately,
then resumes its first failure unless the caller was already unwinding. Panic
payload destructors have a separate boundary; recursive secondary payloads are
deliberately leaked. This cannot recover from an internal double panic within a
single user destructor, panic=abort, or synchronous blocking. A panicking waker
may prevent a receive from returning its value; the detached value is then
disposed, and ordinary poll/try_recv callers can catch the original panic.

## Blocking task ownership on worker creation failure

`src/runtime/blocking/pool.rs` commits a blocking task to the shared queue only
after worker creation succeeds, or an existing worker can service it. The same
pool mutex still covers the worker decision and queue insertion. An idle worker
cannot consume a notification before the task is queued. The upstream temporary
OS failure fallback to existing workers is preserved.

If the first worker cannot be created, the task is shut down outside the pool
mutex before returning the spawn error. A panicking thread-name callback also
leaves no queued task. Previously, `spawn_blocking` could panic while retaining
the closure in the queue: captures were not retired, and a later successful
spawn could run the supposedly failed task. Public spawn panic behavior is
unchanged; a failed dispatch now releases its task ownership.

Parrot preserves the actual input-disposal receipt when dispatch fails. Real
Linux cgroup thread exhaustion covers all three mailboxes and a direct Tokio
job; an ordinary regression covers worker-builder callback panic and recovery.
The pinned upstream `task_blocking` and `rt_spawn_blocking` suites exercise normal
blocking execution, nested blocking, shutdown, paused time and the lost-spawn
race alongside those failure contracts.

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
patch. The core `parrot` crate also requires `tokio/parrot-blocking-ownership`
for failed blocking-dispatch cleanup, including applications without remote or
native loading. Pure ABI without the loader requires neither marker nor Tokio.

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
consumer paths each execute sixteen real C-finalizer contracts: eight runtime
parking/timer contracts and eight cold Parrot trace-entry contracts. Native test
consumers explicitly depend on `parrot-api` for those trace probes; this is a dev
dependency, and the pure ABI consumer still requires neither it nor Tokio. The pure ABI
consumer compiles without the root patch; three non-native app/node configurations
require the deferred-channel patch and compile with it. App host and node
consumers additionally require the core blocking-ownership patch. A core-only
consumer independently checks rejection without the patch
and successful compilation/execution with it. Missing-patch cases must
fail on a required capability, and patched consumers must resolve every required
capability. A real registry 1.53.2 lock
checks rejection and the explicit update recovery path.

Publishing independent Parrot crates needs a separate packaging decision or an
upstream release containing the fix; this patch is not automatically published.

## Notification fanout under waker panic

Storing a terminal watch value does not prove that every pending observer has
been notified. Upstream `WakeList` stops at the first panicking waker, `Notify`
can abandon later batches, and watch's `BigNotify` can skip later shards. Catching
the panic only at the Parrot publisher therefore leaves independent observers
asleep even though the terminal value is available.

`src/util/wake_list.rs` now drains its wake batch before resuming the first panic.
`Notify` completes all batches and watch completes all shards before propagating
that same panic. Later panic payloads are disposed under `catch_unwind`; a payload
from a recursively panicking destructor is retained rather than double-panicking.
Callbacks run outside the existing locks. Reentrant registrations remain in the
next notification generation. Normal notification and unchanged watch updates
retain their existing semantics.

Semaphore permit release, partial-acquisition cancellation and close also span
multiple wake batches. They now finish assigning every released permit and
notifying every affected waiter before resuming the first wake panic. Without
this outer boundary, an aborted release loses its unassigned permit remainder
and close can leave independent waiters asleep. Reentrant acquisition retains
FIFO order, and excess permits remain available after the panic.

`tests/parrot_notification_fanout.rs` adds sixteen contracts for multiple batches
and shards, cancelled waiters, first-panic identity, hostile payload destructors,
reentrant registration, sender close, and unchanged updates. The gate runs these
contracts with both full runtime features and minimal `sync + parking_lot`, in
debug and release, alongside the pinned upstream Notify, watch and borrowed/owned
semaphore suites. Semaphore cases include permit conservation, cancellation,
close, reentrant FIFO ordering and hostile panic payloads. This
does not change the public Tokio API or suppress the caller's first wake panic;
Parrot's existing publishers continue to contain it at their ownership boundary.

The gate also runs the pinned borrowed/owned Mutex and RwLock suites in debug
and release. These locks share the semaphore implementation; their cancellation,
fairness, mapping, downgrade and multithreaded behavior must remain covered when
the underlying wake and permit-distribution paths change.

## Maintenance and removal

Keep the upstream source and license intact. When upgrading, verify the new
archive checksum, review the five native runtime changes, three deferred-channel
changes, three documentation corrections, blocking task ownership, notification
fanout and all three
capability requirements, and
run `native_runtime_progress` and `dylib_assembly` with the real workspace
feature union on Linux GNU. The former must detect both cold-thread defects in
the unpatched implementation and pass with the patch; retain its current-thread
positive control and synchronous oneshot/mpsc/shutdown cases. Check the minimal
`sync + parking_lot` feature combination without a runtime as well. Run workspace
debug/release, strict Clippy, formatting and packaging checks. Retain negative
controls for deferred notification and receptionist lock boundaries; run
`mpsc_deferred_contract` plus the actual pinned upstream mpsc, oneshot and
AsyncFd, blocking-task, Notify, watch, semaphore and notification-fanout suites with
`scripts/check_tokio_upstream.py`. Retain negative
controls for missing initialization and initialization after the parker mutex is
acquired. Remove this local patch only after an upstream replacement passes
those contracts; merely upgrading to 1.53.2 did not fix the independent probes.
Update the capability and external consumer gate together with any replacement;
do not remove the guard while native consumers still require this repair.
