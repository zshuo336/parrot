#!/usr/bin/env bash
# ============================================================================
#  compose.sh —— websearch 多节点模拟编排（docker compose 包装）
#
#  用法：
#    ./compose.sh up   [--sites N --maxdepth D ...]   拉起（爬取参数透传 app）
#    ./compose.sh logs [svc]                          跟踪日志（缺省 app）
#    ./compose.sh ps                                  状态
#    ./compose.sh stats                               Web /stats 快照
#    ./compose.sh search 关键词                        命令行检索验证
#    ./compose.sh down                                销毁（-v 连卷）
#
#  首次 up 前自动：build.sh（app 制品）+ prep-jvm-libs.sh（JVM 镜像依赖包）
# ============================================================================
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
APP="$HERE/.."
COMPOSE_FILE="$HERE/docker-compose.yml"

ACTION="${1:-up}"; shift || true

prebuild() {
  bash "$APP/build.sh" >/dev/null
  bash "$REPO/deploy/images/prep-jvm-libs.sh"
  # 容器目标二进制（宿主交叉编译——musl 静态，免容器内 rust）
  if [ ! -f "$REPO/target/aarch64-unknown-linux-musl/release/websearch" ] \
     || [ "$REPO/apps/websearch/src/main.rs" -nt "$REPO/target/aarch64-unknown-linux-musl/release/websearch" ]; then
    echo "==> zigbuild 交叉编译（aarch64-unknown-linux-musl）"
    (cd "$REPO" && cargo zigbuild --release -p websearch --target aarch64-unknown-linux-musl) >/dev/null
  fi
}

case "$ACTION" in
  up)
    prebuild
    docker compose -f "$COMPOSE_FILE" up -d --build
    echo "==> 等三网关注册 + 爬取启动（docker logs -f ws-app）"
    for i in $(seq 1 240); do
      if docker logs ws-app 2>&1 | grep -q "浏览器打开"; then break; fi
      sleep 2
    done
    docker logs ws-app 2>&1 | tail -5
    echo "✓ Web: http://localhost:8080（站点 /docs · 分词表 /terms）"
    ;;
  logs)  exec docker logs -f "${1:-ws-app}" ;;
  ps)    exec docker compose -f "$COMPOSE_FILE" ps ;;
  stats) exec docker exec ws-app sh -c 'command -v curl >/dev/null && curl -s http://localhost:8080/stats || wget -qO- http://localhost:8080/stats' ;;
  search)
    Q="${1:?用法：compose.sh search 关键词}"
    exec docker exec ws-app sh -c "curl -s 'http://localhost:8080/search?q=$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))" "$Q")'" 2>/dev/null \
      || docker exec ws-app sh -c "wget -qO- 'http://localhost:8080/search?q=$Q'"
    ;;
  down)
    if [ "${1:-}" = "-v" ]; then
      docker compose -f "$COMPOSE_FILE" down -v --remove-orphans
    else
      docker compose -f "$COMPOSE_FILE" down --remove-orphans
    fi
    ;;
  *) echo "未知动作 $ACTION（up|logs|ps|stats|search|down）"; exit 1 ;;
esac
