#!/usr/bin/env bash
# ============================================================================
#  websearch 一键运行：真实搜索引擎五运行时全链
#
#   Erlang(OTP/ETS)   URL Frontier      —— 真实 URL 去重 + BFS 调度
#   Parrot/Rust       爬虫+编排+Web页    —— tokio 并发真实 HTTPS 漫爬
#   Ray(Python/jieba) 中文分词+词频      —— CPU 密集·中文网页分词
#   Akka(JVM)         搜索 API+落盘     —— BM25·倒排·段文件持久化
#   TS Lite           用户终端           —— 查询 CLI（run.sh 自动跑）
#
#  用法：./run.sh <seed-url>... [--pages N] [--maxdepth D] [--port P]
#  示例：./run.sh https://baike.baidu.com/item/分布式系统 --pages 100 --port 8080
#  完成后浏览器打开 http://localhost:8080 即可百度式搜索（中文 jieba 分词）
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/../.."
ROOT="$PWD"

PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done; }
trap cleanup EXIT INT TERM

# 依赖检查
for c in erl java python3 node; do
  command -v "$c" >/dev/null || { echo "缺依赖：$c"; exit 1; }
done
python3 -c "import jieba" 2>/dev/null || { echo "缺 python jieba：pip3 install jieba"; exit 1; }
python3 -c "import ray" 2>/dev/null || { echo "缺 python ray：pip3 install ray"; exit 1; }
[ -f interop/jvm/target/parrot-protocol-jvm-0.1.0.jar ] || {
  echo "缺 JVM 网关 jar——先 make build-jvm（interop/jvm）"; exit 1; }

echo "==> [1/6] Erlang frontier 网关（:19871）"
(cd interop/erlang && erlc parrot_gw.erl 2>/dev/null && \
 exec erl -noshell -pa . -eval 'parrot_gw:main(["19871"])' > /tmp/ws_erl.out 2>&1) &
PIDS+=("$!")

echo "==> [2/6] JVM(akka) 检索网关（:19872——BM25+段落盘）"
(cd interop/jvm/target && \
 exec env WS_DATA="$ROOT/apps/websearch/data" \
   java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
   parrot.protocol.jvm.ParrotGatewayMain 19872 "node=jvm-search-1" 7200 \
 > /tmp/ws_jvm.out 2>&1) &
PIDS+=("$!")

echo "==> [3/6] Ray(jieba 分词) 网关（:19873）"
(cd interop/python && exec env PYTHONPATH=. \
 python3 -m parrot_protocol.ray_gw 19873 > /tmp/ws_ray.out 2>&1) &
PIDS+=("$!")

# 等三网关就绪
ok=0
for i in $(seq 1 90); do
  ok=0
  grep -q "PARROT_ERL_PORT=19871" /tmp/ws_erl.out 2>/dev/null && ok=$((ok+1))
  grep -q "PARROT_JVM_PORT=19872" /tmp/ws_jvm.out 2>/dev/null && ok=$((ok+1))
  grep -q "RAY_GW_PORT=19873" /tmp/ws_ray.out 2>/dev/null && ok=$((ok+1))
  [ "$ok" -eq 3 ] && break
  sleep 1
done
[ "$ok" -eq 3 ] || { echo "网关未就绪（$ok/3）"; tail -3 /tmp/ws_erl.out /tmp/ws_jvm.out /tmp/ws_ray.out; exit 1; }
echo "    三网关就绪（erl:19871 jvm:19872 ray:19873）"

echo "==> [4/6] Rust 应用（真实漫爬 + 编排 + Web 服务）"
BIN=./target/release/websearch
[ -x "$BIN" ] || BIN=./target/debug/websearch
"$BIN" erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872 "$@"
rc=$?

echo "==> 完成 rc=$rc（日志 /tmp/ws_*.out；数据 apps/websearch/data/）"
exit $rc
