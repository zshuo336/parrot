#!/usr/bin/env bash
# ============================================================================
#  parrot-mesh：单机多节点模拟环境生成器
#
#  在单台机器上模拟 N 个"各自部署了完整多 actor 运行时"的节点（每容器
#  = parrot-node 全功能节点：真实引擎 + TCP 组网 + admin 工厂）。
#
#  拓扑：每个节点连接全部前序节点，显式建立双向可用的 mesh 链路。
#  使用 compose 服务 DNS；不依赖尚未接通的 gossip 自动组网。
#
#  用法：
#    ./gen-mesh.sh [N]              # 生成 docker-compose.mesh.yml（缺省 N=5）
#    ./gen-mesh.sh 5 up             # 生成并拉起
#    ./gen-mesh.sh 5 probe          # 生成 + 拉起 + 全量验收
#    ./gen-mesh.sh 5 down           # 销毁
# ============================================================================
set -euo pipefail

N="${1:-5}"
ACTION="${2:-gen}"
[[ "$N" =~ ^[1-9][0-9]{0,2}$ ]] && (( N <= 249 )) || { echo "N 必须为 1..249 的十进制整数" >&2; exit 2; }
case "$ACTION" in gen|up|probe|down) ;; *) echo "未知动作：$ACTION（gen|up|probe|down）" >&2; exit 2 ;; esac

HERE="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_FILE="$HERE/docker-compose.mesh.yml"
BASE_PORT="${PARROT_MESH_BASE_PORT:-21800}"     # host 暴露起始端口
NET_CIDR="${PARROT_MESH_NET:-172.28.0.0/16}"
[[ "$BASE_PORT" =~ ^[1-9][0-9]{0,4}$ ]] && (( BASE_PORT <= 65530 )) || { echo "PARROT_MESH_BASE_PORT 必须为 1..65530" >&2; exit 2; }
[[ "$NET_CIDR" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}/[0-9]{1,2}$ ]] || { echo "PARROT_MESH_NET 必须为 IPv4 CIDR" >&2; exit 2; }
IFS='./' read -r octet_a octet_b octet_c octet_d prefix <<< "$NET_CIDR"
for octet in "$octet_a" "$octet_b" "$octet_c" "$octet_d"; do
  (( 10#$octet <= 255 )) || { echo "非法 IPv4 网段" >&2; exit 2; }
done
(( 10#$prefix >= 1 && 10#$prefix <= 30 )) || { echo "网段前缀必须为 1..30" >&2; exit 2; }
network_address=$(( (10#$octet_a << 24) | (10#$octet_b << 16) | (10#$octet_c << 8) | 10#$octet_d ))
network_size=$(( 1 << (32 - 10#$prefix) ))
(( network_address % network_size == 0 && network_size >= N + 4 )) || { echo "网段必须对齐且容纳节点、probe 和网关" >&2; exit 2; }

gen() {
  {
    cat <<EOF
# 由 gen-mesh.sh 生成（N=$N）——勿手改；重新生成：./gen-mesh.sh $N
# 拓扑：每节点向全部前序节点建立链路；端点使用 compose 服务 DNS
name: parrot-mesh-$N

x-common: &common
  image: parrot-node:latest
  build:
    context: ../..
    dockerfile: deploy/Dockerfile
  restart: unless-stopped
  networks: [mesh]

services:
EOF
    for i in $(seq 1 "$N"); do
      SEEDS=""
      for (( peer = 1; peer < i; peer++ )); do
        SEEDS="${SEEDS:+$SEEDS,}parrot-$peer=parrot-$peer:9801"
      done
      cat <<EOF
  parrot-$i:
    <<: *common
    container_name: mesh-parrot-$i
    environment:
      PARROT_NODE_ID: parrot-$i
      PARROT_BIND: 0.0.0.0:9801
      PARROT_SECURITY_MODE: insecure-development
      PARROT_SEEDS: "$SEEDS"
      PARROT_ACTORS: echo,counter,kv,slow
EOF
      if [ "$i" -le 5 ]; then   # 前 5 个暴露 host 端口（调试/探测用）
        cat <<EOF
    ports:
      - "127.0.0.1:$((BASE_PORT + i)):9801"
EOF
      fi
    done
    # probe 目标：全部节点（容器网内互联）
    PROBE_ARGS="
      - --insecure-development"
    for i in $(seq 1 "$N"); do
      PROBE_ARGS="$PROBE_ARGS
      - parrot-$i=parrot-$i:9801"
    done
    cat <<EOF
  probe:
    <<: *common
    container_name: mesh-probe
    profiles: ["probe"]
    entrypoint: ["/usr/local/bin/parrot-probe"]
    command:$PROBE_ARGS
    depends_on:$(for i in $(seq 1 "$N"); do printf "\n      parrot-%s:\n        condition: service_healthy" "$i"; done)
    restart: "no"

networks:
  mesh:
    driver: bridge
    ipam:
      config:
        - subnet: $NET_CIDR
EOF
  } > "$COMPOSE_FILE"
  echo "✓ 生成 $COMPOSE_FILE（$N 节点 + probe；host 端口 $((BASE_PORT+1))-$((BASE_PORT + (N<5?N:5)))）"
}

case "$ACTION" in
  gen)   gen ;;
  up)    gen && docker compose -f "$COMPOSE_FILE" up -d --build ;;
  probe)
    gen
    trap 'result=$?; trap - EXIT; docker compose -f "$COMPOSE_FILE" down || { if [ "$result" -eq 0 ]; then result=1; fi; }; exit "$result"' EXIT
    docker compose -f "$COMPOSE_FILE" up -d --build --wait --wait-timeout 120
    docker compose -f "$COMPOSE_FILE" run --rm probe
    ;;
  down) docker compose -f "$COMPOSE_FILE" down -v --remove-orphans ;;
  *) echo "未知动作：$ACTION（gen|up|probe|down）" >&2; exit 2 ;;
esac
