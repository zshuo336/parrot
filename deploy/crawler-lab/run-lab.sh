#!/usr/bin/env bash
# ============================================================================
#  crawler-lab 一键运行：五运行时全链集成（双模式组网）
#
#   组网模式（LAB_MODE）：
#     direct  （默认）应用主动拨号三网关（erl=/ray=/jvm= 显式地址——测试形态）
#     registry 应用监听 19870，三网关启动即主动注册到应用（生产形态：
#             应用零网关地址知识；网关分布可任意）
#
#   Erlang(OTP/ETS)   URL Frontier      —— IO 密集·海量轻量进程
#   Parrot/Rust       爬取+编排+hub     —— tokio 并发·路由
#   Ray(Python)       索引构建          —— CPU 密集·分词统计
#   Akka(JVM)         搜索 API          —— 高并发短查询·面向用户
#   TS Lite           用户终端          —— 边缘轻客户端（Node tcp:// 直连）
#
#  用法：[LAB_MODE=registry] ./run-lab.sh [--pages N] [--depth D] ...]
#  依赖：erl / java(mvn jar) / python3+ray / node / cargo build -p crawler-lab
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/../.."
ROOT="$PWD"

LAB_MODE="${LAB_MODE:-direct}"
APP_PORT=19870
PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
}
trap cleanup EXIT INT TERM

if [ "$LAB_MODE" = "registry" ]; then
  # ══════════ 模式 B：registry（网关主动注册到应用）══════════
  echo "==> [registry] 先起应用（监听 :$APP_PORT，等待网关注册）"
  BIN=./target/release/crawler-lab
  [ -x "$BIN" ] || BIN=./target/debug/crawler-lab
  "$BIN" --bind 0.0.0.0:$APP_PORT --wait 60 "$@" > /tmp/lab_app.out 2>&1 &
  APP_PID=$!
  PIDS+=("$APP_PID")
  sleep 1  # 应用监听就位

  echo "==> [registry] 起 Erlang frontier 网关（注册 → 127.0.0.1:$APP_PORT）"
  (cd interop/erlang && erlc parrot_gw.erl 2>/dev/null && \
   exec erl -noshell -pa . -eval 'parrot_gw:main([0, "parrot=127.0.0.1:19870"])' \
   > /tmp/lab_erl.out 2>&1) &
  PIDS+=("$!")

  echo "==> [registry] 起 JVM(akka) 搜索网关（注册）"
  (cd interop/jvm/target && \
   exec java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
     parrot.protocol.jvm.CrawlerSearchMain 0 "parrot=127.0.0.1:19870" 7200 \
   > /tmp/lab_jvm.out 2>&1) &
  PIDS+=("$!")

  echo "==> [registry] 起 Ray(python) 索引网关（注册）"
  (cd interop/python && exec env PYTHONPATH=. \
   python3 -m parrot_protocol.ray_gw 0 "parrot=127.0.0.1:19870" > /tmp/lab_ray.out 2>&1) &
  PIDS+=("$!")

  # 等应用完成（等待注册 + 场景执行一体）
  wait $APP_PID
  rc=$?

  # TS 终端（JVM 网关此模式无监听端口——TS 查询走 direct 模式验证）
  echo "==> 完成 rc=$rc（registry 模式；日志 /tmp/lab_*.out）"
  exit $rc
fi

# ══════════ 模式 A：direct（应用主动拨号网关）══════════
echo "==> [1/5] Erlang frontier 网关"
(cd interop/erlang && erlc parrot_gw.erl 2>/dev/null && \
 exec erl -noshell -pa . -eval 'parrot_gw:main(["19861"])' > /tmp/lab_erl.out 2>&1) &
ERL_PID=$!
PIDS+=("$ERL_PID")

echo "==> [2/5] JVM(akka) 搜索网关"
(cd interop/jvm/target && \
 exec java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
   parrot.protocol.jvm.CrawlerSearchMain 19862 7200 > /tmp/lab_jvm.out 2>&1) &
JVM_PID=$!
PIDS+=("$JVM_PID")

echo "==> [3/5] Ray(python) 索引网关"
(cd interop/python && exec env PYTHONPATH=. \
 python3 -m parrot_protocol.ray_gw 19863 > /tmp/lab_ray.out 2>&1) &
RAY_PID=$!
PIDS+=("$RAY_PID")

# 等三网关就绪（stdout 端口行）
ok=0
for i in $(seq 1 90); do
  ok=0
  grep -q "PARROT_ERL_PORT=19861" /tmp/lab_erl.out 2>/dev/null && ok=$((ok+1))
  grep -q "PARROT_JVM_PORT=19862" /tmp/lab_jvm.out 2>/dev/null && ok=$((ok+1))
  grep -q "RAY_GW_PORT=19863" /tmp/lab_ray.out 2>/dev/null && ok=$((ok+1))
  [ "$ok" -eq 3 ] && break
  sleep 1
done
if [ "$ok" -ne 3 ]; then
  echo "网关未全部就绪（$ok/3）"; tail -3 /tmp/lab_erl.out /tmp/lab_jvm.out /tmp/lab_ray.out; exit 1
fi
echo "    三网关就绪（erl:19861 jvm:19862 ray:19863）"

echo "==> [4/5] Rust 应用（apps/crawler-lab——爬取→双路索引→搜索验证）"
BIN=./target/release/crawler-lab
[ -x "$BIN" ] || BIN=./target/debug/crawler-lab
"$BIN" erl=127.0.0.1:19861 ray=127.0.0.1:19863 jvm=127.0.0.1:19862 "$@"
rc=$?

echo "==> [5/5] TS Lite 用户终端（真实'用户 Web 访问'——Node tcp:// 直连 JVM 网关）"
if [[ "$rc" -eq 0 && " $* " != *" --skip-ts "* ]]; then
  (cd interop/typescript-lite && npm run build >/dev/null 2>&1 && \
   node --test --test-reporter=spec tests/crawler_client.test.mjs 2>&1 | tail -6)
  ts_rc=$?
  [ "$ts_rc" -ne 0 ] && rc=$ts_rc
fi

echo "==> 完成 rc=$rc（direct 模式；网关日志：/tmp/lab_*.out）"
exit $rc
