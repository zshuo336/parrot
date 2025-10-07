#!/usr/bin/env bash
# parrot-obs 网关一键起停（三方言演示网关——观测/联调速用）
#
# 用法：
#   gw.sh up     # 起 erl/jvm/ray 三网关（默认端口 19871/19872/19873）→ 打印 NODES
#   gw.sh down   # 全部停掉
#   gw.sh status # 端口监听 + parrot-obs status 一行摘要
#   gw.sh log [erl|jvm|ray]  # 看网关日志（默认全部）
#
# 环境变量：GW_ERL_PORT / GW_JVM_PORT / GW_RAY_PORT / GW_JVM_NODE
set -uo pipefail
cd "$(dirname "$0")/../.."   # 仓库根

ERL_PORT=${GW_ERL_PORT:-19871}
JVM_PORT=${GW_JVM_PORT:-19872}
RAY_PORT=${GW_RAY_PORT:-19873}
JVM_NODE=${GW_JVM_NODE:-jvm-search-1}
RUN=/tmp/parrot-obs-gw
OBS=./target/debug/parrot-obs

# 双 fork 守护化（脱离会话/进程组——免疫终端关闭与会话清理）
daemon() {  # daemon LOG CMD...
    local log=$1; shift
    python3 - "$log" "$@" <<'PYEOF'
import os, sys
log, cmd = sys.argv[1], sys.argv[2:]
if os.fork() > 0:
    sys.exit(0)
os.setsid()
if os.fork() > 0:
    sys.exit(0)
f = os.open(log, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
os.dup2(f, 1); os.dup2(f, 2)
os.execvp(cmd[0], cmd)
PYEOF
}

up() {
    down
    mkdir -p "$RUN"
    echo "── 起三网关（erl:$ERL_PORT jvm:$JVM_PORT ray:$RAY_PORT）…"
    # erl：-noinput 防 stdin EOF 即退；beam 驻留
    (cd interop/erlang && erlc -W parrot_gw.erl 2>/dev/null; \
     daemon "$RUN/erl.log" erl -noinput -noshell -pa . \
       -eval "parrot_gw:main([\"$ERL_PORT\"])")
    # jvm：需先 make build-jvm（mvn package）；idle 86400s 驻留
    (cd interop/jvm/target && daemon "$RUN/jvm.log" java \
       -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
       parrot.protocol.jvm.ParrotGatewayMain "$JVM_PORT" "node=$JVM_NODE" 86400)
    # ray：python 模块（PYTHONPATH=interop/python）
    (cd interop/python && daemon "$RUN/ray.log" env PYTHONPATH=. \
       python3 -u -m parrot_protocol.ray_gw "$RAY_PORT")
    # 等三端口就绪（最长 30s——ray init 慢）
    for i in $(seq 1 30); do
        n=0
        lsof -nP -iTCP:"$ERL_PORT" -sTCP:LISTEN >/dev/null 2>&1 && n=$((n+1))
        lsof -nP -iTCP:"$JVM_PORT" -sTCP:LISTEN >/dev/null 2>&1 && n=$((n+1))
        lsof -nP -iTCP:"$RAY_PORT" -sTCP:LISTEN >/dev/null 2>&1 && n=$((n+1))
        [ "$n" = 3 ] && break
        sleep 1
    done
    if [ "$n" != 3 ]; then
        echo "✗ 网关起失败（就绪 $n/3）——日志："; tail -3 "$RUN"/*.log; exit 1
    fi
    echo "✓ 三网关就绪（日志 $RUN/*.log）"
    echo ""
    echo '观测（zsh 用户直接复制 NODES 行使用）：'
    echo "  NODES=\"erl=127.0.0.1:$ERL_PORT ray=127.0.0.1:$RAY_PORT jvm=127.0.0.1:$JVM_PORT\""
    echo "  $OBS status \"\$NODES\"      # 单串也认（已兼容 zsh 不分词）"
    echo "  $OBS metrics \$NODES"
    echo "  $OBS web \$NODES             # → http://localhost:8190"
}

down() {
    pkill -f "parrot_gw:main" 2>/dev/null
    pkill -f "ParrotGatewayMain" 2>/dev/null
    pkill -f "parrot_protocol.ray_gw" 2>/dev/null
    sleep 1
    echo "✓ 网关已停（gw.sh up 可再起）"
}

status() {
    for spec in "erl:$ERL_PORT" "jvm:$JVM_PORT" "ray:$RAY_PORT"; do
        n=${spec%%:*}; p=${spec##*:}
        if lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1; then
            echo "  $n  ✓ :$p"
        else
            echo "  $n  ✗ :$p（未监听——gw.sh up）"
        fi
    done
    [ -x "$OBS" ] && "$OBS" status "erl=127.0.0.1:$ERL_PORT ray=127.0.0.1:$RAY_PORT jvm=127.0.0.1:$JVM_PORT" 2>&1 | head -4
}

log() {
    w=${1:-all}
    if [ "$w" = all ]; then tail -n 15 "$RUN"/*.log 2>/dev/null; else tail -n 15 "$RUN/$w.log" 2>/dev/null; fi
}

case "${1:-help}" in
    up) up ;;
    down) down ;;
    status) status ;;
    log) log "${2:-all}" ;;
    *) sed -n '2,12p' "$0" ;;
esac
