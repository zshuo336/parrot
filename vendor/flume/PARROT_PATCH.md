# Deferred receiver notifications

The rendezvous regression test establishes the absence of a waiting receiver
through ownership, then checks zero-capacity Full/Timeout results, retained
payloads, empty queues, real handoffs and disconnection. It does not infer
sender startup from a sleep in another thread. A separate watchdog guards
liveness; scheduler latency is not treated as a channel timing guarantee.

Based on flume 0.11.1. `UPSTREAM.json` records the unmodified registry source
hashes; upstream licenses and tests are retained.

Parrot's managed typed mailbox must serialize admission against sealing. The
ordinary `try_send` implementation can invoke an asynchronous receiver waker
while both Flume's queue lock and Parrot's admission lock are held, and before
the message has been inserted. A waker that seals the managed channel deadlocks.
A reentrant application mutex alone would not make the queued message visible
before that callback runs.

`Sender::try_send_deferred` returns the send result and a notification guard.
It completes queue insertion or synchronous receiver handoff under Flume's
existing channel lock. Dropping the guard then notifies all selected receivers.
Parrot releases its admission lock before dropping this guard. Full and closed
errors retain their original payload and produce no notifications. Stream
waiters remain distinct from one-shot waiters, so all required receivers are
notified without duplicating the message. A panicking callback does not suppress
other selected notifications. Disposal of the retained hook is separately caught
because a cancelled future can leave the guard as a user waker's last owner;
its first panic resumes after notification, unless
already unwinding. Additional user panic payloads are deliberately leaked to
avoid their destructors skipping notifications or causing a second unwind.

Notifications record their responsibility before the channel lock is released.
Cancelling a selected receive future before delivery therefore transfers the
notification to another waiter. A blocking waiter receives the message in its
slot; an async waiter is notified without reordering the queue. Ordinary sends,
pending-send transfers, cancellation and disconnect also deliver their collected
notifications after releasing the queue lock. Initial waker cloning happens
before entering the queue lock, because a RawWaker clone is executable user
code. Notification takes the registered waker and invokes it outside the
registration lock, without cloning it there. A later poll reinstalls its current
waker when needed. Replacement clones and disposes wakers outside the lock and
rechecks the notification flag after cloning: the clone callback may itself
commit a message or release capacity and consume the old registration.

A receiver rechecks the queue and rearms under one lock, consuming the previous
notification flag exactly once. This avoids duplicate registrations on spurious
polls and a missed message between an empty read and rearming. Completed receive
futures remove their registrations immediately.

Existing public APIs remain available. Only the deferred API lets callers
postpone notifications beyond the call to release their own locks. RawWaker
contracts cover clone lock boundaries, sends and receives during replacement,
and clone panics preserving queue usability and the previous registration.
The caller remains responsible for a valid RawWaker vtable and its ownership.
Four elided return lifetimes and deprecated Cargo key spellings are also made
explicit without changing their types or dependency features.

The fork is consumed through explicit local path dependencies by parrot-api,
parrot and the local rumqttc dependency, including their use from standalone
example and POC workspaces. MQTT cleanup has the same reentrant publisher wake
hazard, so it must not silently use the unfixed registry source.
These dependencies deliberately have no registry fallback version: the upstream
0.11.1 package lacks the required API and fixes. Publishing to a registry requires
a separately published fork or an upstream release containing these changes,
then a verified dependency migration; current source builds use this directory.
The vendored test workspace retains a lockfile and is tested separately from the
main workspace. No performance benefit is claimed without measurement.

The local test and benchmark targets also pass strict Clippy without ignoring
warnings. Stream forwarding checks the complete ordered 0..1000 result and both
task completions under a shared timeout; the disconnect fixture requires its
send to succeed. Boolean assertions, imports, redundant casts and closures are
updated without reducing the original channel or benchmark workloads.

`Receiver::disconnect_deferred` closes admission under the channel lock and
returns a notification guard, leaving committed values readable or drainable.
Pending send registrations are removed without committing their values; those
values remain owned by the send futures. Repeated disconnection is idempotent.
Ordinary last-endpoint disconnection uses the same path. It must not pull pending
sends into a queue that has already been drained. Parrot's native MPSC mailbox
disconnects before draining and isolates notification failures from independent
message disposal. The new `disconnect` tests cover delayed callbacks, repeat
disconnection, preserved queued values, rejected pending sends and last-receiver
release. Public send/receive APIs retain their existing signatures.

The three blocking timeout/deadline tests check the API contract rather than a
five-millisecond operating-system scheduling window. An empty connected channel
cannot report Timeout before its deadline; errors distinguish disconnection and
preserve unsent values. Zero or expired deadlines still consume ready messages,
and timed-out registrations must not steal later sends or retain queued payloads.
A separate five-second watchdog detects a stuck test. These functional checks
no longer need the previous macOS/Windows ignore attributes. This does not change
channel implementation, performance gates, or claim real-time scheduling bounds.
