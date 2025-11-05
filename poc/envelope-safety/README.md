# Production envelope memory safety

This independent workspace imports the actual production `single_alloc.rs` by
path and runs the same allocation/unwind contracts as the root integration test.
It uses the real `parrot-api` types and the repository's selected Tokio source;
there is no copied allocator implementation or substitute reply type.

Run `CARGO_BUILD_JOBS=2 ./scripts/miri.sh` from the repository root. The script
interprets all unit/integration targets with strict provenance and default leak
checking. It requires an installed nightly Miri component and rust-src. An
explicit test filter narrows the selected cases; an unmatched filter is an error.

This checks the standalone public envelope API. Thread mailboxes currently use
`AskEnvelope`/`AskPayload`; this harness does not claim that SingleAllocEnvelope
is their active path, nor does it replace engine, scheduler or end-to-end tests.
`new` allocates one message block; `new_ask` also allocates a oneshot state. Payload-
owned resources and complete actor exchanges have additional independent costs.
