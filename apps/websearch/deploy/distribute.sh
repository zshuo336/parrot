#!/usr/bin/env bash
# ============================================================================
#  distribute.sh —— websearch 制品预分发（跨机部署第一步）
#
#  file:// 制品语义：uri 是【目标网关节点本地路径】。跨机部署时先把
#  app 制品分发到各节点同一目录（NODE_ROOT，默认 /opt/parrot/websearch），
#  再由 start-remote.sh / run-mesh.sh 拉起。
#
#  用法：./distribute.sh user@host1[,user@host2,...] [--root /opt/parrot/websearch]
#  依赖：ssh/scp（免密或 agent）；目标机无需 rust 工具链——只传产物
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")/../../.."   # 仓库根（apps/websearch/deploy → 上三级）
ROOT="$PWD"

HOSTS="${1:?用法：$0 user@host1[,user@host2,...] [--root DIR]}"
shift || true
NODE_ROOT="/opt/parrot/websearch"
while [ $# -gt 0 ]; do
  case "$1" in
    --root) NODE_ROOT="$2"; shift 2 ;;
    *) echo "未知参数 $1"; exit 1 ;;
  esac
done

APP="$ROOT/apps/websearch"

echo "==> [1/4] 本地制品就位（缺失则构建）"
bash "$APP/build.sh" >/dev/null
[ -f "$APP/jvm/target/websearch-jvm-1.0.0.jar" ] || { echo "缺 JVM jar"; exit 1; }
[ -f "$APP/erlang/frontier.beam" ] || { echo "缺 beam"; exit 1; }
[ -f "$APP/python/tokenizer.py" ] || { echo "缺 tokenizer.py"; exit 1; }
echo "    ✓ jar + beam + py 就绪"

# 网关宿主 jar（JVM 节点需要：parrot-protocol-jvm + 依赖 cp.txt）
GW_JAR="interop/jvm/target/parrot-protocol-jvm-0.1.0.jar"
[ -f "$GW_JAR" ] || { echo "缺网关宿主 jar——先 make build-jvm"; exit 1; }
echo "    ✓ 网关宿主 jar 就绪"

echo "==> [2/4] 打包（tar 流——制品 + 三方言网关宿主）"
STAGE=$(mktemp -d /tmp/ws-dist.XXXXXX)
mkdir -p "$STAGE/jvm/target" "$STAGE/erlang" "$STAGE/python" \
         "$STAGE/gw" "$STAGE/gw_erl" "$STAGE/gw_py"
cp "$APP/jvm/target/websearch-jvm-1.0.0.jar" "$STAGE/jvm/target/"
cp "$APP/erlang/frontier.beam" "$APP/erlang/frontier.erl" "$STAGE/erlang/"
cp "$APP/python/tokenizer.py" "$STAGE/python/"
cp "$GW_JAR" "$STAGE/gw/"
cp "interop/jvm/target/cp.txt" "$STAGE/gw/" 2>/dev/null || true
# erl/ray 网关宿主源码（远程节点无仓库——连同协议层一起分发）
cp interop/erlang/parrot_gw.erl "$STAGE/gw_erl/" 2>/dev/null || true
( cd interop/python && tar cf - parrot_protocol) | tar xf - -C "$STAGE/gw_py" 2>/dev/null || true
TARBALL="$STAGE.tar.gz"
tar -C "$STAGE" -czf "$TARBALL" .
echo "    ✓ $(du -h "$TARBALL" | cut -f1)"

echo "==> [3/4] 分发到节点（root=$NODE_ROOT）"
IFS=',' read -ra HOST_ARR <<< "$HOSTS"
for h in "${HOST_ARR[@]}"; do
  echo "    → $h"
  ssh "$h" "mkdir -p $NODE_ROOT"
  scp -q "$TARBALL" "$h:/tmp/ws-dist.tar.gz"
  ssh "$h" "tar -xzf /tmp/ws-dist.tar.gz -C $NODE_ROOT && rm /tmp/ws-dist.tar.gz"
done

echo "==> [4/4] 验证"
for h in "${HOST_ARR[@]}"; do
  ok=$(ssh "$h" "ls $NODE_ROOT/erlang/frontier.beam $NODE_ROOT/python/tokenizer.py $NODE_ROOT/jvm/target/websearch-jvm-1.0.0.jar $NODE_ROOT/gw/parrot-protocol-jvm-0.1.0.jar 2>/dev/null | wc -l")
  echo "    $h: $ok/4 制品就位"
  [ "$ok" -eq 4 ] || { echo "    ⚠ $h 制品不全"; exit 1; }
done

rm -rf "$STAGE" "$TARBALL"
echo "✓ 分发完成——各节点 $NODE_ROOT 同构。下一步：start-remote.sh 拉网关 / run-mesh.sh docker 跨网"
