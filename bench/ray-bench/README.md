# Ray benchmark

Use Python 3.13, Rust and the pinned dependencies. Build a dedicated environment;
the launcher selects the interpreter through `PYTHON` (default `python3`).

```sh
python3.13 -m venv target/ray-bench-venv
target/ray-bench-venv/bin/python -m pip install -r bench/ray-bench/requirements.txt
export PYTHON="$PWD/target/ray-bench-venv/bin/python"
make check-ray-bench
bench/ray-bench/run.sh --full --scale 85.9375  # explicit historical iteration divisor
bench/ray-bench/run.sh --full                 # unscaled iterations, can take an hour or more
```

Small checks are not a full workload/performance certification. The pinned
environment is Python 3.13 / Ray 2.59.0 / psutil 7.2.2; its transitive package
versions are constrained by requirements-lock.txt. Historical candidate coverage
also includes Python 3.9 / Ray 2.51.2. See the batch-240 verification record under
`docs/review/independent-2026-10-08/ray-benchmark-240/` for the actual tested scope.

All original twenty Ray scenarios and their full message/actor counts are retained:
150 herd actors, three waves of 100 storm actors, 5,000 tells/slow-consumer jobs,
four long and twenty medium mixed jobs. These original Ray counts differ from
Parrot/Actix/Akka/Erlang; this is not an equal-work cross-runtime comparison.
Default scale is 1. `--scale 85.9375` preserves the old divisor explicitly, using
exact nearest-integer rounding (ties to even); it does not claim equal time.
`--check` caps counts and per-job iterations, runs rejection contracts, and then
executes all twenty small scenarios. It is not a full performance profile.

The default logical Ray resource pool is 15 CPUs and a 1,500,000,000-byte object
store. `--cpus` is a scheduling resource declaration, not a physical CPU quota.
Full mode needs at least seven logical CPUs so actors reserving one CPU each can
all start; check needs four. The 150-process herd can consume substantial RAM.
There is no claim of a hard 10 GiB total memory limit or a universal five-minute
runtime. Each business/group deadline is one hour; each launcher stage has a
six-hour outer deadline; actor/session cleanup has 30 seconds and launcher signal
cleanup allows 90 seconds before killing its owned process group.

The launcher compiles the Rust reference with `-O -D warnings`, generates the
exact CPU input set, and computes all reference answers before starting Ray.
Literal vectors exercise both implementations, including the first-step result
42 that must not end a two-step job. Oracle SHA256 and explicit run parameters
are printed. CPU results are compared exactly; tells check exact completed counts
and modulo-u64 checksums. Test rejection checks remain active under Python `-O`.

Ownership and failure semantics:

- `ray.init(address="local")` always creates an owned local cluster. An existing
  `RAY_ADDRESS` cannot cause attachment to another cluster. There is no global
  `ray stop`, process-name kill, shared session deletion, or global Ray shutdown.
- Each Ray 2.59 cluster uses its own 0600 token file. User-level credentials are
  neither reused nor generated; the token path is confined to the private session.
- Temporary sockets, logs and reference files live under a fresh short `/tmp/prb-`
  directory, avoiding macOS Unix-socket path limits. Child workers receive the
  explicit local module path. This driver is for one local node; it does not
  provide distributed code deployment to remote Ray nodes.
- Actor retirement checks a post-kill `RayActorError` and observes the matching
  PID/creation-time pair exit. Scope cleanup kills only created handles. Session
  cleanup closes its cluster and verifies owned descendants; process enumeration
  failures cannot bypass `ray.shutdown()`.
- Driver workers propagate any failed future immediately, cancel waiting siblings
  and join. Probe threads validate replies, propagate failure, bound sample storage
  and join before samples are read. Python threads cannot be forcibly killed;
  owned driver tasks use cooperative waits or finite submission loops.
- SIGINT/SIGTERM reach the child driver, allowing scope/session cleanup. The
  launcher then kills and reaps its own remaining process group, including children
  left by an already-exited leader. SIGKILL of the launcher is not covered.
- A real two-cluster test keeps an independent sentinel actor responding while
  interrupting the benchmark, proving its cluster and actor identity survive.

Measurement contracts:

- Warmup is 500 validated echoes. Resource readiness is measured explicitly for
  herd/storm. Normal stage retirement is outside timing; storm includes all retirements.
- `CLOCK_MONOTONIC` timestamps share a system-wide epoch even on Python 3.9/macOS;
  elapsed times never use adjustable wall clocks. Quantiles use integer nearest rank.
  False rows, invalid/zero duration, runtime errors and bad results fail the process.
- Gates prove the timeout/FIFO probe is submitted behind held work. The caller's
  `ray.get` timeout does not cancel the task; its eventual reply is also checked.
  FIFO now includes its tail request (306 full-mode units).
- IO uses actual `asyncio.sleep` under an actor-local `asyncio.Lock`. All eight
  actors and all 320 results are checked, together with peak per-actor concurrency.
- Chunked/solid use the same state, salt and global index for the same total work.
  The chunk actor yields through async scheduling, and a gate proves a probe is
  served after the first chunk and before completion. There are four business
  requests across both variants, with gate overhead included. Internal continuation,
  control and snapshot work is not counted as a business throughput unit.
- Probe windows use actual CPU start/end timestamps, not guesses. A mixed-work
  envelope does not prove continuous saturation. Zero overlapping samples means
  no latency evidence. Pre-window probes do not inflate throughput, but all probe
  counts and values are still validated.
- Pingpong is two client asks; the chain is client ask then tell. Storm validates
  all 300 actor processes and counts 600 echo+kill units, not thirty spot checks.
- Results and explanatory notes go to stdout. Save to a caller-selected log. No
  fixed report or session path is overwritten. Full lifecycle counts include the
  gate/control actors; they are distinct from reported business message counts.
