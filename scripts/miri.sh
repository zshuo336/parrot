#!/usr/bin/env bash
# miri 门禁（M5 unsafe 前置）：
# 只对 unsafe 密集模块相关测试跑 miri（全 workspace 跑不现实）。
# 用法：./scripts/miri.sh [test_filter]
#
# 注意：macOS 上 tokio runtime 依赖 kqueue，miri 不支持该系统调用——
# 需要 runtime 的测试（envelope 回复路径等）无法在 miri 下运行。
# 因此门禁聚焦 unsafe 密集的 single_alloc（单块信封裸指针操作）。
set -euo pipefail
cd "$(dirname "$0")/.."

# rustup +nightly 切换；无 nightly 时给出明确指引
if ! rustup toolchain list | grep -q nightly; then
    echo "需要 nightly 工具链：rustup toolchain install nightly"
    exit 1
fi

FILTER="${1:-single_alloc}"

cargo +nightly miri test -p parrot --lib -- "$FILTER"
echo "miri gate done"
