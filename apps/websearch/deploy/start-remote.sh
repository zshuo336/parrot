#!/usr/bin/env bash
# ============================================================================
#  start-remote.sh —— 跨机网关拉起（每节点跑一次；产物须已 distribute.sh 就位）
#
#  拓扑：应用（本机）+ 三网关（任意机器）。
#   Erlang frontier   → 本脚本参数 erl
#   Ray jieba 分词     → 本脚本参数 ray
#   JVM akka 检索      → 本脚本参数 jvm
#
#  组网：registry 模式——各网关主动反拨注册到应用 --bind 端口。
#  应用零网关地址知识（生产形态：网关分布/迁移应用无感）。
#
#  用法（在对应节点上）：
#    ./start-remote.sh erl  <应用IP:19870> [--root /opt/parrot/websearch]
#    ./start-remote.sh ray  <应用IP:19870> [--root /opt/parrot/websearch]
#    ./start-remote.sh jvm  <应用IP:19870> [--data /var/lib/websearch] [--root DIR]
# ============================================================================
set -euo pipefail

ROLE="${1:?用法：$0 erl|ray|jvm <应用IP:PORT> [--root DIR] [--data DIR]}"
APP_TARGET="${2:?缺应用地址（应用 --bind 的对外 IP:端口）}"
shift 2
NODE_ROOT="/opt/parrot/websearch"
DATA_DIR="/var/lib/websearch"
while [ $# -gt 0 ]; do
  case "$1" in
    --root) NODE_ROOT="$2"; shift 2 ;;
    --data) DATA_DIR="$2"; shift 2 ;;
    *) echo "未知参数 $1"; exit 1 ;;
  esac
done

case "$ROLE" in
  erl)
    command -v erl >/dev/null || { echo "缺 erl"; exit 1; }
    echo "==> erl frontier 网关 → 注册 $APP_TARGET"
    # 宿主网关源码编译（distribute.sh 分发的是 .erl——目标机 OTP 版本自洽）
    (cd "$NODE_ROOT/gw_erl" && erlc parrot_gw.erl) 2>/dev/null \
      || erlc "$NODE_ROOT/gw_erl/parrot_gw.erl" -o "$NODE_ROOT/gw_erl"
    # 业务 beam 同理（OTP 版本对齐——容器形态踩过 badfile）
    erlc "$NODE_ROOT/erlang/frontier.erl" -o "$NODE_ROOT/erlang" 2>/dev/null || true
    exec erl -noshell -pa "$NODE_ROOT/erlang" "$NODE_ROOT/gw_erl" \
      -eval 'parrot_gw:main([0, "parrot='"$APP_TARGET"'"])'
    ;;
  ray)
    command -v python3 >/dev/null || { echo "缺 python3"; exit 1; }
    python3 -c "import jieba, ray" 2>/dev/null || { echo "缺 jieba/ray"; exit 1; }
    echo "==> ray tokenizer 网关 → 注册 $APP_TARGET"
    # ray_gw 参数形态：端口0 + parrot= 注册目标
    cd "$NODE_ROOT/gw_py" 2>/dev/null || cd "$NODE_ROOT"
    exec env PYTHONPATH="$NODE_ROOT/gw_py:$NODE_ROOT/python" \
      python3 -m parrot_protocol.ray_gw 0 "parrot=$APP_TARGET"
    ;;
  jvm)
    command -v java >/dev/null || { echo "缺 java"; exit 1; }
    mkdir -p "$DATA_DIR/index"
    echo "==> jvm search 网关（WS_DATA=$DATA_DIR）→ 注册 $APP_TARGET"
    cd "$NODE_ROOT/gw"
    exec env WS_DATA="$DATA_DIR" \
      java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
        parrot.protocol.jvm.ParrotGatewayMain 0 "parrot=$APP_TARGET" "node=jvm-search-1" 7200
    ;;
  *) echo "未知角色 $ROLE（erl|ray|jvm）"; exit 1 ;;
esac
