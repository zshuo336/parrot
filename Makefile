# ============================================================================
#  Parrot polyglot monorepo — unified build & test entrypoint
#
#  一个命令编译整个项目，一个命令跑全部测试；模式即 target，通过 make 变量开洞。
#
#  ── 编译 ──────────────────────────────────────────────────────────────────
#    make build                # Rust debug（最快，日常开发默认）
#    make build MODE=release   # Rust release（跑阈值/压测前必须先编这个）
#    make build-all            # 全语言：Rust + TS + C++ + JVM + Python 检查
#    make build-all MODE=release
#
#  ── 测试 ──────────────────────────────────────────────────────────────────
#    make test                 # 快速模式（默认）：Rust debug 单测+集成
#    make test MODE=full       # 全模式：debug 全量 + release 阈值 + 压测 + lint
#    make test MODE=stress     # release 测试与压力实验（A5 stretch 单独执行）
#    make test MODE=polyglot   # 只跑多语言：TS + Python + JVM + Erlang + C++
#    make test MODE=full-polyglot  # 全模式 + 多语言全家桶
#
#  ── 单项快捷 ──────────────────────────────────────────────────────────────
#    make lint                 # fmt --check + clippy -D warnings
#    make bench                # remote-bench --gate p1（性能门禁）
#    make matrix               # Rust↔JVM 一致性矩阵（需先 make build-jvm）
#    make check-a5-perf        # A5 近期 >50000/s 门槛
#    make check-a5-stretch     # A5 原 >300000/s 长期目标（失败仍返回非零）
#    make ts / py / jvm / erl / cpp    # 单语言测试快捷方式
#    make clean                # 清理 cargo target/ + TS dist/ + C++ bin/
#
#  ── 作用域裁剪（对 test/build 变体生效）────────────────────────────────────
#    make test CRATE=parrot-remote        # 只跑一个 crate
#    make test MODE=stress CRATE=parrot   # 只跑 parrot 的压测
#    make test FEATURE=remote             # 打开某个 feature
#    make test RELEASE=1                  # 快速模式也用 release 二进制
#
#  其他变量：V=1 透传 cargo verbose；N=4 限制测试线程数；EXTRA="--nocapture"
# ============================================================================

# ── 可配置项（命令行覆盖：make test MODE=stress）──────────────────────────
# 注意：Make 行内注释会把前置空格并入变量值，故注释一律独立成行。
# MODE: fast | full | stress | polyglot | full-polyglot
MODE     ?= fast
# CRATE: 限定 crate（空 = 全 workspace）
CRATE    ?=
# FEATURE: 附加 cargo feature（空 = 各 crate 默认）
FEATURE  ?=
# RELEASE: 1 = 快速模式也编/跑 release
RELEASE  ?= 0
# V: 1 = cargo -v
V        ?= 0
# N: 测试线程数（空 = cargo 默认）
N        ?=
# EXTRA: 附加 -- 之后的参数，如 --nocapture
EXTRA    ?=

# ── 内部常量 ──────────────────────────────────────────────────────────────
CARGO    := cargo
TS_DIR   := interop/typescript-lite
PY_DIR   := interop/python
JVM_DIR  := interop/jvm
CPP_DIR  := interop/cpp-lite
CPP_BUILD_DIR := $(abspath target/cpp-lite)
ERL_DIR  := interop/erlang

# GNU Make 3.81 (the macOS system make) ignores .SHELLFLAGS. Put the
# required failure policy in SHELL so compound recipes and pipelines cannot
# report success after a failed build/test on either old or modern GNU Make.
SHELL := /bin/bash -eu -o pipefail
.SHELLFLAGS := -c

.DEFAULT_GOAL := help
.PHONY: help build build-all build-ts build-cpp build-jvm build-python \
        test test-fast test-full test-stress test-polyglot test-full-polyglot test-lab \
        lint fmt clippy bench matrix clean distclean \
        ts py jvm erl cpp check-ts check-py check-jvm check-erl check-cpp test-jvm-gateway \
        build-release-bins twin-app build-probe probe obs-up obs-down

# ── 帮助 ──────────────────────────────────────────────────────────────────
help: ## 显示本帮助
	@echo "Parrot 统一构建/测试入口（详细模式说明见 Makefile 头部注释）"
	@echo ""
	@echo "  make build [MODE=release]      编译 Rust（默认 debug）"
	@echo "  make build-all                 全语言编译（Rust+TS+C++/JVM+Py）"
	@echo "  make test [MODE=...]           测试（默认 fast；详见下方模式表）"
	@echo "  make lint / bench / matrix     质量门禁 / 性能门禁 / 一致性矩阵"
	@echo "  make ts py jvm erl cpp         单语言测试快捷方式"
	@echo "  make check-a5-perf / check-a5-stretch  近期 A5 门槛 / 原长期目标"
	@echo "  make clean / distclean         清理"
	@echo ""
	@echo "测试模式："
	@echo "  fast          debug workspace 全量（默认）"
	@echo "  full          debug 全量 + release 阈值 + ignored 压测 + lint"
	@echo "  stress        release 测试与压力实验（A5 stretch 另行执行）"
	@echo "  polyglot      TS + Python + JVM + Erlang + C++（缺工具链失败）"
	@echo "  full-polyglot full + polyglot"
	@echo ""
	@echo "裁剪变量：CRATE= FEATURE= RELEASE=1 V=1 N=4 EXTRA=\"--nocapture\""
	@echo "示例："
	@echo "  make test MODE=stress CRATE=parrot"
	@echo "  make build-all && make test MODE=full-polyglot"

# ══════════════════════════════════════════════════════════════════════════
# 编译
# ══════════════════════════════════════════════════════════════════════════

# cargo 参数拼接（空值自然消失）
CARGO_SCOPE := $(if $(CRATE),-p $(CRATE),)
CARGO_FEAT  := $(if $(FEATURE),--features $(FEATURE),)
CARGO_REL   := $(if $(or $(filter 1,$(RELEASE)),$(filter release,$(MODE))),--release,)
CARGO_VERB  := $(if $(filter $(V),1),-v,)
TEST_THREADS := $(if $(N),--test-threads=$(N),)
# Performance experiments run serially by default. N remains an explicit override.
PERF_TEST_THREADS := $(if $(strip $(N)),$(TEST_THREADS),--test-threads=1)
# The original A5 target remains independently executable via check-a5-stretch.
# Current acceptance uses the owner-authorized >50000/s target.
CURRENT_PERF_TEST_ARGS := --skip a5_herd_spawn_throughput_thread_stretch
# Workspace tests include the crawler component integration target.
CRAWLER_PREPARE := $(if $(strip $(CRATE)),$(filter crawler-lab,$(CRATE)),1)

build: ## 编译 Rust（MODE=release 或 RELEASE=1 时编 release）
	$(CARGO) build $(CARGO_SCOPE) $(CARGO_FEAT) $(CARGO_REL) $(CARGO_VERB)

build-all: build build-ts build-cpp build-jvm build-python ## 全语言编译

build-ts: ## 编译 TypeScript lite（tsc + esbuild bundle）
	cd $(TS_DIR) && npm run build && npm run bundle

build-cpp: ## 编译 C++ lite 测试（clang++ -O1，产物在 target/cpp-lite/）
	mkdir -p "$(CPP_BUILD_DIR)"
	cd $(CPP_DIR) && \
	clang++ -std=c++17 -O1 -Wall -Wextra parrot_lite.cpp test_vectors.cpp -o "$(CPP_BUILD_DIR)/test_vectors" && \
	clang++ -std=c++17 -O1 -Wall -Wextra parrot_lite.cpp test_interop.cpp -o "$(CPP_BUILD_DIR)/test_interop"

build-jvm: ## 打包 JVM 网关 jar（mvn -q package）
	cd $(JVM_DIR) && mvn -q -s settings-central.xml package -DskipTests
	cd $(JVM_DIR) && mvn -q -s settings-central.xml org.apache.maven.plugins:maven-dependency-plugin:3.8.1:build-classpath -Dmdep.outputFile=target/cp.txt

.PHONY: test-app-host test-app-startup-shutdown test-app-polyglot build-crawler-components build-erl test-crawler-app check-crawler-launcher
.PHONY: test-container
test-container: ## 构建Linux镜像，验证非root健康检查、真实组网/拒绝旁路、SIGTERM退出
	python3 scripts/verify_container.py

test-app-startup-shutdown: build-jvm ## Real CLI TERM/INT during first/partial startup and independent retirement oracle
	CARGO_BUILD_JOBS=2 python3 -O crates/parrot-app/tests/run_cli_shutdown.py

test-app-host: build-jvm ## 应用配置、真实 JVM 组件及 CLI 业务链路（依赖缺失即失败）
	$(CARGO) test --locked -p parrot-app --features host --lib --test test_assemble_helper --test application_admission --test application_wiring --test application_lifecycle --test application_ref_disposal --test host_ref_disposal --test chaos_scenarios -- --include-ignored

build-erl: ## 构建通用 Erlang 网关及组件宿主
	cd $(ERL_DIR) && erlc parrot_gw.erl parrot_trace.erl parrot_component_runtime.erl parrot_beam_snapshot.erl parrot_beam_guest.erl parrot_beam_host.erl

.PHONY: test-erl-causal test-erl-causal-rust
test-erl-causal: ## Actual SDK, TCP, serial worker and isolated BEAM tracing contracts
	python3 -B $(ERL_DIR)/run_causal.py

test-erl-causal-rust: ## Production Rust sender to real Erlang native/guest SDK handlers in both profiles
	CARGO_BUILD_JOBS=2 python3 -B $(ERL_DIR)/run_causal.py --rust

build-crawler-components: build-jvm build-erl ## 构建 crawler 的真实组件制品
	python3 -c 'import ray'
	cd apps/crawler-lab/erlang && erlc frontier.erl
	cd apps/crawler-lab/jvm && mvn -q -s ../../../interop/jvm/settings-central.xml package -DskipTests

test-app-polyglot: build-crawler-components test-python-apps test-jvm-apps ## 四引擎真实应用业务与失败回滚
	python3 crates/parrot-app/tests/test_gateway_port_probe.py
	python3 crates/parrot-app/tests/run_gateway_ownership.py --cargo "$(CARGO)"

build-crawler-business: ## 构建实际 Dylib/Wasm 业务；Wasm 需要 wasm-tools 与对应 Rust target
	CARGO="$(CARGO)" python3 scripts/crawler_business_artifacts.py build

test-crawler-business: build-crawler-business build-crawler-components build-ts ## 三形态业务、固定 golden 与真实网关退休
	$(CARGO) test --locked -p crawler-lab-core -p crawler-lab-dylib -p crawler-lab -- --include-ignored
	python3 apps/crawler-lab/tests/run_component_business.py

test-crawler-manifest: build-crawler-business build-crawler-components build-ts ## 标准装配、三形态业务、TS查询与进程回收
	$(CARGO) test --locked -p crawler-lab -- --include-ignored
	python3 apps/crawler-lab/tests/run_manifest_integration.py

.PHONY: build-crawler-business test-crawler-business test-crawler-manifest

test-crawler-app: build-crawler-components build-ts ## 实际应用 CLI：正常业务及损坏回复不能假成功
	$(CARGO) build --locked -p crawler-lab
	python3 apps/crawler-lab/tests/run_integration.py

.PHONY: test-ts-jvm
test-ts-jvm: build-jvm build-ts ## TS bin 编码栈与真实 JVM 网关互通（缺依赖即失败）
	cd $(TS_DIR) && npm run test:jvm

test-jvm-gateway: build-jvm ## Rust/JVM 网关真实 TCP 管理链路（依赖缺失即失败）
	$(CARGO) test --locked -p parrot-node --test jvm_gateway_admin jvm_gateway_admin_v2_full_chain -- --ignored --exact --nocapture

.PHONY: test-jvm-causal
test-jvm-causal: build-jvm ## Production Rust sender, JVM handler spans, panic recovery and process retirement
	PARROT_JVM_CP="$(abspath $(JVM_DIR)/target/test-classes):$(abspath $(JVM_DIR)/target/classes):$$(cat $(JVM_DIR)/target/cp.txt)" \
	$(CARGO) test --locked -p parrot-remote --test jvm_causal_context -- --ignored --exact rust_sender_observes_actual_jvm_handler_children_and_retirement --nocapture

build-python: ## Python 语法预热（compileall；依赖见 pyproject.toml）
	cd $(PY_DIR) && python3 -m compileall -q parrot_protocol tests

# ── 观测五件套（docs/OBSERVABILITY.md）──────────────────────────────────────
build-probe: ## 编译 parrot-obs 观测 CLI+Web（tools/parrot-obs）
	$(CARGO) build --locked -p parrot-obs $(CARGO_REL)

obs-up: ## 一键起三网关（守护化——erl/jvm/ray；就绪后打印 NODES 行）
	bash tools/parrot-obs/gw.sh up

obs-down: ## 停三网关
	bash tools/parrot-obs/gw.sh down

probe: build-probe ## 观测探针速用（NODES='erl=… ray=… jvm=…'；CMD=metrics|status|…）
	@test -n "$(CMD)" || (echo '用法：make probe CMD=status|metrics|ping|ask|load|watch|trace|web NODES="erl=127.0.0.1:19871 …" [ARGS="--seconds 10"]  （或先 make obs-up 一键起网关）'; exit 2)
	$(CARGO) run --locked -q -p parrot-obs $(CARGO_REL) -- $(CMD) "$(NODES)" $(ARGS)

# ══════════════════════════════════════════════════════════════════════════
# 测试（MODE 分发 → 具体 recipe）
# ══════════════════════════════════════════════════════════════════════════

test: ## 测试入口（按 MODE 分发：fast|full|stress|polyglot|full-polyglot）
ifeq ($(MODE),fast)
	@$(MAKE) --no-print-directory test-fast
else ifeq ($(MODE),full)
	@$(MAKE) --no-print-directory test-full
else ifeq ($(MODE),stress)
	@$(MAKE) --no-print-directory test-stress
else ifeq ($(MODE),polyglot)
	@$(MAKE) --no-print-directory test-polyglot
else ifeq ($(MODE),full-polyglot)
	@$(MAKE) --no-print-directory test-full-polyglot
else
	@echo "错误：未知 MODE=$(MODE)（可选 fast|full|stress|polyglot|full-polyglot）" >&2; exit 1
endif

# ---- 快速模式：debug 单测+集成（日常默认）---------------------------------
test-fast: build
ifneq ($(CRAWLER_PREPARE),)
	@$(MAKE) --no-print-directory build-crawler-business
endif
	$(CARGO) test $(CARGO_SCOPE) $(CARGO_FEAT) $(CARGO_REL) $(CARGO_VERB) -- $(TEST_THREADS) $(EXTRA)
	@echo "✓ 快速模式完成（当前 scope/profile 测试）"

# ---- 全模式：debug 全量 + release 阈值 + ignored 压测 + lint ---------------
test-full: test-fast lint
	@echo ""
	@echo "==> release 当前阈值测试（A5 stretch 另行执行）"
	@$(MAKE) --no-print-directory check-thread-perf
	@echo ""
	@echo "==> release 压测（包含 ignored 用例，A5 stretch 另行执行）"
	$(CARGO) test --release $(CARGO_SCOPE) $(CARGO_FEAT) --lib --bins --tests -- --include-ignored $(CURRENT_PERF_TEST_ARGS) $(PERF_TEST_THREADS)
	@echo "✓ 全模式完成（debug 全量 + release 当前阈值 + 压测 + lint；A5 stretch 另行执行；doc-tests 的 ignore 示例不参与压测——与 test-stress 同语义）"

# ---- 压测模式：release 常规用例与压力/阈值实验，A5 stretch 独立--------------------------
test-stress:
ifneq ($(CRAWLER_PREPARE),)
	@$(MAKE) --no-print-directory build-crawler-business
endif
	$(CARGO) test --release $(CARGO_SCOPE) $(CARGO_FEAT) --lib --bins --tests -- --include-ignored $(CURRENT_PERF_TEST_ARGS) $(PERF_TEST_THREADS) $(EXTRA)
	@echo "✓ 压测模式完成（release --include-ignored，A5 stretch 另行执行；doc-tests 由 fast/full 覆盖——ignore 示例不参与压测语义）"

# ---- 多语言模式：TS + Python + JVM + Erlang + C++（缺工具链失败）----------
test-polyglot: check-ts check-py check-jvm check-erl check-cpp test-jvm-gateway test-app-polyglot
	@echo "✓ 多语言模式完成（TS/Python/JVM/Erlang/C++）"

# ---- 五运行时全链集成场景（crawler-lab：大规模爬虫+索引+Web 检索）----------
# 验收入口：依赖缺失即失败，不把未执行当作通过。
LAB_SCRIPT := apps/crawler-lab/deploy/run-lab.sh
check-crawler-launcher: ## 真实子进程故障、信号、后代清理与并发隔离契约
	python3 -m unittest discover -s apps/crawler-lab/tests -p 'test_*.py' -v

test-lab: build-crawler-components
	@if [ -x "$(LAB_SCRIPT)" ] && command -v erl >/dev/null 2>&1 \
	   && command -v java >/dev/null 2>&1 && command -v node >/dev/null 2>&1 \
	   && python3 -c "import ray" >/dev/null 2>&1; then \
		$(CARGO) build --locked -p crawler-lab $(CARGO_Q) && \
		./$(LAB_SCRIPT) --pages 200 --depth 2 --fanout 3 --batch 32 && \
		echo "✓ crawler 场景通过（运行时范围见 launcher 的 mode/ts 结果）" ; \
	else \
		echo "ERROR: lab requires $(LAB_SCRIPT), erl, java, node and python3/ray" >&2 ; exit 1 ; \
	fi

test-full-polyglot: test-full test-polyglot
	@echo "✓ 全模式 + 多语言完成"

test-everything: test-full-polyglot test-lab
	@echo "✓ 全量 + 五运行时场景完成"

# ---- 多语言单项快捷方式 ----------------------------------------------------
ts: build-ts
	cd $(TS_DIR) && npm test

py: build-python
	cd $(PY_DIR) && python3 -m pytest tests/ -q

jvm: build-jvm
	cd $(JVM_DIR) && mvn -q -s settings-central.xml test

ERL_RUN := erl -noshell -pa . -eval '{ok,parrot_test_vectors}=c:c(parrot_test_vectors), {ok,parrot_trace}=c:c(parrot_trace), {ok,parrot_beam_snapshot}=c:c(parrot_beam_snapshot), {ok,parrot_beam_guest}=c:c(parrot_beam_guest), {ok,parrot_beam_host}=c:c(parrot_beam_host), {ok,parrot_component_runtime}=c:c(parrot_component_runtime), {ok,parrot_gw}=c:c(parrot_gw), {ok,test_wire}=c:c(test_wire), {ok,test_admin_v2}=c:c(test_admin_v2), {ok,test_measured}=c:c(test_measured), ok=test_wire:run(), ok=test_admin_v2:run(), ok=test_measured:run(), {ok,test_component_routes}=c:c(test_component_routes), ok=test_component_routes:run(), {ok,test_endpoints}=c:c(test_endpoints), ok=test_endpoints:run(), halt().'

ERL_VECTOR_RUN := python3 scripts/tests/test_protocol_vectors.py

ERL_WORKER_RUN := erl -noshell -pa . -eval '[{ok,M}=c:c(M) || M <- [parrot_trace,parrot_beam_snapshot,parrot_beam_guest,parrot_beam_host,parrot_component_runtime,parrot_gw,test_beam_host_fixture,test_terminal_workers]], ok=test_terminal_workers:run(), halt().'

ERL_OWNER_RUN := erl -noshell -pa . -eval '[{ok,M}=c:c(M) || M <- [parrot_trace,parrot_beam_snapshot,parrot_beam_guest,parrot_beam_host,parrot_component_runtime,parrot_gw,test_network_owner]], ok=test_network_owner:run(), halt().'

ERL_RECOVERY_RUN := erl -noshell -pa . -eval '[{ok,M}=c:c(M) || M <- [parrot_trace,parrot_beam_snapshot,parrot_beam_guest,parrot_beam_host,parrot_component_runtime,parrot_gw,test_network_recovery]], ok=test_network_recovery:run(), halt().'

ERL_STARTUP_RUN := erl -noshell -pa . -eval '[{ok,M}=c:c(M) || M <- [parrot_trace,parrot_beam_snapshot,parrot_beam_guest,parrot_beam_host,parrot_component_runtime,parrot_gw,test_startup_admission]], ok=test_startup_admission:run(), halt().'

ERL_CONCURRENT_RUN := erl -noshell -pa . -eval '[{ok,M}=c:c(M) || M <- [parrot_trace,parrot_beam_snapshot,parrot_beam_guest,parrot_beam_host,parrot_component_runtime,parrot_gw,test_concurrent_startup]], ok=test_concurrent_startup:run(), halt().'

ERL_LEASE_RUN := erl -noshell -pa . -eval '[{ok,M}=c:c(M) || M <- [parrot_trace,parrot_beam_snapshot,parrot_beam_guest,parrot_beam_host,parrot_component_runtime,parrot_gw,test_lease_activation]], ok=test_lease_activation:run(), halt().'

.PHONY: check-erl-compile
check-erl-compile: ## Strictly compile every gateway/test module into a disposable directory
	@beam_dir="$$(mktemp -d)"; \
	trap 'rm -rf -- "$$beam_dir"' EXIT; \
	erlc -Werror -o "$$beam_dir" $(ERL_DIR)/*.erl

erl: check-erl-compile
	$(ERL_VECTOR_RUN)
	cd $(ERL_DIR) && $(ERL_RUN)
	cd $(ERL_DIR) && $(ERL_WORKER_RUN)
	cd $(ERL_DIR) && $(ERL_OWNER_RUN)
	cd $(ERL_DIR) && $(ERL_RECOVERY_RUN)
	cd $(ERL_DIR) && $(ERL_STARTUP_RUN)
	cd $(ERL_DIR) && $(ERL_CONCURRENT_RUN)
	cd $(ERL_DIR) && $(ERL_LEASE_RUN)
	python3 $(ERL_DIR)/test_connection.py
	python3 $(ERL_DIR)/test_negotiated_limits.py
	python3 $(ERL_DIR)/test_heartbeat_envelopes.py
	python3 $(ERL_DIR)/test_ipv6.py
	python3 $(ERL_DIR)/test_terminal_error.py
	python3 $(ERL_DIR)/test_direct_terminal_error.py

cpp: build-cpp
	"$(CPP_BUILD_DIR)/test_vectors" && "$(CPP_BUILD_DIR)/test_interop"
	python3 $(CPP_DIR)/test_interop_cli.py --binary "$(CPP_BUILD_DIR)/test_interop"
	python3 $(CPP_DIR)/test_transport.py

# check-*：polyglot 聚合用；工具链缺失或任何测试失败均失败
check-ts:
	@if command -v npm >/dev/null 2>&1; then \
		echo "==> [npm] TypeScript lite 测试"; \
		(cd $(TS_DIR) && npm test) && \
		$(CARGO) test --locked -p parrot --test test_lite_interop; \
	else \
		echo "==> [npm] ERROR：未安装 npm" >&2; exit 1; \
	fi

check-py:
	@if command -v python3 >/dev/null 2>&1; then \
		echo "==> [python3] pytest 协议向量测试"; \
		(cd $(PY_DIR) && python3 -m pytest tests/ -q); \
	else \
		echo "==> [python3] ERROR：未安装 python3" >&2; exit 1; \
	fi

check-jvm:
	@if command -v mvn >/dev/null 2>&1; then \
		echo "==> [mvn] JVM 协议测试"; \
		(cd $(JVM_DIR) && mvn -q -s settings-central.xml test); \
	else \
		echo "==> [mvn] ERROR：未安装 mvn" >&2; exit 1; \
	fi

check-erl: check-erl-compile
	@if command -v erl >/dev/null 2>&1; then \
		echo "==> [erl] Erlang 网关 wire 测试"; \
		$(ERL_VECTOR_RUN) && (cd $(ERL_DIR) && $(ERL_RUN)) && (cd $(ERL_DIR) && $(ERL_WORKER_RUN)) && (cd $(ERL_DIR) && $(ERL_OWNER_RUN)) && (cd $(ERL_DIR) && $(ERL_RECOVERY_RUN)) && (cd $(ERL_DIR) && $(ERL_STARTUP_RUN)) && (cd $(ERL_DIR) && $(ERL_CONCURRENT_RUN)) && (cd $(ERL_DIR) && $(ERL_LEASE_RUN)) && python3 $(ERL_DIR)/test_connection.py && python3 $(ERL_DIR)/test_negotiated_limits.py && python3 $(ERL_DIR)/test_heartbeat_envelopes.py && python3 $(ERL_DIR)/test_ipv6.py && python3 $(ERL_DIR)/test_terminal_error.py && python3 $(ERL_DIR)/test_direct_terminal_error.py && \
		(cd $(ERL_DIR) && erl -noshell -eval '{ok,parrot_gw}=c:c(parrot_gw), {ok,test_frontier}=c:c(test_frontier), ok=test_frontier:run(crawler), {ok,test_frontier_bfs}=c:c(test_frontier_bfs), ok=test_frontier_bfs:run(crawler), {ok,test_frontier_instances}=c:c(test_frontier_instances), ok=test_frontier_instances:run(crawler), halt().') && \
		(cd $(ERL_DIR) && erl -noshell -eval '{ok,parrot_gw}=c:c(parrot_gw), {ok,test_frontier}=c:c(test_frontier), ok=test_frontier:run(websearch), {ok,test_frontier_bfs}=c:c(test_frontier_bfs), ok=test_frontier_bfs:run(websearch), {ok,test_frontier_instances}=c:c(test_frontier_instances), ok=test_frontier_instances:run(websearch), halt().') && \
		(cd $(ERL_DIR) && erl -noshell -eval '{ok,test_instance_fixture}=c:c(test_instance_fixture), {ok,test_instance_lifecycle}=c:c(test_instance_lifecycle), ok=test_instance_lifecycle:run(), halt().') && \
		(cd $(ERL_DIR) && erl -noshell -eval '{ok,test_beam_artifacts}=c:c(test_beam_artifacts), ok=test_beam_artifacts:run(), halt().') && \
		(cd $(ERL_DIR) && erl -noshell -eval '{ok,test_beam_hosts}=c:c(test_beam_hosts), ok=test_beam_hosts:run(), halt().') && \
		(cd $(ERL_DIR) && erl -noshell -eval 'ok=test_frontier_instances:run_both(), halt().') && \
		python3 $(ERL_DIR)/test_beam_bundle.py && python3 $(ERL_DIR)/test_frontier_reply_budget.py && \
		python3 $(ERL_DIR)/test_frontier_capacity.py && \
		(cd $(ERL_DIR) && erl -noshell -eval '{ok,test_frontier_capacity_storage}=c:c(test_frontier_capacity_storage), ok=test_frontier_capacity_storage:run(crawler), ok=test_frontier_capacity_storage:run(websearch), halt().'); \
	else \
		echo "==> [erl] ERROR：未安装 Erlang" >&2; exit 1; \
	fi

.PHONY: check-tokio-upstream
check-tokio-upstream: ## Run pinned Tokio upstream suites and minimal sync/parking_lot contracts
	python3 scripts/check_tokio_upstream.py

.PHONY: check-cpp-sanitizers
check-cpp-sanitizers: ## Run all C++ contracts with ASan/UBSan and real fault controls
	python3 scripts/check_cpp_sanitizers.py

check-cpp:
	@if command -v clang++ >/dev/null 2>&1; then \
		echo "==> [clang++] C++ lite wire/interop 测试"; \
		$(MAKE) --no-print-directory cpp; \
	else \
		echo "==> [clang++] ERROR：未安装 clang++" >&2; exit 1; \
	fi

# ══════════════════════════════════════════════════════════════════════════
# 质量门禁 / 工具
# ══════════════════════════════════════════════════════════════════════════

.PHONY: check-erlang-bench
check-erlang-bench: ## Erlang 基准失败/计算/生命周期契约及20个小规模场景
	python3 bench/erlang-bench/tests/test_launcher.py
	bench/erlang-bench/run.sh --check

.PHONY: check-make check-business-artifacts check-image-artifacts
check-image-artifacts: ## JVM dependency archives and Docker COPY context contracts (Python + Go)
	python3 scripts/tests/test_jvm_image_artifacts.py -v
	cd scripts/tests/docker_context && go test -mod=readonly -count=1 -v ./...

check-business-artifacts: ## 校验业务构建回执、输入变化和失败重建契约
	python3 scripts/tests/test_crawler_business_artifacts.py -v

check-make: ## 验证真实 recipe／递归 make 的失败传播（无需外部网关）
	python3 scripts/tests/test_make_contracts.py -v
	python3 scripts/tests/test_cpp_make_contracts.py -v

lint: check-make check-business-artifacts fmt clippy ## recipe failure propagation + fmt + strict clippy

fmt:
	$(CARGO) fmt --all --check

clippy:
	$(CARGO) clippy $(CARGO_SCOPE) --all-targets $(CARGO_FEAT) $(CARGO_VERB) -- -D warnings

build-release-bins:
	$(CARGO) build --release -p parrot-remote --bins

bench: build-release-bins ## remote-bench 性能门禁（ask p50<150µs / tell<60µs）
	$(CARGO) run --release -p parrot-remote --bin remote-bench -- --gate p1

matrix: build-jvm ## Rust↔JVM 一致性矩阵（RTT p50<300µs 门禁）
	$(CARGO) run --locked --release -p parrot-remote --bin interop-matrix

clean:
	$(CARGO) clean
	rm -rf $(TS_DIR)/dist $(TS_DIR)/build
	rm -rf "$(CPP_BUILD_DIR)"

distclean: clean
	rm -rf $(TS_DIR)/node_modules


# ---- F4（DEV_09）：App 孪生门禁——Manifest 输入 ---------------------------
# 默认输入：crawler-lab 迁移后的 crawler.app.toml（M1+M2 联合验收）
MANIFEST ?= apps/crawler-lab/crawler.app.toml
twin-app:
	$(CARGO) run -p federation-lab -- twin-app --manifest $(MANIFEST)

.PHONY: test-python-apps
test-python-apps:
	PYTHONPATH=interop/python python3 -m pytest -q interop/python/tests/test_ray_startup.py
	python3 apps/test_support/test_python_components.py
	python3 apps/test_support/run_python_components.py
	python3 apps/test_support/run_ray_lifecycle.py
	python3 apps/test_support/run_ray_artifacts.py
	python3 apps/test_support/run_ray_environments.py

.PHONY: test-jvm-apps
test-jvm-apps: ## JVM 应用真实 actor 请求边界、失败恢复与排名存储契约
	bash apps/test_support/test_jvm_requests.sh
	bash apps/websearch/jvm/test.sh

.PHONY: check-cpp-perf
check-cpp-perf: ## 独立release性能门禁（20k ASK/s、decoder 300ms；需记录运行环境）
	PL_CPP_MIN_QPS=20000 PL_CPP_MAX_DECODE_MS=300 $(CARGO) test --locked --release -p parrot --test test_cpp_interop cpp_interop_ask -- --exact --nocapture

.PHONY: check-akka-bench
check-akka-bench: ## Akka Typed 正确性、失败传播及20个小规模场景
	python3 bench/akka-bench/tests/test_launcher.py
	bench/akka-bench/run.sh --check

.PHONY: check-ray-bench
check-ray-bench: ## Ray 结果/生命周期/双集群隔离契约及20个小规模场景
	"$${PYTHON:-python3}" bench/ray-bench/tests/test_launcher.py
	"$${PYTHON:-python3}" bench/ray-bench/tests/test_ownership.py
	bench/ray-bench/run.sh --check --cpus 4 --object-store-bytes 134217728

# A5 current acceptance and original stretch are separate, real failing gates.
.PHONY: check-thread-perf check-a5-perf check-a5-stretch
check-thread-perf: ## Thread acceptance thresholds; original A5 stretch is explicit
	@$(CARGO) test --locked --release -p parrot --test test_thread_advantages -- --include-ignored $(CURRENT_PERF_TEST_ARGS) $(PERF_TEST_THREADS) --list | awk '$$0 == "a5_herd_spawn_throughput_thread: test" { a++ } $$0 == "a1_dedicated_thread_isolation: test" { b++ } END { if (a != 1 || b != 1) { print "ERROR: expected exactly one current A5 and one A1 isolation test"; exit 1 } }'
	$(CARGO) test --locked --release -p parrot --test test_thread_advantages -- --include-ignored $(CURRENT_PERF_TEST_ARGS) $(PERF_TEST_THREADS) --nocapture

check-a5-perf: ## Current A5 reference-host gate: >50000 registrations/s
	@$(CARGO) test --locked --release -p parrot --test test_thread_advantages a5_herd_spawn_throughput_thread -- --ignored --exact --list | awk '$$0 == "a5_herd_spawn_throughput_thread: test" { n++ } END { if (n != 1) { print "ERROR: expected exactly one a5_herd_spawn_throughput_thread test"; exit 1 } }'
	$(CARGO) test --locked --release -p parrot --test test_thread_advantages a5_herd_spawn_throughput_thread -- --ignored --exact --nocapture

check-a5-stretch: ## Original A5 target: >300000 registrations/s; failures are preserved
	@$(CARGO) test --locked --release -p parrot --test test_thread_advantages a5_herd_spawn_throughput_thread_stretch -- --ignored --exact --list | awk '$$0 == "a5_herd_spawn_throughput_thread_stretch: test" { n++ } END { if (n != 1) { print "ERROR: expected exactly one a5_herd_spawn_throughput_thread_stretch test"; exit 1 } }'
	$(CARGO) test --locked --release -p parrot --test test_thread_advantages a5_herd_spawn_throughput_thread_stretch -- --ignored --exact --nocapture

.PHONY: check-m4-perf
check-m4-perf: ## M4双向交替采样release门禁（跨线程echo，配对延迟中位比值<=1.15）
	$(CARGO) test --locked --release -p parrot --test bench_m4_actix_parity -- --include-ignored --nocapture --test-threads=1
