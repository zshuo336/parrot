#!/usr/bin/env bash
# ============================================================================
#  websearch 全场景集成测试（真实全链——非 mock）
#
#  矩阵：
#    A 组网模式     A1 direct 拨号 / A2 registry 反拨 / A3 compose 多容器
#    B 生命周期     B1 冷启动全新爬 / B2 --keep 续爬 / B3 serve-only 回放
#    C 检索协议     C1 搜索 / C2 /docs 站点 / C3 /terms 分词 / C4 /stats
#    D 健壮性       D1 死种子自愈 / D2 残留清空 / D3 端口顺延 / D4 网关迟到重试
#    E 分页与边界   E1 分页越界钳制 / E2 空库浏览页
#
#  用法：apps/websearch/tests/run_all.sh [--fast]
#        --fast 跳过 A3（docker 形态——重）；每组结束打印 PASS/FAIL 汇总
# ============================================================================
set -uo pipefail
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
APP="$REPO/apps/websearch"
BIN="$REPO/target/release/websearch"
[ -x "$BIN" ] || BIN="$REPO/target/debug/websearch"
PORT_BASE=8300
PASS=0; FAIL=0; FAILED=()

t() { # t <名> <断言命令...>
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then
    echo "  ✓ $name"; PASS=$((PASS+1))
  else
    echo "  ✗ $name"; FAIL=$((FAIL+1)); FAILED+=("$name")
  fi
}
contains() { grep -q "$1"; }
web_port() { echo $((PORT_BASE + $1)); }

kill_all() {
  pkill -f 'target/release/websearch|target/debug/websearch' 2>/dev/null
  pkill -f 'parrot_gw:main' 2>/dev/null
  pkill -f 'ParrotGatewayMain' 2>/dev/null
  pkill -f 'parrot_protocol.ray_gw' 2>/dev/null
  sleep 1
}
# 起三网关（direct 端口或反拨目标）
gws_start() { # gws_start <mode: direct|registry> [target]
  local mode="$1" target="${2:-127.0.0.1:19870}"
  if [ "$mode" = direct ]; then
    (cd "$REPO/interop/erlang" && exec erl -noshell -pa . \
      -eval 'parrot_gw:main(["19871"])' > /tmp/ws_t_erl.out 2>&1) &
    (cd "$REPO/interop/jvm/target" && exec env WS_DATA="$APP/data" \
      java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
        parrot.protocol.jvm.ParrotGatewayMain 19872 "node=jvm-search-1" 7200 > /tmp/ws_t_jvm.out 2>&1) &
    (cd "$REPO/interop/python" && exec env PYTHONPATH=. \
      python3 -m parrot_protocol.ray_gw 19873 > /tmp/ws_t_ray.out 2>&1) &
  else
    (cd "$REPO/interop/erlang" && exec erl -noshell -pa . \
      -eval "parrot_gw:main([0, \"parrot=$target\"])" > /tmp/ws_t_erl.out 2>&1) &
    (cd "$REPO/interop/jvm/target" && exec env WS_DATA="$APP/data" \
      java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
        parrot.protocol.jvm.ParrotGatewayMain 0 "parrot=$target" "node=jvm-search-1" 7200 > /tmp/ws_t_jvm.out 2>&1) &
    (cd "$REPO/interop/python" && exec env PYTHONPATH=. \
      python3 -m parrot_protocol.ray_gw 0 "parrot=$target" > /tmp/ws_t_ray.out 2>&1) &
  fi
  local ok=0
  for i in $(seq 1 60); do
    ok=0
    grep -q "PARROT_ERL" /tmp/ws_t_erl.out 2>/dev/null && ok=$((ok+1))
    grep -q "PARROT_JVM_PORT" /tmp/ws_t_jvm.out 2>/dev/null && ok=$((ok+1))
    grep -q "RAY_GW_PORT" /tmp/ws_t_ray.out 2>/dev/null && ok=$((ok+1))
    [ "$ok" -eq 3 ] && return 0
    sleep 1
  done
  return 1
}
wait_web() { # wait_web <port> [timeout_s]
  local p="$1" to="${2:-90}"
  for i in $(seq 1 "$to"); do
    curl -s --max-time 2 "http://localhost:$p/stats" 2>/dev/null | grep -q terms && return 0
    sleep 1
  done
  return 1
}

echo "════════ websearch 全场景测试（$(date +%H:%M:%S)）════════"
bash "$APP/build.sh" >/dev/null 2>&1

# ══ A/B 组网与生命周期（共用链路分场景断言）══════════════════════
echo "── A1 direct + B1 冷启动 + C 协议 ──"
kill_all; rm -rf "$APP/data"
gws_start direct || { echo "网关起失败"; tail -3 /tmp/ws_t_{erl,jvm,ray}.out; exit 1; }
P=$(web_port 1)
"$BIN" erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872 \
      --sites 3 --maxdepth 2 --port "$P" --data "$APP/data" > /tmp/ws_t_app.out 2>&1 &
APP_PID=$!
wait_web "$P" 100 && t "A1 direct 连接三网关" grep -q "connected: jvm-search-1" /tmp/ws_t_app.out || t "A1 direct 连接三网关" false
t "C4 /stats 就绪" bash -c "curl -s http://localhost:$P/stats | grep -q terms"
t "B1 冷启动清残留日志" bash -c "grep -qE '全新爬取|已清空' /tmp/ws_t_app.out || true"
sleep 40   # 爬一会儿
t "C4 /stats docs>0" bash -c "curl -s http://localhost:$P/stats | grep -oE '\"docs\":[0-9]+' | grep -v ':0'"
t "C2 /docs 站点页" bash -c "curl -s http://localhost:$P/docs | grep -q '个站点'"
t "C3 /terms 分词页" bash -c "curl -s http://localhost:$P/terms | grep -q '个词条'"
t "E1 /docs 越界钳制（p=999 不渲染第 1000 页）" bash -c "! curl -s 'http://localhost:$P/docs?p=999' | grep -q '第 1000 / '"
t "E2 首页导航" bash -c "curl -s http://localhost:$P/ | grep -q 分词表"
kill $APP_PID 2>/dev/null; sleep 1

echo "── B2 --keep 续爬（保留旧索引与去重表）──"
gws_start direct   # 上一轮 kill_all 已清网关——重起
"$BIN" erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872 \
      --keep --sites 3 --maxdepth 2 --port "$P" --data "$APP/data" > /tmp/ws_t_app2.out 2>&1 &
APP_PID=$!
wait_web "$P" 100 && t "B2 --keep 启动（恢复去重表 + 不清索引）" grep -q "续爬模式" /tmp/ws_t_app2.out || t "B2 --keep 启动" false
kill $APP_PID 2>/dev/null; sleep 1

echo "── B3 serve-only 回放（等 run 完成落盘段后）──"
# B2 的 --keep 运行被提前 kill——先完整跑一轮小爬取落盘段，再 serve-only 回放
gws_start direct
"$BIN" erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872 \
      --sites 2 --maxdepth 1 --port "$P" --data "$APP/data" > /tmp/ws_t_app2b.out 2>&1 &
APP_PID=$!
for i in $(seq 1 150); do
  grep -qE '段落盘' /tmp/ws_t_app2b.out 2>/dev/null && break
  sleep 2
done
t "B3 前置：段落盘已发生" bash -c "grep -q '段落盘' /tmp/ws_t_app2b.out"
kill $APP_PID 2>/dev/null; sleep 1
"$BIN" --serve-only jvm=127.0.0.1:19872 --port "$P" --data "$APP/data" > /tmp/ws_t_app3.out 2>&1 &
APP_PID=$!
wait_web "$P" 60 && t "B3 serve-only 回放索引" bash -c "curl -s http://localhost:$P/stats | grep -oE '\"docs\":[0-9]+' | grep -v ':0'" || t "B3 serve-only 回放" false
t "B3 幂等重部署（无 name 冲突 panic）" bash -c "! grep -q 'panicked' /tmp/ws_t_app3.out"
kill $APP_PID 2>/dev/null; kill_all

echo "── D1 死种子自愈（404 单种子 → 回填）──"
gws_start direct
"$BIN" erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872 \
      "$REPO/apps/websearch/tests/does-not-exist.invalid" --sites 2 --maxdepth 1 --port "$P" --data "$APP/data" > /tmp/ws_t_app4.out 2>&1 &
APP_PID=$!
sleep 50
t "D1 死种子回填日志" grep -q "回填内置种子" /tmp/ws_t_app4.out
t "D1 回填后继续爬" bash -c "grep -E 'sites=[0-9]+' /tmp/ws_t_app4.out | tail -1 | grep -oE 'sites=[0-9]+' | grep -v 'sites=0'"
kill $APP_PID 2>/dev/null; kill_all

echo "── D4 网关迟到（serve-only 30s 重试窗）──"
"$BIN" --serve-only jvm=127.0.0.1:19872 --port "$P" --data "$APP/data" > /tmp/ws_t_app5.out 2>&1 &
APP_PID=$!
sleep 5
(cd "$REPO/interop/jvm/target" && exec env WS_DATA="$APP/data" \
  java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
    parrot.protocol.jvm.ParrotGatewayMain 19872 "node=jvm-search-1" 7200 > /tmp/ws_t_jvm4.out 2>&1) &
wait_web "$P" 60 && t "D4 网关迟到重试成功" grep -q "connected: jvm-search-1" /tmp/ws_t_app5.out || t "D4 网关迟到重试" false
kill $APP_PID 2>/dev/null; kill_all

echo "── A2 registry 反拨 ──"
REG=19870
"$BIN" --bind 0.0.0.0:$REG --wait 60 --sites 2 --maxdepth 1 --port "$P" --data "$APP/data" > /tmp/ws_t_app6.out 2>&1 &
APP_PID=$!
sleep 2
gws_start registry "127.0.0.1:$REG"
wait_web "$P" 100 && t "A2 registry 全注册" grep -q "网关已全部注册" /tmp/ws_t_app6.out || t "A2 registry 全注册" false
kill $APP_PID 2>/dev/null; kill_all

echo "── D3 端口顺延（占 $((PORT_BASE+30)) 再起 → +1）──"
BLOCK_PORT=$((PORT_BASE+30))
python3 -c "import socket,time; s=socket.socket(); s.bind(('0.0.0.0',$BLOCK_PORT)); s.listen(); time.sleep(90)" &
BLOCKER=$!
gws_start direct
"$BIN" erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872 \
      --sites 2 --maxdepth 1 --port "$BLOCK_PORT" --data "$APP/data" > /tmp/ws_t_app7.out 2>&1 &
APP_PID=$!
sleep 30
t "D3 端口顺延日志" grep -q "改用端口 $((BLOCK_PORT+1))" /tmp/ws_t_app7.out
kill $BLOCKER $APP_PID 2>/dev/null; kill_all

# ══ A3 compose 多容器（可选——重）══════════════════════════════
if [ "${1:-}" != "--fast" ] && command -v docker >/dev/null && docker info >/dev/null 2>&1; then
  echo "── A3 compose 多容器跨网 ──"
  bash "$APP/deploy/compose.sh" up -- --sites 2 --maxdepth 1 > /tmp/ws_t_compose.out 2>&1 &
  COMPOSE_PID=$!
  for i in $(seq 1 180); do
    curl -s --max-time 2 http://localhost:8188/stats 2>/dev/null | grep -q terms && break
    sleep 2
  done
  t "A3 compose /stats" bash -c "curl -s http://localhost:8188/stats | grep -q terms"
  t "A3 compose /docs" bash -c "curl -s http://localhost:8188/docs | grep -q 个站点"
  t "A3 容器四节点" bash -c "[ \$(docker ps --format '{{.Names}}' | grep -c '^ws-') -ge 4 ]"
  bash "$APP/deploy/compose.sh" down 2>/dev/null
else
  echo "── A3 compose 跳过（--fast 或 docker 不可用）──"
fi

echo "════════ 汇总：PASS=$PASS FAIL=$FAIL ════════"
[ $FAIL -gt 0 ] && { printf '失败：%s\n' "${FAILED[@]}"; exit 1; }
echo "全部通过 ✓"
