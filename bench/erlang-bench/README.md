# Erlang workload correctness and measurements

```sh
make check-erlang-bench
bench/erlang-bench/run.sh --full                # scale 1: unscaled Rust iteration counts
bench/erlang-bench/run.sh --full --scale 24     # explicit historical Erlang work budget
```

Requirements: POSIX host, Python 3.9+, Rust compiler, Erlang/OTP 27+ (`erl`,
`erlc`, `escript`). Compilation uses a private temporary directory and `-Werror`;
no existing `out/*.beam` files are loaded. Compiler, oracle and runtime failures
fail the command. SIGINT/SIGTERM stop the active tool process group and remove
its temporary build directory. This does not promise cleanup after SIGKILL of
the Python launcher. BEAM actors and workers are owned by their scenario process.

The twenty scenarios retain the original full message/actor counts, including
20,000 herd actors and 5,000 storm actors. All storm actors now perform their
verified echo: that row counts 5,000 echoes plus 5,000 stops. The FIFO row includes
its queued probe, and the chunk row counts one solid job plus twenty independent
chunks. The full default no longer divides CPU iterations; `--scale 24` explicitly
retains the previous iteration divisor (including timeout work's original
`floor(50,000,000 / scale) * 10`). A scaled run must not be compared with an
unscaled run as equal work. `--check` runs failure contracts and all twenty
scenarios with small, explicitly identified workloads; it is not a performance run.

Before warming or timing, a separate optimized Rust program computes the requested
answers using the independently reviewed Rust reference recurrence. Both languages
check literal vectors, including a case whose first step is 42 but must continue.
Every CPU reply is checked; tell workloads must have the exact count and modular
u64 result sum. Echo and IO payloads are checked too. The preflight oracle hash,
number of answers, scale and scheduler count are printed. Rust is not invoked
inside any measurement window. Preflight still heats the machine, so this is not
an isolated, cold-start performance experiment.

Time uses the monotonic clock. Each stage retires all its actors and joins its
workers before publishing its result; an error or owner failure reaps owned
processes. Stage wall time includes admission, business checks and drain, but
excludes that final retirement. The storm stage explicitly includes retirement.
A zero-duration microsecond measurement reports zero throughput rather than an
invented finite duration. Percentiles use nearest-rank integer indices. Empty
samples remain empty, not a fabricated latency observation.

The timeout and FIFO probes are enqueued while a known handler is held by a gate,
then computation is released. Requests use reply aliases so a timeout cannot leave
late replies in the caller's mailbox. Producer send durations end when the producer
actually finishes sending; each producer performs its own FIFO drain fence.
Medium jobs are drained as well as long jobs. Probe errors fail the run. Probes
are timestamped and bounded to one million samples, with a five-millisecond delay
between subsequent samples. The long-run starvation row reports only intervals
that overlap actual computation; the mixed row reports overlap with its admission,
compute and drain window. Short contract runs need not contain a starvation sample. Business requests and
worker collections have a one-hour budget; actor retirement and forced cleanup
have separate ten-second budgets. A full unscaled run can take well over an hour
in total. These limits are explicit failures, not a guarantee of completion on
arbitrarily slow or overloaded machines.

The IO case sleeps serially inside each actor but proceeds concurrently across
actors. The ping-pong case consists of two sequential client asks, not actor-to-actor
forwarding. Chunk jobs each start from the same salt, whereas the solid job runs a
single longer recurrence; both answers are independently checked. Their timing ratio
is not a proof of equal algorithmic results or scheduler preemption.

A `correct=true` row is published only after checks and retirement succeed. The
final `ERLANG_BENCH_PASS` identifies mode, scale and all twenty completed rows.
These measurements do not establish cross-runtime superiority, wrapper overhead,
repeatability, equal resource budgets, or release performance certification. Earlier
Erlang numbers used a wrong addend and weak result/lifecycle checks and cannot be
reused for those claims. Repeated controlled, equal-work comparisons remain separate.
