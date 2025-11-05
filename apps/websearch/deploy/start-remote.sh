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

if [ "${1:-}" = --help ] || [ "${1:-}" = -h ]; then
  echo "start-remote.sh erl|ray|jvm HOST:PORT [--root ABSOLUTE_RELEASE_DIR] [--data DIR]"
  exit 0
fi
ROLE="${1:?用法：$0 erl|ray|jvm <应用IP:PORT> [--root DIR] [--data DIR]}"
APP_TARGET="${2:?缺应用地址（应用 --bind 的对外 IP:端口）}"
shift 2
NODE_ROOT="/opt/parrot/websearch"
DATA_DIR="/var/lib/websearch"
SEEN_ROOT=0
SEEN_DATA=0
while [ $# -gt 0 ]; do
  case "$1" in
    --root) [ "$#" -ge 2 ] && [ -n "$2" ] && [ "$SEEN_ROOT" -eq 0 ] || { echo "--root requires one directory" >&2; exit 2; }; NODE_ROOT="$2"; SEEN_ROOT=1; shift 2 ;;
    --data) [ "$#" -ge 2 ] && [ -n "$2" ] && [ "$SEEN_DATA" -eq 0 ] || { echo "--data requires one directory" >&2; exit 2; }; DATA_DIR="$2"; SEEN_DATA=1; shift 2 ;;
    *) echo "未知参数 $1"; exit 1 ;;
  esac
done

case "$NODE_ROOT" in /*) ;; *) echo "--root must be absolute" >&2; exit 2 ;; esac
case "$DATA_DIR" in /*) ;; *) DATA_DIR="$PWD/$DATA_DIR" ;; esac
# Parse data as data, before compiling or creating storage. Every remote node
# already requires Python 3 for the verified bundle receiver.
python3 - "$APP_TARGET" <<'PY_TARGET'
import sys
from urllib.parse import urlsplit
try:
    text = sys.argv[1]
    address = urlsplit('//'+text)
    if (not address.hostname or not address.port or address.username is not None or
            address.password is not None or address.path or address.query or address.fragment or
            any(c.isspace() or ord(c) < 32 for c in text)):
        raise ValueError('expected HOST:PORT with a nonzero port')
except ValueError as error:
    print('invalid application target: '+str(error), file=sys.stderr)
    raise SystemExit(2)
PY_TARGET

case "$ROLE" in
  erl)
    command -v erl >/dev/null || { echo "缺 erl"; exit 1; }
    echo "==> erl frontier 网关 → 注册 $APP_TARGET"
    # 宿主网关源码编译（distribute.sh 分发的是 .erl——目标机 OTP 版本自洽）
    erlc -o "$NODE_ROOT/gw_erl" "$NODE_ROOT/gw_erl/parrot_gw.erl" \
      "$NODE_ROOT/gw_erl/parrot_trace.erl" "$NODE_ROOT/gw_erl/parrot_component_runtime.erl" "$NODE_ROOT/gw_erl/parrot_beam_snapshot.erl" "$NODE_ROOT/gw_erl/parrot_beam_guest.erl" "$NODE_ROOT/gw_erl/parrot_beam_host.erl"
    # 业务 beam 同理（OTP 版本对齐——容器形态踩过 badfile）
    erlc -o "$NODE_ROOT/erlang" "$NODE_ROOT/erlang/frontier.erl"
    exec env WS_APP_TARGET="$APP_TARGET" PARROT_NODE_ID=erl-gw-1 erl -noshell -pa "$NODE_ROOT/erlang" "$NODE_ROOT/gw_erl" \
      -eval 'parrot_gw:main([0, "parrot=" ++ os:getenv("WS_APP_TARGET")]).'
    ;;
  ray)
    command -v python3 >/dev/null || { echo "缺 python3"; exit 1; }
    python3 -c "import jieba, ray" 2>/dev/null || { echo "缺 jieba/ray"; exit 1; }
    echo "==> ray tokenizer 网关 → 注册 $APP_TARGET"
    # ray_gw 参数形态：端口0 + parrot= 注册目标
    cd "$NODE_ROOT/gw_py"
    RAY_DIRECTORY="$(mktemp -d /tmp/ws-remote-ray.XXXXXX)"
    exec env PARROT_NODE_ID=ray-gw-1 PYTHONPATH="$NODE_ROOT/gw_py:$NODE_ROOT/python" \
      python3 "$NODE_ROOT/remote-ray.py" "$RAY_DIRECTORY" "$APP_TARGET"
    ;;
  jvm)
    command -v java >/dev/null || { echo "缺 java"; exit 1; }
    # The installed classpath is relative to gw and includes every dependency.
    python3 - "$NODE_ROOT/gw" <<'PY_CP'
from pathlib import Path
import sys
root = Path(sys.argv[1])
entries = (root/'cp.txt').read_text().strip().split(':')
if not entries or any(not name or Path(name).is_absolute() or '..' in Path(name).parts or not (root/name).is_file() for name in entries):
    raise SystemExit('invalid or incomplete portable JVM classpath; redistribute a complete release')
if not (root/'parrot-protocol-jvm-0.1.0.jar').is_file():
    raise SystemExit('missing JVM gateway jar')
PY_CP
    mkdir -p "$DATA_DIR/index"
    echo "==> jvm search 网关（WS_DATA=$DATA_DIR）→ 注册 $APP_TARGET"
    cd "$NODE_ROOT/gw"
    exec env WS_DATA="$DATA_DIR" \
      java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
        parrot.protocol.jvm.ParrotGatewayMain 0 "parrot=$APP_TARGET" "node=jvm-search-1" 7200
    ;;
  *) echo "未知角色 $ROLE（erl|ray|jvm）"; exit 1 ;;
esac
