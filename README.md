# Parrot Actor Framework

Parrot is a Rust actor framework with a shared API, Actix and thread runtime implementations, a remote protocol stack, and application orchestration across Rust, JVM, Erlang, and Python gateways.

The project is under active development. Local actor runtimes and remote request/reply paths have executable contracts; cluster recovery, control-plane authorization, and some orchestration capabilities remain incomplete. See the [review and repair ledger](docs/review/independent-2026-10-08/REVIEW_AND_REPAIR.md) for verified behavior and outstanding work. Design documents describe both implemented behavior and planned capabilities.

应用声明的默认资源预算、失败顺序和宿主 API 见[应用资源准入](docs/APPLICATION_ADMISSION.md)。

## Run your first actors

From the repository root, with Rust 1.98.1 (the toolchain used for current validation):

```sh
cargo run --locked -p parrot --example quickstart
```

The [complete quickstart source](crates/parrot/examples/quickstart.rs) creates one counter on each engine, sends typed requests, checks the returned values, shuts down both systems, and exits. It needs no external services or Ctrl-C. Its application output is:

```text
Actix: count=5
Thread: count=5
```

External path/git consumers using `parrot-remote` (including the app and node
crates), or native ABI loading, must select this repository's Tokio
copy in their own workspace root. Cargo does not inherit a dependency's root
patches. Add `[patch.crates-io]` with `tokio = { path = "path/to/parrot/vendor/tokio" }`
and retain the resulting lockfile. See the [runtime patch integration guide](vendor/tokio/PARROT_PATCH.md)
for capability checks, existing-lock migration and independent consumer gates.

Start with this source when creating an application:

1. Derive `Message` with `#[message(result = "...")]` on each message. The example uses separate `u32` and `String` result types.
2. Bind the chosen engine as `__parrot_engine`, and derive `ParrotActor` with `#[ParrotActor(engine = "actix")]` or `#[ParrotActor(engine = "thread")]`. The context type is specific to that engine.
3. Handle `BoxedMessage` by downcasting it to a concrete message and returning a boxed result. The typed `ask` interface checks and extracts the message's declared result type.
4. On Actix, use `actix::System::new().block_on(...)`. The example selects `async_handler = true`; the derive also requires a synchronous `handle_message_engine` method, which that mode bypasses. A thread-only application can use `#[tokio::main]`.
5. Create the engine system (`ActixActorSystem::new().await` or `ThreadActorSystem::shared(...)`), spawn with `EmptyConfig`, await each request, and explicitly shut down the system. The thread `shared` constructor installs the internal weak reference required by actor contexts.

`ask(...).await` waits for a reply and reports errors. `tell(...).await` waits for admission and reports rejection; successful admission does not confirm handler completion or durable delivery. Dropping the `tell` future before polling it sends nothing. See [semantic changes](docs/SEMANTIC_CHANGES.md) before migrating older examples or applications.

`ThreadActorRef` also exposes an inherent `ask(BoxedMessage)` method. The quickstart uses `ActorRefExt::ask(&actor, message)` on that reference to select the shared typed interface explicitly; importing the trait alone does not override an inherent method.

For static typed actors, [typed_death_watch.rs](crates/parrot/examples/typed_death_watch.rs) shows a Thread watcher observing an Actix actor through a different entry protocol. Run `cargo run --locked -p parrot --example typed_death_watch`; see the [DeathWatch contract](docs/DEATH_WATCH.md) for bounded delivery, cancellation, and lifecycle semantics.

Dynamic actors can receive idle events on either engine. Run `cargo run --locked -p parrot --example receive_timeout`; see the [receive-timeout contract](docs/RECEIVE_TIMEOUT.md) for activity reset, cancellation, bounded delivery and independence from ask deadlines.

Stream callbacks now run through real serial actor dispatch on both engines. Run `cargo run --locked -p parrot --example actor_stream`; the [stream contract](docs/STREAMS.md) covers custom handlers, processing backpressure, shared task quotas and disposal.

Managed contexts can create owned child actors on either engine. Run `cargo run --locked -p parrot --example child_actors`; the [child actor contract](docs/CHILD_ACTORS.md) explains initialization, bounded admission, recursive disposal and executed failure policies. Run `cargo run --locked -p parrot --example child_supervision` for OneForAll Stop on both engines. For child OneForOne reconstruction, run `cargo run --locked -p parrot --example child_factory` (Thread) or `cargo run --locked -p parrot --example actix_child_factory` (Actix). Run `cargo run --locked -p parrot --example group_restart` for coordinated child OneForAll reconstruction on both engines; see the [group recovery contract](docs/GROUP_RECOVERY.md). Managed dynamic handlers now support Resume. Static typed instances also support explicit Stop／Resume; run `cargo run --locked -p parrot --example typed_supervision` and see the [static typed supervision contract](docs/TYPED_SUPERVISION.md). Thread and Actix static root factories now support bounded reconstruction through original typed references; run `cargo run --locked -p parrot --example typed_factory`. Thread and Actix static parents now create owned typed child instances through a generation-owned context; run `cargo run --locked -p parrot --example typed_children` and see the [typed child contract](docs/TYPED_CHILDREN.md). Static child factories now inherit or override the parent policy and support coordinated OneForAll reconstruction; run `cargo run --locked -p parrot --example typed_child_factory`. Static parents can Resume child escalation without abandoning their pending init or handler; run `cargo run --locked -p parrot --example typed_escalation_resume`. Dynamic parents now also preserve their in-flight execution on escalation Resume across Thread schedulers and Actix async/synchronous entry points; run `cargo run --locked -p parrot --example dynamic_escalation_resume`. Failed initialization and root cohorts remain outside these contracts.

Thread and Actix root factories register before construction, bound constructor concurrency and retain cleanup ownership through timeout. Run `cargo run --locked -p parrot --example factory_actors` (Thread) or `cargo run --locked -p parrot --example actix_root_factory` (Actix); see the [factory contract](docs/FACTORY_ACTORS.md).

For engine-neutral async actors, run `cargo run --locked -p parrot --example neutral_runtime` (instances, factories and parent-owned children). Run `cargo run --locked -p parrot --example facade_runtime` for the existing facade with native and application-defined runtimes, named creation, actual lookup and shared shutdown. See the [engine migration guide](docs/ENGINE_MIGRATION_GUIDE.md) for selection, replacement, cancellation and capability boundaries.

More examples live in [crates/parrot/examples](crates/parrot/examples). Some are interactive or require services; they are not all finite smoke tests. The [websearch application](apps/websearch/README.md) has a separate build and integration workflow.

The application runner can explicitly record incoming request chains and export them over OTLP/HTTP with `run --trace-otlp <https-url>`. It checks delivery and loss counts during shutdown; see [application tracing](docs/APP_TRACE_INGRESS.md) for options and the supported capture boundary.

For persistent delivery, the application host provides a [CloudProxy component](docs/CLOUD_PROXY.md) with a [manifest example](crates/parrot-app/examples/cloud-proxy.app.toml). Its acceptance reply follows WAL sync; remote completion and transactional receiver state are separate guarantees. Optional [durable submission IDs](docs/IDEMPOTENT_SUBMISSIONS.md) preserve the original sequence across acceptance-reply loss, ACK cleanup, and restart.

## Repository map

- `crates/parrot-api`: actor, message, reference, context, and system interfaces.
- `crates/parrot-api-derive`: message and actor procedural macros.
- `crates/parrot`: Actix and thread runtimes, shared façade, and examples.
- `crates/parrot-config`: configuration parsing, validation, and overlays.
- `crates/parrot-remote`: wire protocol, transports, remote references, and cluster/storage building blocks. The presence of a module does not establish a complete cluster runtime.
- `crates/parrot-app` and `crates/parrot-node`: application assembly and node entry points.
- `interop/`: JVM/Scala, Erlang, Python, C ABI, and TypeScript protocol implementations and adapters, with different capability levels. See the [TypeScript lite connection contract](docs/TYPESCRIPT_LITE_CONTRACT.md) for supported transports, explicit pb/bin payload negotiation, deadlines and limits. TS messaging takes caller-encoded bytes; the crawler example selects bin for the JVM gateway. See the [JVM codec contract](docs/JVM_CODEC_CONTRACT.md) for scalar/protobuf compatibility and the real Rust/JVM gate.
- `apps/`, `tools/`, `deploy/`: applications, lab/observability tools, and deployment assets.
- `docs/`: design documents, protocol vectors, semantic changes, and validation records.

## Validate changes

The [runtime test gate](docs/RUNTIME_TESTING.md) runs the complete `parrot` package in default/legacy envelope modes and debug/release profiles. It discovers new tests automatically and includes strict lint; CI configures all four combinations on Linux and macOS.

Full workspace tests require Node/npm, a C++ compiler (`clang++` by default), Java 21/Maven, Erlang, and an activated CPython 3.11 environment. Prepare the real gateway and crawler business artifacts from the current checkout first:

```sh
export CARGO_BUILD_JOBS=2
python3 scripts/check_workspace_lockfiles.py
npm ci --prefix interop/typescript-lite
sh deploy/images/install-ray.sh
python3 -m pip install --no-deps -e ./interop/python
rustup target add wasm32-unknown-unknown
cargo install wasm-tools --version 1.261.0 --locked
make build-crawler-components build-ts build-crawler-business

cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --features parrot-app/wasm,parrot-app/dylib,parrot-node/wasm,parrot-node/dylib,parrot-remote/mem -- -D warnings
cargo test --locked --workspace --features parrot-app/wasm,parrot-app/dylib,parrot-node/wasm,parrot-node/dylib,parrot-remote/mem --no-fail-fast
cargo run --locked -p parrot --example quickstart
```

Every source workspace includes its `Cargo.lock`, including standalone Wasm applications, POCs, benchmarks and fixtures. The lockfile check discovers workspace roots through Cargo and rejects missing or Git-ignored locks. The dependency security gate uses the same discovery before auditing them.

Workspace tests already exercise real external peers; explicitly ignored integration and performance gates remain unexecuted. Run additional gateway and application contracts through the [Makefile](Makefile):

```sh
make test-jvm-gateway
make test-app-host
make test-app-polyglot
```

These targets fail on missing prerequisites. Workspace tests alone do not establish gateway, Linux/container, release-performance, or deployment acceptance. CI runs separate jobs for some of these scopes; the repair ledger records actual results and remaining gaps.

The [A5 registration performance contract](docs/A5_PERFORMANCE.md) distinguishes the current reference-host floor (>50000 registrations/s) from the original, still unmet >300000/s target. Both execute the full 20000-actor workload, validate 40000 replies, and require actual retirement. Passing the current floor does not establish the original target or portable performance across hardware.

## Observability

For running gateways, build the tools and pass explicit node endpoints:

```sh
make build-probe
./target/debug/parrot-obs status erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872
./target/debug/parrot-obs metrics erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872
```

See [observability documentation](docs/OBSERVABILITY.md) for the CLI/Web reference and protocol. Metrics and tracing coverage still vary across runtimes; consult the ledger for known gaps.

## Contributing

Include a reproducible failure, a focused fix, and validation of observable behavior. Preserve test failures and skipped-scope information in reports, and update affected examples and semantic documentation when changing a public contract.

Remote TCP/QUIC authentication, control authorization and development-mode migration are documented in [REMOTE_SECURITY.md](docs/REMOTE_SECURITY.md).

Persistent Directory groups can now run through the standard node's `directory` role and explicit `remote.directory` configuration, with a `/system/directory` actor for local reads. See the [configuration and lifecycle contract](docs/STANDARD_DIRECTORY.md), [authenticated writes with persistent request IDs](docs/DIRECTORY_CLIENT.md), and [explicit linearizable reads](docs/DIRECTORY_LINEAR_READS.md). Federation route resolution uses the bounded, versioned [`RESOLVE_Q/R/INVALIDATE` control plane](docs/RESOLVE_CONTROL_PLANE.md); local readiness still does not imply quorum availability.

观测指标的可用性、计数器实例身份与完整 Wire 帧计数口径见 [带可用性声明的指标采样](docs/MEASURED_METRICS.md)，附真实内存连接示例。跨语言采样与观测工具接线的当前边界在该文中单列。

Derived static actors can opt in to `TypedLifecycle` with `#[ParrotTypedActor(msgs(Query), lifecycle)]`, including initialization that creates owned children. Run `cargo run --locked -p parrot --example typed_children` for a finite Thread/Actix example; see [the lifecycle contract](docs/TYPED_CHILDREN.md).

For actors with `Rc` or other state confined to one thread, use `#[ParrotTypedActor(msgs(Query), local)]` with `LocalTypedReceive`; add `lifecycle` for `LocalTypedLifecycle`. The generated protocol works with `spawn_local_typed` and keeps messages and references Send. Run `cargo run --locked -p parrot --example local_runtime` for cross-thread calls, dynamic lookup, parent-owned Local children and owner-thread cleanup. `TypedContextExt::spawn_local_typed_child` creates a Local child in the current Actix parent generation; parent retirement waits for child cleanup. `spawn_local_typed_factory` and `spawn_local_typed_child_factory` retain their non-Send instances on one arbiter across supervised replacement; unread messages and logical references survive, while failed messages are not replayed. Neutral runtime Local root factories are available through the optional capability described below; Local child factories are available through `context.local_spawner()` on supported native, typed and Local parent contexts.


Local actors can also be created through the production engine catalog: query
`runtime.local_runtime()` and call `spawn_local_factory` with a Send constructor
returning `Box<dyn LocalActor>`. Non-Send state is created and retained on its
owner thread; Thread explicitly reports this capability as unavailable. The
`neutral_runtime` example exercises both engines and Local capability selection,
including raw dynamic messages, native/Local child scopes, and real registry/death-watch identity. See
[semantic details](docs/SEMANTIC_CHANGES.md#engine-neutral-local-root-factories).
