# Native Actix fixed-work benchmark

Run `./run.sh [report-path]`. Cargo uses `--locked --release` and honors
`CARGO_TARGET_DIR`; a failed build or failed run returns a nonzero status. The
full workload takes several minutes and is intentionally not a normal unit test.
`--help` prints usage without running it. Default report: `/tmp/parrot_bench_actix_raw.md`.

Each CPU reply is compared with a reference result prepared before measurement
on joined OS threads, using a separate recurrence implementation. Small literal
vectors (including a state that reaches 42) guard the reference and measured
kernels. Reference generation heats the host; this is a verified workload run,
not a controlled cold-start performance certification. Wall time includes reply
validation and orchestration. Measurements use monotonic `Instant`; the report
also records wall-clock run identity, OS, architecture, worker count and build.

The native suite currently records 22 rows:

- Sequential/concurrent echo, request/reply throughput and CPU workloads validate
  actual replies. Native `do_send` rows validate message count and business checksum
  after a mailbox barrier; submission time and completion time are distinguished.
- Timeout uses an acknowledged blocked handler and requires `MailboxError::Timeout`;
  a closed mailbox is not accepted as timeout. Release and completion are verified.
- Herd throughput spans creation through all replies. Stop storms wait for every
  actor to disconnect and for destruction; sleeps are not lifecycle proof. The
  storm counts 5050 messages: 5000 stop requests and 50 echo checks.
- Cross-actor probes are matched to timestamps captured inside actual heavy work.
  A request stalled past the entire interval remains in overlapping latency samples;
  zero completions in the interval is a valid observed outcome.
- M1 latency contains probe timings only, never business values. Probe receipt time is captured before local validation. Its message count
  includes 48 workload replies plus probes. All work samples and task joins must succeed.
- M2 submits a probe while its long handler is gated, then releases computation.
  This proves queueing behind that handler; it does not claim that all concurrent
  producers were admitted ahead of the probe.
- M3 performs the same fixed recurrence with identical seed and global iteration
  index in both variants. `AtomicResponse` serializes the async actor, matching the
  Parrot async path. Both probes are submitted before computation and must complete
  after it. Yielding scheduler capacity does not make the actor reentrant.
- E1 is two caller-mediated requests, not autonomous actor-to-actor ping-pong.
  E2 is two sequential request/reply operations, not ask/tell or self-messaging.

Every scenario is followed by actor-retirement checks before the next major
scenario. The pool owns all Arbiters, stops and joins them on normal completion
and during unwinding. The report is written only after correctness and cleanup
succeed; write errors fail the process. An existing report is from its recorded
run identity, not proof that a later failed invocation succeeded.

No CPU utilization is inferred from a hardcoded iterations/sec constant. Missing
latency samples or zero elapsed time render `n/a`. Saved reports include scenario
notes and sample counts. A false correctness row or empty report fails verification.

The root Parrot stress suite has different startup, registry and admission costs,
retains actors for a different lifetime, and its M2 still measures a post-completion
sentinel. Do not subtract rows to claim a universal wrapper tax. A comparable
campaign needs matched scenarios, validated lifetimes, identical build settings,
repeated interleaved trials, host metadata and uncertainty estimates. This benchmark
alone does not certify production throughput or tail-latency SLOs.

Validation:

```sh
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo test --locked --release
python3 tests/test_launcher.py
```

CI runs these contracts, not the full performance experiment on shared runners.
