# Production envelope memory safety

This independent workspace imports the actual production `single_alloc.rs` by
path and runs the same allocation/unwind contracts as the root integration test.
It uses the real `parrot-api` types, production `parrot::thread::ReplyReceiver`,
and the repository's selected Tokio source;
there is no copied allocator implementation or substitute reply type.

This workspace is outside the root workspace. A root `cargo test --workspace`
does not execute its tests. For a native run, use
`cargo test --locked --manifest-path poc/envelope-safety/Cargo.toml --all-targets`.
Native allocation/unwind checks do not replace the interpreter run below.

Run `CARGO_BUILD_JOBS=2 ./scripts/miri.sh` from the repository root. The script
interprets all unit/integration targets with strict provenance and default leak
checking. It requires an installed nightly Miri component and rust-src. An
explicit test filter narrows the selected cases; an unmatched filter is an error.

This checks the standalone public envelope API. Thread mailboxes currently use
`AskEnvelope`/`AskPayload`; this harness does not claim that SingleAllocEnvelope
is their active path, nor does it replace engine, scheduler or end-to-end tests.
`new` allocates one message block; `new_ask` also allocates a oneshot state. Payload-
owned resources and complete actor exchanges have additional independent costs.
The shared contracts check exact allocation retirement even when payload Drop
or a reply waker panics, independent reply ownership after transfer, mismatched
downcasts and over-aligned zero-sized payloads. Passing these selected operations
does not prove every unsafe API, arbitrary user destructor, or the full runtime
memory model. Production integration, coverage and performance remain separate
acceptance work under K11/K21/K22/K26.
