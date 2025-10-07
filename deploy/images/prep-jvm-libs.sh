#!/usr/bin/env bash
# ============================================================================
#  prep-jvm-libs.sh —— JVM 网关镜像依赖打包（框架通用，app 复用）
#
#  cp.txt 是 ~/.m2 绝对路径清单（无法直接 COPY 进 docker context）。
#  本脚本把全部依赖 jar 汇成 gw-jvm-libs.tar 放到 context 根，供
#  deploy/images/gw-jvm.Dockerfile COPY。
#
#  幂等：cp.txt mtime 不变则跳过。app compose 构建前跑一次即可
#  （websearch 的 compose 用 x-prebuild 语义——脚本内置调用）。
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")/../.."   # 仓库根

CP_FILE="interop/jvm/target/cp.txt"
[ -f "$CP_FILE" ] || { echo "缺 $CP_FILE——先 make build-jvm"; exit 1; }
OUT="gw-jvm-libs.tar"

# 幂等检查
if [ -f "$OUT" ] && [ "$OUT" -nt "$CP_FILE" ]; then
  exit 0
fi

STAGE=$(mktemp -d /tmp/gw-jvm-libs.XXXXXX)
trap 'rm -rf "$STAGE"' EXIT
# 复制成短名（避免绝对路径结构；重名覆盖取最后——m2 无同 basename 冲突实践）
while IFS= read -r jar; do
  [ -f "$jar" ] || { echo "缺依赖 $jar"; exit 1; }
  cp "$jar" "$STAGE/$(basename "$jar")"
done < <(tr ':' '\n' < "$CP_FILE")

tar -C "$STAGE" -czf "$OUT.tmp" . && mv "$OUT.tmp" "$OUT"
echo "✓ $(ls -1 "$STAGE" | wc -l | tr -d ' ') 个依赖 → $OUT（$(du -h "$OUT" | cut -f1)）"
