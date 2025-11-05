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
export CARGO_BUILD_JOBS=2
HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
APP="$HERE/.."
COMPOSE_FILE="$HERE/docker-compose.yml"

ACTION="${1:-up}"; shift || true
COMPOSE_ARGS=(-f "$COMPOSE_FILE")

request() {
  docker compose "${COMPOSE_ARGS[@]}" exec -T app sh -c '
    url="http://localhost:${WS_WEB_LISTEN_PORT:-8080}$1"
    if command -v curl >/dev/null; then exec curl -fsS --max-time 5 -- "$url";
    else exec wget -T 5 -qO- "$url"; fi' sh "$1"
}

prebuild() {
  # One explicit platform controls both the prebuilt binary and Compose image.
  export WS_PLATFORM="${WS_PLATFORM:-linux/arm64}"
  case "$WS_PLATFORM" in
    linux/arm64) TRIPLE=aarch64-unknown-linux-musl ;;
    linux/amd64) TRIPLE=x86_64-unknown-linux-musl ;;
    *) echo "ERROR: unsupported WS_PLATFORM=$WS_PLATFORM (linux/arm64|linux/amd64)" >&2; return 1 ;;
  esac
  bash "$APP/build.sh" >/dev/null
  bash "$REPO/deploy/images/prep-jvm-libs.sh"
  # 容器目标二进制（宿主交叉编译——musl 静态，免容器内 rust）
  # Cargo tracks all sources, manifests and dependencies, not just main.rs.
  echo "==> zigbuild 交叉编译（${TRIPLE} / ${WS_PLATFORM}）"
  (cd "$REPO" && cargo zigbuild --locked --release -p websearch --target "$TRIPLE") >/dev/null
}

case "$ACTION" in
  up)
    prebuild
    OVERRIDE="$(mktemp "${TMPDIR:-/tmp}/parrot-websearch-override.XXXXXX")"
    ARGV_FILE=""
    trap 'rm -f "$OVERRIDE" "${ARGV_FILE:-}"' EXIT
    # Render a complete resolved model: overlaying ports would append the old
    # mapping and bind the same host port twice when --port changes.
    docker compose -f "$COMPOSE_FILE" config --format json |
      python3 "$HERE/compose_config.py" render "$@" > "$OVERRIDE"
    COMPOSE_ARGS=(-f "$OVERRIDE")
    ARGV_FILE="$(mktemp "${TMPDIR:-/tmp}/parrot-websearch-argv.XXXXXX")"
    python3 "$HERE/compose_config.py" argv "$OVERRIDE" > "$ARGV_FILE"
    APP_ARGS=()
    while IFS= read -r -d '' argument; do APP_ARGS+=("$argument"); done < "$ARGV_FILE"
    docker compose "${COMPOSE_ARGS[@]}" build
    # The actual target-platform binary owns complete CLI validation. Do not
    # launch persistent gateways for invalid or duplicate application options.
    docker compose "${COMPOSE_ARGS[@]}" run --rm --no-deps app --validate-only "${APP_ARGS[@]}"
    docker compose "${COMPOSE_ARGS[@]}" up -d --no-build
    echo "==> 等网关注册及 Web 健康检查（compose.sh logs app）"
    READY_DEADLINE=$((SECONDS + 480))
    READY=false
    while (( SECONDS < READY_DEADLINE )); do
      APP_ID="$(docker compose "${COMPOSE_ARGS[@]}" ps --all --quiet app)"
      if [ -z "$APP_ID" ] || [[ "$APP_ID" == *$'\n'* ]]; then
        echo "ERROR: 当前项目必须有且仅有一个 app 容器" >&2
        exit 1
      fi
      APP_STATE="$(docker inspect --format '{{.State.Running}} {{.State.StartedAt}}' "$APP_ID")"
      read -r APP_RUNNING APP_STARTED <<< "$APP_STATE"
      if [ "$APP_RUNNING" != true ]; then
        docker logs --tail 30 "$APP_ID" >&2 || true
        echo "ERROR: app 在就绪前退出" >&2
        exit 1
      fi
      # Read the complete bounded output before matching; pipefail must not
      # mistake grep -q's early close / producer SIGPIPE for failed readiness.
      APP_LOGS="$(docker logs --since "$APP_STARTED" --tail 200 "$APP_ID" 2>&1)"
      if [[ "$APP_LOGS" == *浏览器打开* ]] && request /stats | python3 "$HERE/compose_config.py" health; then
        READY=true; break
      fi
      sleep 2
    done
    if [ "$READY" != true ]; then
      echo "ERROR: app 未在 480 秒内就绪" >&2
      exit 1
    fi
    printf '%s\n' "$APP_LOGS" | tail -5
    APP_PORT="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["services"]["app"]["environment"]["WS_WEB_LISTEN_PORT"])' "$OVERRIDE")"
    WEB_ADDRESS="$(docker compose "${COMPOSE_ARGS[@]}" port app "$APP_PORT")"
    if [ -z "$WEB_ADDRESS" ]; then
      echo "ERROR: app 没有已发布的 Web 端口" >&2
      exit 1
    fi
    echo "✓ Web: http://${WEB_ADDRESS}（站点 /docs · 分词表 /terms）"
    ;;
  logs)  exec docker compose "${COMPOSE_ARGS[@]}" logs -f "${1:-app}" ;;
  ps)    exec docker compose -f "$COMPOSE_FILE" ps ;;
  stats) request /stats ;;
  search)
    Q="${1:?用法：compose.sh search 关键词}"
    SEARCH_URL="/search?q=$(python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1]))' "$Q")"
    request "$SEARCH_URL"
    ;;
  down)
    # Gateways must remain available for the crawler's final acknowledged flush.
    docker compose -f "$COMPOSE_FILE" stop -t 75 app
    if [ "${1:-}" = "-v" ]; then
      docker compose -f "$COMPOSE_FILE" down -v --remove-orphans
    else
      docker compose -f "$COMPOSE_FILE" down --remove-orphans
    fi
    ;;
  *) echo "未知动作 $ACTION（up|logs|ps|stats|search|down）"; exit 1 ;;
esac
