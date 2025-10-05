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
#    make test MODE=stress     # 只跑压测（release, --include-ignored）
#    make test MODE=polyglot   # 只跑多语言：TS + Python + JVM + Erlang + C++
#    make test MODE=full-polyglot  # 全模式 + 多语言全家桶
#
#  ── 单项快捷 ──────────────────────────────────────────────────────────────
#    make lint                 # fmt --check + clippy -D warnings
#    make bench                # remote-bench --gate p1（性能门禁）
#    make matrix               # Rust↔JVM 一致性矩阵（需先 make build-jvm）
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
ERL_DIR  := interop/erlang

SHELL := /bin/bash
.SHELLFLAGS := -eu -o pipefail -c

.DEFAULT_GOAL := help
.PHONY: help build build-all build-ts build-cpp build-jvm build-python \
        test test-fast test-full test-stress test-polyglot test-full-polyglot test-lab \
        lint fmt clippy bench matrix clean distclean \
        ts py jvm erl cpp check-ts check-py check-jvm check-erl check-cpp \
        build-release-bins

# ── 帮助 ──────────────────────────────────────────────────────────────────
help: ## 显示本帮助
	@echo "Parrot 统一构建/测试入口（详细模式说明见 Makefile 头部注释）"
	@echo ""
	@echo "  make build [MODE=release]      编译 Rust（默认 debug）"
	@echo "  make build-all                 全语言编译（Rust+TS+C++/JVM+Py）"
	@echo "  make test [MODE=...]           测试（默认 fast；详见下方模式表）"
	@echo "  make lint / bench / matrix     质量门禁 / 性能门禁 / 一致性矩阵"
	@echo "  make ts py jvm erl cpp         单语言测试快捷方式"
	@echo "  make clean / distclean         清理"
	@echo ""
	@echo "测试模式："
	@echo "  fast          debug workspace 全量（默认）"
	@echo "  full          debug 全量 + release 阈值 + ignored 压测 + lint"
	@echo "  stress        只跑压测（release --include-ignored）"
	@echo "  polyglot      TS + Python + JVM + Erlang + C++（缺工具链自动 SKIP）"
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
CARGO_REL   := $(if $(filter $(RELEASE),1),--release,)
CARGO_VERB  := $(if $(filter $(V),1),-v,)
TEST_THREADS := $(if $(N),--test-threads=$(N),)

build: ## 编译 Rust（MODE=release 或 RELEASE=1 时编 release）
	$(CARGO) build $(CARGO_SCOPE) $(CARGO_FEAT) $(CARGO_REL) $(CARGO_VERB)

build-all: build build-ts build-cpp build-jvm build-python ## 全语言编译

build-ts: ## 编译 TypeScript lite（tsc + esbuild bundle）
	cd $(TS_DIR) && npm run build && npm run bundle

build-cpp: ## 编译 C++ lite 测试（clang++ -O1，产物在 interop/cpp-lite/bin/）
	cd $(CPP_DIR) && mkdir -p bin && \
	clang++ -std=c++17 -O1 -Wall -Wextra parrot_lite.cpp test_vectors.cpp -o bin/test_vectors && \
	clang++ -std=c++17 -O1 -Wall -Wextra parrot_lite.cpp test_interop.cpp -o bin/test_interop

build-jvm: ## 打包 JVM 网关 jar（mvn -q package）
	cd $(JVM_DIR) && mvn -q -s settings-central.xml package -DskipTests

build-python: ## Python 语法预热（compileall；依赖见 pyproject.toml）
	cd $(PY_DIR) && python3 -m compileall -q parrot_protocol tests

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
	$(CARGO) test $(CARGO_SCOPE) $(CARGO_FEAT) $(CARGO_REL) $(CARGO_VERB) -- $(TEST_THREADS) $(EXTRA)
	@echo "✓ 快速模式完成（debug workspace 测试）"

# ---- 全模式：debug 全量 + release 阈值 + ignored 压测 + lint ---------------
test-full: test-fast lint
	@echo ""
	@echo "==> release 阈值测试（test_thread_advantages --include-ignored）"
	$(CARGO) test --release -p parrot --test test_thread_advantages -- --include-ignored $(TEST_THREADS)
	@echo ""
	@echo "==> release 压测（ignored stress/bench 套件，release --include-ignored）"
	$(CARGO) test --release $(CARGO_SCOPE) -- --include-ignored $(TEST_THREADS)
	@echo "✓ 全模式完成（debug 全量 + release 阈值 + 压测 + lint）"

# ---- 压测模式：只跑 ignored 的压测/阈值（release）--------------------------
test-stress:
	$(CARGO) test --release $(CARGO_SCOPE) $(CARGO_FEAT) --lib --bins --tests -- --include-ignored $(TEST_THREADS) $(EXTRA)
	@echo "✓ 压测模式完成（release --include-ignored；doc-tests 由 fast/full 覆盖——ignore 示例不参与压测语义）"

# ---- 多语言模式：TS + Python + JVM + Erlang + C++（缺工具链 SKIP）----------
test-polyglot: check-ts check-py check-jvm check-erl check-cpp
	@echo "✓ 多语言模式完成（TS/Python/JVM/Erlang/C++）"

# ---- 五运行时全链集成场景（crawler-lab：大规模爬虫+索引+Web 检索）----------
# 依赖齐备才跑（erl/java/python+ray/node）；缺项跳过并提示。
LAB_SCRIPT := deploy/crawler-lab/run-lab.sh
test-lab:
	@if [ -x "$(LAB_SCRIPT)" ] && command -v erl >/dev/null 2>&1 \
	   && command -v java >/dev/null 2>&1 && command -v node >/dev/null 2>&1 \
	   && python3 -c "import ray" >/dev/null 2>&1; then \
		$(CARGO) build -p crawler-lab $(CARGO_Q) ; \
		./$(LAB_SCRIPT) --pages 200 --depth 2 --fanout 3 --batch 32 ; \
	else \
		echo "==> [lab] 跳过（需 erl+java+node+python3/ray 工具链）" ; \
	fi
	@echo "✓ 五运行时全链场景完成（Erlang+Rust+Ray+Akka+TS-Lite）"

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

ERL_RUN := erl -noshell -pa . -eval 'c:c(parrot_gw), c:c(test_wire), test_wire:run(), halt().'

erl:
	cd $(ERL_DIR) && $(ERL_RUN)

cpp: build-cpp
	cd $(CPP_DIR)/bin && ./test_vectors && ./test_interop

# check-*：polyglot 聚合用；缺工具链时 SKIP（打印提示），装了则必须通过
check-ts:
	@if command -v npm >/dev/null 2>&1; then \
		echo "==> [npm] TypeScript lite 测试"; \
		(cd $(TS_DIR) && npm test); \
	else \
		echo "==> [npm] SKIP：未安装 npm"; \
	fi

check-py:
	@if command -v python3 >/dev/null 2>&1; then \
		echo "==> [python3] pytest 协议向量测试"; \
		(cd $(PY_DIR) && python3 -m pytest tests/ -q); \
	else \
		echo "==> [python3] SKIP：未安装 python3"; \
	fi

check-jvm:
	@if command -v mvn >/dev/null 2>&1; then \
		echo "==> [mvn] JVM 协议测试"; \
		(cd $(JVM_DIR) && mvn -q -s settings-central.xml test); \
	else \
		echo "==> [mvn] SKIP：未安装 mvn"; \
	fi

check-erl:
	@if command -v erl >/dev/null 2>&1; then \
		echo "==> [erl] Erlang 网关 wire 测试"; \
		(cd $(ERL_DIR) && $(ERL_RUN)); \
	else \
		echo "==> [erl] SKIP：未安装 Erlang"; \
	fi

check-cpp:
	@if command -v clang++ >/dev/null 2>&1; then \
		echo "==> [clang++] C++ lite wire/interop 测试"; \
		$(MAKE) --no-print-directory build-cpp; \
		(cd $(CPP_DIR)/bin && ./test_vectors && ./test_interop); \
	else \
		echo "==> [clang++] SKIP：未安装 clang++"; \
	fi

# ══════════════════════════════════════════════════════════════════════════
# 质量门禁 / 工具
# ══════════════════════════════════════════════════════════════════════════

lint: fmt clippy ## fmt --check + clippy -D warnings

fmt:
	$(CARGO) fmt --all --check

clippy:
	$(CARGO) clippy $(CARGO_SCOPE) --all-targets $(CARGO_FEAT) $(CARGO_VERB) -- -D warnings

build-release-bins:
	$(CARGO) build --release -p parrot-remote --bins

bench: build-release-bins ## remote-bench 性能门禁（ask p50<150µs / tell<60µs）
	$(CARGO) run --release -p parrot-remote --bin remote-bench -- --gate p1

matrix: build-jvm ## Rust↔JVM 一致性矩阵（RTT p50<300µs 门禁）
	$(CARGO) run -p parrot-remote --bin interop-matrix

clean:
	$(CARGO) clean
	rm -rf $(TS_DIR)/dist $(TS_DIR)/build
	rm -rf $(CPP_DIR)/bin

distclean: clean
	rm -rf $(TS_DIR)/node_modules
