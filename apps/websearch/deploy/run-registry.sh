#!/usr/bin/env bash
# ============================================================================
#  run-registry.sh —— 单机 registry 模式（网关反拨注册——进程形态）
#
#  与 run.sh（direct：应用主动拨号）对照的生产组网形态：
#    应用 --bind :19870 监听，三网关各自起进程并主动反拨注册。
#    应用零网关地址知识（网关可分布任意机器——本脚本单机演示同机多进程）。
#
#  用法：[WS_SITES=100] [WS_DEPTH=10] [WS_PORT=8080] ./run-registry.sh [seed...]
# ============================================================================
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
APP="$HERE/.."

WS_PORT="${WS_PORT:-8080}"
REG_PORT=19870
PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT INT TERM

for c in erl java python3; do command -v "$c" >/dev/null || { echo "缺依赖：$c"; exit 1; }; done
bash "$APP/build.sh" >/dev/null

echo "==> [1/4] 应用（registry 监听 :$REG_PORT）"
BIN="$REPO/target/release/websearch"; [ -x "$BIN" ] || BIN="$REPO/target/debug/websearch"
"$BIN" --bind 0.0.0.0:$REG_PORT --wait 60 --port "$WS_PORT" "$@" > /tmp/ws_reg_app.out 2>&1 &
PIDS+=($!)

echo "==> [2/4] 三网关（反拨注册 → 127.0.0.1:$REG_PORT）"
(cd "$REPO/interop/erlang" && exec erl -noshell -pa . \
  -eval "parrot_gw:main([0, \"parrot=127.0.0.1:$REG_PORT\"])" > /tmp/ws_reg_erl.out 2>&1) &
PIDS+=($!)
(cd "$REPO/interop/jvm/target" && exec env WS_DATA="$APP/data" \
  java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
    parrot.protocol.jvm.ParrotGatewayMain 0 "parrot=127.0.0.1:$REG_PORT" "node=jvm-search-1" 7200 \
  > /tmp/ws_reg_jvm.out 2>&1) &
PIDS+=($!)
(cd "$REPO/interop/python" && exec env PYTHONPATH=. \
  python3 -m parrot_protocol.ray_gw 0 "parrot=127.0.0.1:$REG_PORT" > /tmp/ws_reg_ray.out 2>&1) &
PIDS+=($!)

echo "==> [3/4] 等注册与爬取"
for i in $(seq 1 90); do
  grep -q "网关已全部注册" /tmp/ws_reg_app.out 2>/dev/null && break
  sleep 1
done
grep -q "网关已全部注册" /tmp/ws_reg_app.out || { echo "注册超时"; tail -3 /tmp/ws_reg_{erl,jvm,ray}.out; exit 1; }
echo "    ✓ 三网关已注册（erl/ray/jvm → :$REG_PORT）"

echo "==> [4/4] Web 就绪"
for i in $(seq 1 60); do grep -q "浏览器打开" /tmp/ws_reg_app.out 2>/dev/null && break; sleep 1; done
grep "浏览器打开" /tmp/ws_reg_app.out | tail -1
echo "    （Ctrl-C 结束；日志 /tmp/ws_reg_*.out）"
wait $(jobs -p | head -1) 2>/dev/null
