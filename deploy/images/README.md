# deploy/images —— 框架通用网关运行时镜像

app 的 docker compose **引用**这些镜像（`build.context` = 仓库根 +
`dockerfile` = 本目录路径），自身目录保持完备——不复制、不侵入框架。

| 镜像 Dockerfile | 内容 | app 制品挂载点 |
|---|---|---|
| `gw-erl.Dockerfile` | erlang:27 + `parrot_gw`（编译好） | `/app/erlang`（beam 目录） |
| `gw-jvm.Dockerfile` | temurin:21 + `parrot-protocol-jvm` + libs | `/app/app.jar` + `/app/data` |
| `gw-ray.Dockerfile` | python:3.11 + ray + jieba + `parrot_protocol` | `/app/python`（模块目录） |
| `gw-app.Dockerfile` | debian + app 二进制（ARG BIN） | —（构建期 COPY） |

Ray 容器必须使用 Docker 的 `--init`，Compose 使用 `init: true`（仓库内两个 Ray 服务均已配置）。Python 不应直接承担容器 PID 1 的孤儿回收职责：调用方 driver 崩溃后，未回收的 Ray 服务僵尸会让其他 worker 的父进程存活检查失真，进而阻止部署 guard 发现 owner 丢失。init 负责操作系统的孤儿回收，不替代部署的 job、actor 和快照租约协议。自定义容器平台也应提供等价的 init/subreaper。

## 约定

1. **镜像 = 运行时 + 框架协议层**；**app 制品 = volume 挂载**（beam/jar/py）。
   挂载的业务组件更新无需重建网关镜像；Rust 应用二进制和框架协议升级需要重建对应镜像。
2. JVM 镜像构建前先跑 `deploy/images/prep-jvm-libs.sh`（把 `~/.m2` 依赖
   汇成 `gw-jvm-libs.tar` 供 COPY——cp.txt 绝对路径不可 COPY）。
3. app deploy uri（admin-v2 `file://`）指**容器内路径**（挂载点），即上表列。

## app 侧使用样例（websearch）

```yaml
services:
  erl-gw:
    build: { context: ../../.., dockerfile: deploy/images/gw-erl.Dockerfile }
    volumes: [ "../../erlang:/app/erlang:ro" ]
    command: ["erl","-noshell","-pa","/gw","-pa","/app/erlang",
              "-eval","parrot_gw:main([0, \"parrot=app:19870\"])"]
```

完整样例见 `apps/websearch/deploy/docker-compose.yml`。

## Build inputs and dependency preparation

`make build-jvm` prepares the gateway JAR and `interop/jvm/target/cp.txt`.
Then run `bash deploy/images/prep-jvm-libs.sh` with Python 3.9+ before building
the JVM image. Every classpath entry is included even without a trailing newline.
Each invocation reads and validates current JAR bytes; it never trusts timestamps
or an existing archive as a cache hit. Missing/corrupt JARs, empty entries and
conflicting basenames fail without replacing the previous archive. Identical
repeated paths are deduplicated; symlinks keep their classpath basename. Publication
uses a unique temporary file and atomic replacement, with deterministic gzip/tar
metadata and a final input-content check. A failed preparation must stop deployment;
the retained previous archive is not evidence that the new build succeeded.

The `gw-jvm.Dockerfile.dockerignore` and `gw-app.Dockerfile.dockerignore` files
replace the root ignore rules for these prebuilt images. The JVM context admits
its gateway JAR and the standard `gw-jvm-libs.tar`; a custom `LIBS_TAR` requires an
explicit corresponding allowlist entry. The application context admits direct
release artifacts selected by `BIN` and `TRIPLE`, including an empty `TRIPLE`,
while excluding debug output and nested compiler caches. Root target metadata may
also be included to traverse the selected build directories. The ELF validator is
also admitted. `TRIPLE=auto` (default) selects `aarch64-unknown-linux-musl` for
`--platform linux/arm64` and `x86_64-unknown-linux-musl` for `linux/amd64`.
An explicit `TRIPLE` selects that directory; an explicitly empty value selects
`target/release`. There is no existence-based fallback. The build-platform Python
stage checks the actual ELF64 machine, executable mapping and entry point before
copying the validated bytes into the target-platform Debian image. A renamed
wrong-architecture binary, Mach-O/PE file or truncated ELF fails the build. This
requires BuildKit and does not execute the target binary during validation.
Dynamic ELF executables remain accepted; compatible loaders and shared libraries
are the deployment owner's responsibility. This gate does not certify runtime
compatibility, signatures, provenance or complete ELF loader semantics.

Websearch retains an explicit ARM64 default. Use
`WS_PLATFORM=linux/amd64 apps/websearch/deploy/compose.sh up` for AMD64; the wrapper
selects the matching Rust target before prebuilding and Compose uses the same
platform. Install that Rust target and cargo-zigbuild/Zig first. Cross-architecture
execution requires host support or emulation. Other gateway images use the Docker
host default platform independently; their communication is over TCP.

Run `make check-image-artifacts` for the packaging and context contracts (Python
3.9+ and Go 1.22+). Context tests use Docker's pinned `moby/patternmatcher` instead
of a hand-written approximation. These checks do not build an image or verify its
Linux runtime; real Docker build and runtime acceptance remain separate gates.
Run `python3 scripts/tests/test_app_image_artifacts.py -v` for binary-selection and
ELF rejection contracts, and `python3 scripts/verify_app_image.py` on Linux with
Docker and a C compiler for actual image build, native execution and wrong-machine
rejection. Both are wired into the container CI job.

## Ray dependency installation and acceptance

The Ray image installs `requirements-ray-build.lock` and `requirements-ray.lock`
through `install-ray.sh`. Both pin exact versions and distribution SHA-256 hashes.
The runtime includes Ray 2.59.0, jieba 0.42.1 and virtualenv: Ray's base package
alone does not provide `runtime_env.pip`. The installer requires CPython 3.11,
checks package metadata with `pip check`, permits source builds only for jieba,
and disables isolated build dependency resolution. The separate tool lock fixes
pip/setuptools/wheel and their dependencies before that source build. These locks
are deployment inputs; the SDK's optional dependency range remains separate.

`deploy/parrot-stack` now builds the same gateway image. Enabling the `ray`
profile no longer downloads packages at container startup or replaces tested
protocol code with a host mount. Rebuild the image after protocol/lock changes:
`docker compose -f deploy/parrot-stack/docker-compose.yml --profile ray up -d --build`.
The dedicated Ray dockerignore admits only the protocol package and installer
inputs, excluding tests, Python caches and application code.

To update locks deliberately, run these commands from the repository root and
review the resolved versions and hashes before accepting the change:

```sh
uv pip compile deploy/images/requirements-ray-build.in --python-version 3.11 --python-platform linux --generate-hashes -o deploy/images/requirements-ray-build.lock
uv pip compile deploy/images/requirements-ray.in --python-version 3.11 --python-platform linux --generate-hashes -o deploy/images/requirements-ray.lock
```

For local verification, create a dedicated CPython 3.11 venv with pip, set
`PYTHON=/absolute/path/to/venv/bin/python`, and run `sh deploy/images/install-ray.sh`.
Use that interpreter for `scripts/tests/test_ray_image_runtime.py`, the full
`interop/python/tests` pytest suite, `apps/test_support/run_ray_environments.py`
and `apps/test_support/run_ray_lifecycle.py`, followed by
`apps/test_support/run_ray_shutdown.py` and
`apps/test_support/run_ray_network_shutdown.py`, then
`apps/test_support/run_ray_gateway_retirement.py`. The environment test actually
installs two distinct local fixture wheels into separate Ray hosts and checks
child inheritance, failed-setup effects and recovery. Merely importing Ray or
passing `pip check` does not establish those capabilities. CI uses the same
installer and also runs `scripts/verify_ray_image.py` to build and exercise the
actual image without an external container network. That verifier mounts a small
test-only tree, not the repository: its protocol-package path is an absolute
symlink to `/gw/parrot_protocol` inside the image. Before any runtime test, it
compares the packaged files (excluding Python caches) and SHA-256 values with the build input,
rejects implementation symlinks, and checks the actual import location. Missing,
stale or extra image files cannot be supplied by a host-source mount. The mounted
fixture then runs dependency, runtime environment, lifecycle and TERM tests against the
image package. Run `python3 scripts/tests/test_container_acceptance.py -v` for the
local provenance and verifier failure-path contracts; controlled Docker responses
in those tests are not evidence of a successful image build.

With the locked Ray 2.59 runtime, the CLI treats SIGTERM as a graceful stop, explicitly shuts down the runtime
it initialized, and returns zero after cleanup. Repeated TERM during that cleanup
does not interrupt it. Startup/configuration errors still fail; an existing
runtime owned by an embedding caller is not shut down by the CLI. The standalone
`serve()` API still leaves Ray job shutdown to its caller. It reuses an initialized
runtime; nonempty `ray_kwargs` cannot reconfigure that existing job. On unwind it closes
its listener, shuts down accepted sockets, and joins their Wire reader/writer
threads. Registration sockets and partially initialized connections are also
retired. It then seals ASK/management admissions, discards unstarted management
commands, waits for running ASK/admin operations, retires deployed components
and their snapshots, and confirms the coordinator's death. It does not shut down
the caller's Ray job or unrelated actors. Component retirement is retryable;
uncertain actor death or filesystem cleanup keeps the remaining ownership.

If retirement cannot complete, `GatewayCleanupError` exposes `error.owner`;
retain it and call `error.owner.close(timeout=...)` after resolving the blocking
condition. A pending remote close is reused, rather than resubmitted on timeout.
The initial `serve(..., shutdown_timeout=10.0)` budget applies to retirement waits
after the network unwinds. It does not forcibly interrupt user code, DNS, socket
or native Ray calls, and is not a hard deadline for the entire shutdown path.
Supervisors still need an independent termination policy. A CLI that must exit
with unresolved cleanup reports failure. Snapshot recovery after driver/job death
uses filesystem leases independently of the in-memory retry owner.

The network acceptance catches termination while retaining the exception
traceback and keeps both the embedding process and Ray alive. It requires peer
EOF, listener rebinding and no surviving network driver/writer before allowing
the caller to shut down Ray. This distinguishes explicit socket/thread cleanup
from garbage collection or process-exit cleanup. The same test is included in
the image gate, using the packaged implementation.

The runtime retirement acceptance deploys two native actors, keeps an unrelated
actor in the caller's existing Ray job, and verifies owned actor death and
snapshot removal before allowing that job to exit. A second scenario blocks a
TELL operation: cleanup must time out with its owner and snapshot retained, then
complete through the same owner after the operation is released.

Snapshots now live in registered `parrot-artifact-lease-*` control directories.
The coordinator, host and native workers hold shared OS file locks; a recovery
scan needs an exclusive lock before deleting a snapshot. Recovery runs on runtime
initialization and new snapshot allocation. Operators can also call
`parrot_protocol.ray_snapshot_lease.collect_orphaned_snapshots()`; its report lists
removed, busy, skipped and failed paths. It does not run as a background janitor.
Live leases, changed identities, invalid registration and legacy unregistered
`parrot-artifact-*` directories are preserved. Do not manually remove a busy
directory. Partial payload cleanup retains registration so a later scan can retry.
Crashes before registration or during final control-metadata removal may leave
unregistered control directories; source copying starts only after registration.

This is a cooperative same-user, same-host local-filesystem mechanism using
`flock`, not a hostile-code sandbox or a multi-node/NFS recovery guarantee.
Worker setup acquires the lease before invoking the user's importable
`worker_process_setup_hook`. It uses Ray's private
`WORKER_PROCESS_SETUP_HOOK_ENV_VAR` adapter because actor environments do not
perform the job-level hook upload. Ray upgrades must revalidate this integration.
The environment keys `PARROT_SNAPSHOT_CONTROL`, `PARROT_SNAPSHOT_USER_HOOK` and
`__RAY_WORKER_PROCESS_SETUP_HOOK_ENV_VAR` are reserved and rejected in user input;
configure a user hook through `worker_process_setup_hook` instead.

Hook precedence follows the deployment hierarchy. Without an explicit deployment
hook, hosts and native actors inherit the caller job's hook, including a callable
already exported by Ray. A deployment hook replaces that inherited hook; a native
actor's explicit hook replaces the deployment hook. An empty string disables the
user hook while retaining the lease. A failing host hook prevents the user factory
from running and is reported as a startup failure. Once Ray confirms the host is
dead, retirement can release its paths; snapshot deletion still waits for any
other worker's lease. A wrapped user exception is not host-death confirmation.

To preserve child overrides, leased workers install a process-local adapter for
Ray's actor/task runtime-environment parsers before importing user code. It
composes importable child hooks at submission time and preserves Ray's validation
and environment merging. It does not read `RuntimeContext.runtime_env` during
worker startup: that context is not ready and can crash native Ray code. Caller
job hooks use Ray's existing GCS loader; explicit child hooks must be importable
paths. The parser adapter and GCS loader are additional private Ray compatibility
seams, covered by tests on 2.51.2 and 2.59 and requiring upgrade validation.
The adapters operate in leased workers; they do not alter the embedding driver's
Ray installation or globally patch unrelated jobs. Arbitrary pre-created remote
definitions and user-created task workers still need lifecycle auditing; a live
worker lease is retained even after a task returns.

`apps/test_support/run_ray_setup_hooks.py` verifies importable and callable job
hooks, variable-only child environments, explicit actor options, decorator hooks,
disabled hooks, and failed inherited setup with subsequent retirement. Successful
host/native hooks must run exactly once, after the worker's lease is acquired.
The same acceptance runs in CI and against the packaged image.

Each deployment has a dedicated Ray driver job connected to the caller's existing
cluster, on the same node as its coordinator. The job owns a named host in a unique
namespace and its reusable task pool. Normal tasks retain Ray worker reuse and
explicit `max_calls` limits; actors keep their normal lifetime semantics. The
coordinator retires user paths and the host, closes only this driver job, then
requires an exclusive snapshot lease before deleting source. Closing the job is
not itself proof that all worker leases have ended. A timeout retains the same
owner and snapshot for a later retirement attempt.

Each deployment adds three persistent helper processes: the actual Ray driver,
a lightweight control guard, and an independent watchdog. Both monitors avoid
loading Ray. Before driver startup, the watchdog acquires a shared snapshot
lease and acknowledges readiness. Guard EOF kills the watchdog's current group,
including a native-stalled driver. Only a successfully reaped driver permits
disarming the watchdog; watchdog failure causes the guard to retire its driver.
The extra process adds startup and RSS cost: 64 deployments now mean up to 192
helper processes, excluding Ray services/workers, plus 64 log-reader threads.
Deployment density and performance are not certified by these failure tests. The guard owns the driver's Popen
handle and monitors the coordinator's private socket separately from Ray's
interpreter. Coordinator EOF or a protocol failure closes the driver's control
socket. The guard allows one second for cooperative shutdown, then kills only its
own child and waits up to three more seconds to reap it. This also works when the
Ray driver is stuck in a native call holding Python's GIL. The guard protects the
source during bootstrap; the driver and workers acquire their own leases before
using it. Snapshot deletion still requires an exclusive lease after these helper
processes exit. A caller's shorter retirement deadline retains the owner for retry.

This is not an OS hard-real-time guarantee: an uninterruptible kernel task can
outlast the reap budget. Simultaneous loss of both monitors, descendants escaping their owned process
group, or a machine failure still require wider
process containment/recovery. An RPC timeout does not cancel a queued or locked
mount operation; end-to-end admission/cancellation deadlines remain separate.
Inherited runtime environments preserve parent dependencies and merge environment
variables with deployment overrides. Internal job identity and lease variables
cannot be overridden by deployment or child environment options.

Leased workers without a dedicated job retain a conservative one-call fallback,
using Ray's native `max_calls` boundary after result/error/stream processing.
That fallback costs a worker startup per normal task. The implementation uses
private Ray job-address, worker-identity, parser, GCS hook and execution-metadata
interfaces, which require validation when upgrading Ray. These local-node tests
do not establish multi-node source distribution or project performance targets.

`apps/test_support/run_ray_task_retirement.py` verifies worker reuse, explicit
five-call limits, separate deployment/caller job identities, source access during
result serialization, errors/retries/cancellation, streams/nested references,
child hook overrides, blocked retirement, actors created through tasks, driver
exit, snapshot deletion and continued caller operation. It runs in CI and the
packaged-image gate.

`apps/test_support/run_ray_artifact_recovery.py` checks actual coordinator and
caller-driver death without Python finalizers. After coordinator death, the
caller job and an unrelated actor stay alive while the deployment driver exits
and recovery removes its snapshot. A separate live snapshot and original source
must survive. All checks precede fallback process-group cleanup; the independent
deployment driver is outside that group, and its exit is checked separately.
The harness also runs in CI and the packaged-image gate.

`apps/test_support/run_ray_job_cancellation.py` injects native GIL-holding stalls
into actual deployment-driver initialization and shutdown. It verifies owner
crash cleanup, normal retirement, and forced guard death during both native
phases while the caller job, an unrelated actor and
a separate live snapshot remain usable. Driver and guard PIDs are checked only
for exit, before fallback cleanup. CI and the packaged-image gate run this test.
The earlier harness field `job_driver` identifies the launcher's process (now
the guard); this new gate checks the actual driver and guard separately.

Ray 2.51/2.59 replaces Python's SIGTERM handler during initialization, and its
native logger can reset SIGTERM to the OS default during shutdown. When the
main-thread caller installed an explicit callback or SIG_IGN, our initialization
adapter restores it and temporarily sets Ray's private driver option
`RAY_DISABLE_FAILURE_SIGNAL_HANDLER` while constructing the driver. The option is
restored even on failure, without changing the environment of Ray subprocesses.
This trades the driver's Ray-native fatal-signal stack dump for caller-owned
signal handling; worker diagnostics retain their original setting. Default
unconfigured callers and background initialization keep Ray's native policy.
Ray 2.51 lacks this driver option (its native handler is unconditional), so it
retains compatibility with an explicit warning and cannot guarantee cleanup under
repeated TERM. Use the locked runtime for that guarantee. Both paths have local
contract coverage; the native seam must be revalidated on Ray upgrades.
Concurrent unrelated `ray.init()` calls in one
process are unsupported. The TERM acceptance exercises a live Wire exchange,
repeated TERM, registration retry, explicit shutdown receipts and connection EOF;
emergency cleanup is recorded separately and cannot make a failed exit pass.

Ray 2.59 enables authentication for its internal local cluster by default; keep
that enabled and configure its token source when operational isolation requires
it. This is distinct from Parrot's own transport authentication. This lock does
not pin base-image digests, OS packages, arbitrary application `runtime_env`
requirements or artifacts from external URLs; it is not a claim of reproducible
whole-image bytes or of successful Linux execution without the container gate.

### Websearch 项目隔离与四镜像验收

Websearch Compose 由 Docker 分配容器地址，网关使用项目内 DNS
`app:19870` 注册。通过 `COMPOSE_PROJECT_NAME` 选择独立项目；后续
`logs/ps/stats/search/down` 必须使用相同项目名。并行部署可设置
`WS_WEB_PORT=0`，由 Docker 分配宿主回环端口；默认仍为 8188。

```sh
COMPOSE_PROJECT_NAME=search-a WS_WEB_PORT=0 WS_PLATFORM=linux/amd64 \
  apps/websearch/deploy/compose.sh up --port 9090 --data '/data/tenant a'
COMPOSE_PROJECT_NAME=search-a apps/websearch/deploy/compose.sh stats
COMPOSE_PROJECT_NAME=search-a apps/websearch/deploy/compose.sh down -v
```

`up` 的同名选项替换 Compose 默认值；用户自己重复提供的选项仍由实际
Rust CLI 拒绝。容器 `--port` 必须为 1..65535；随机端口应设置宿主
`WS_WEB_PORT=0`。参数以 argv 传递，空格和美元符号保持字面值。
启动前以目标镜像执行 `--validate-only`；就绪要求应用启动标记和有效
`/stats` 同时成立。`stats/search` 的非成功 HTTP 状态使命令失败。

原生 Linux 四镜像验收需要 Docker Compose、对应架构的 release musl
Websearch 二进制、JDK 21 构建的框架和业务 JAR，以及
`prep-jvm-libs.sh` 生成的宿主用户可读归档。CI 的 `websearch-images`
作业列出了完整准备步骤。运行：

```sh
python3 scripts/verify_websearch_images.py --report-dir /tmp/websearch-images-report
```

报告目录必须尚不存在。默认构建四个生产 Dockerfile，并检查两个独立
项目的精确文档、检索 URL、分数、HTTP 请求计数、交叉检索隔离、另一
项目销毁后的存活、`--keep` 追加，以及真实 JVM 重启后的持久化回放。
语料位于自有项目 bridge 网络，以显式 `--crawl-origin` 限定抓取范围，
业务不依赖公网；网络本身没有禁止出站。使用普通 bridge 才能验收实际
宿主端口发布。退出时清理本次资源；清理
失败使验收失败，报告和日志保留。已有镜像可显式用
`--skip-build --image-prefix PREFIX`，四标签为 `PREFIX-app/erl/ray/jvm`；
这种模式不证明镜像构建过程，也不会删除调用者镜像。

此入口验证生产 Compose 模型、参数渲染和镜像运行；不执行 `compose.sh`
的宿主预构建分支，不替代多物理机、跨架构、NFS/WAN 或性能验收。

Websearch 应用配置 `init: true` 和 75 秒停止宽限。应用捕获 INT/TERM，
停止派发新抓取，完成已派发批次的分词、索引和确认落盘，关闭 HTTP 与
远程连接后退出 0；整个信号处理期限为 60 秒，超时或任何确认失败退出非零。
第二次 TERM 不跳过同一次收尾。直接运行仍应使用 `docker run --init`。

使用 `compose.sh down` 先停止应用，再销毁网关；原始 `docker compose down`
按依赖逆序停止网关，不能用它保证应用的最终落盘。宿主启动器同样先等待
应用退出，再回收网关组，避免清理先切断落盘所需的连接。

验收另含阻塞在 HTTP 响应中的页面：两次 TERM 后放行响应，随后真正重启
JVM，要求新增文档及词项完整回放；应用必须退出 0 并报告 shutdown complete。
这覆盖当前批次的正常收尾，不提供 SIGKILL、机器掉电或待爬队列事务恢复保证。
