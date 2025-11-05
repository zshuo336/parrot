# Akka Typed benchmark

Requires JDK 21, Python 3, Rust and the six pinned JARs in `lib/` (Akka 2.6.20,
Scala 2.13.10, config 1.4.2, Scala Java8 compat 1.0.2, SLF4J API 1.7.36).
This repairs the existing Akka baseline; it does not establish that these versions
are current or recommend them for new production deployments.

```sh
make check-akka-bench             # rejection contracts + all 20 small scenarios
bench/akka-bench/run.sh --full    # original full counts/iterations, no scaling
bench/akka-bench/run.sh --full --dispatcher-threads 4
```

Default invocation runs full work. `--check` is correctness validation, not a
performance run. Full computation includes eight 57.2-billion-iteration jobs and
can take many minutes. Each business/worker deadline is one hour; cleanup has a
10-second budget. The launcher defaults to 15 dispatcher threads, a 1 GiB heap
and ZGC. Output records the selected thread count and mode. Hardware, CPU quota,
JDK version and runtime flags must accompany any performance interpretation.

The launcher compiles Java with `-Xlint:all -Werror` and the independent Rust
reference with `-D warnings -O` into a fresh private directory. It generates the
exact CPU input set and all reference answers before starting the benchmark JVM.
Literal vectors check both implementations, including a first result of 42 that
must not end a two-iteration job. Output includes the oracle SHA-256. Every ask
checks the returned value; tells check exact drained counts and modulo-u64 sums.
No arbitrary nonzero-result test is a correctness oracle.

IO completion is an actor message. Active IO and its queue are owned by that
actor; one actor serializes its delays while separate actors overlap. Stop cancels
timers and fails outstanding locally tracked requests. Every stage observes real
Akka `Terminated` events, after `PostStop`, before reusing stage resources. Worker
failures cancel and join siblings; probe failures propagate, samples are bounded,
and the ticker is joined before samples are read. The runtime owner cancels any
unfinished CPU/gate work on exceptional cleanup. The launcher propagates compiler,
oracle and JVM failures without output-filter pipelines; SIGINT/SIGTERM kill and
reap its active tool process group. Abrupt SIGKILL of the launcher is not covered.

Measurement details:

- Full mode retains all original 20 scenarios and their work counts. The CPU
  parallel label now correctly says 64 actors. Storm validates all 5,000 actors,
  not 50 samples; its 10,000 units mean echo requests plus stop requests.
- CPU timing uses `System.nanoTime`. Quantiles use integer nearest rank. Zero or
  nonfinite durations and false rows fail the process; no fabricated throughput.
- Warmup is 3,000 validated echo asks. This is not complete workload-specific JVM
  steady-state warmup. Independent oracle compilation/execution is untimed.
- Gate handshakes establish queued timeout/FIFO probes. Timeout is a caller's
  `CompletableFuture.get(1ms)` timeout and does not cancel queued actor work.
  Raw sends after observed termination get no reply within a stated 50ms window;
  this is not a claim of immediate mailbox rejection by Akka.
- Probe timestamps are compared with the actual first/last CPU execution times.
  The combined mixed-work interval is an envelope, not continuous saturation.
  Zero overlap (possible in small checks) means there is no latency evidence.
  Probe counts exclude pre-window baseline requests from throughput; total
  probe accounting is still checked. Probe/control overhead affects timings.
- Chunked/solid perform the same full recurrence, preserving state and global
  iteration index across 20 chunks. Chunk probes execute after an admitted first
  chunk and before job completion. Gate synchronization is included. The row has
  four business requests (two computations and two tiny probes), not two; internal
  continuation and snapshot/control messages are not throughput business units.
- Pingpong is two sequential client asks, not actor-to-actor messaging. The chain
  is client ask then tell to one actor. Herd wall includes spawn and every echo;
  spawn-only time is separately reported. Normal per-stage retirement is outside
  timing except storm, where spawn/echo/retire are the measured workload.

Results and notes go to stdout; there is no fixed `/tmp` report to overwrite.
Save output to a caller-chosen file if needed. Historical Akka results in
`docs/ENGINE_STRESS_REPORT.md` used weak/incorrect checks and are not current
validated comparisons. Equal-work, equal-parameter, repeated cross-runtime
performance experiments remain separate work.
