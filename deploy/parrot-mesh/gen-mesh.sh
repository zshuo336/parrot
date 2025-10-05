#!/usr/bin/env bash
# ============================================================================
#  parrot-mesh：单机多节点模拟环境生成器
#
#  在单台机器上模拟 N 个"各自部署了完整多 actor 运行时"的节点（每容器
#  = parrot-node 全功能节点：真实引擎 + TCP 组网 + admin 工厂）。
#
#  拓扑：链式种子（i 连 i-1）——链式最坏路径验证路由；全连由 gossip 收敛。
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
N=$((N)); [ "$N" -ge 1 ] || { echo "N 必须 >= 1" >&2; exit 2; }

HERE="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_FILE="$HERE/docker-compose.mesh.yml"
BASE_PORT="${PARROT_MESH_BASE_PORT:-21800}"     # host 暴露起始端口
NET_CIDR="${PARROT_MESH_NET:-172.28.0.0/16}"    # compose 网段（IP 稳定性）

gen() {
  {
    cat <<EOF
# 由 gen-mesh.sh 生成（N=$N）——勿手改；重新生成：./gen-mesh.sh $N
# 拓扑：链式种子 parrot-i → parrot-(i-1)；mesh 网络 IP 固定（$NET_CIDR 段）
name: parrot-mesh-$N

x-common: &common
  image: parrot-node:latest
  build:
    context: ../..
    dockerfile: deploy/Dockerfile
  restart: unless-stopped
  networks:
    mesh:
      # 固定 IP（4 起——.1/.2 网关/DNS 保留）：探测与断言确定性
      ipv4_address: 172.28.0.$((N + 4))

services:
EOF
    for i in $(seq 1 "$N"); do
      IP=$((3 + i))
      SEEDS=""
      if [ "$i" -gt 1 ]; then
        PREV=$((i - 1))
        SEEDS="parrot-$PREV=172.28.0.$((3 + PREV)):9801"
      fi
      cat <<EOF
  parrot-$i:
    <<: *common
    container_name: mesh-parrot-$i
    environment:
      PARROT_NODE_ID: parrot-$i
      PARROT_BIND: 0.0.0.0:9801
      PARROT_SEEDS: "$SEEDS"
      PARROT_ACTORS: echo,counter,kv,slow
    networks:
      mesh:
        ipv4_address: 172.28.0.$IP
EOF
      if [ "$i" -le 5 ]; then   # 前 5 个暴露 host 端口（调试/探测用）
        cat <<EOF
    ports:
      - "$((BASE_PORT + i)):9801"
EOF
      fi
    done
    # probe 目标：全部节点（容器网内互联）
    PROBE_ARGS=""
    for i in $(seq 1 "$N"); do
      PROBE_ARGS="$PROBE_ARGS
      - parrot-$i=172.28.0.$((3 + i)):9801"
    done
    cat <<EOF
  probe:
    <<: *common
    container_name: mesh-probe
    profiles: ["probe"]
    entrypoint: ["/usr/local/bin/parrot-probe"]
    command:$PROBE_ARGS
    depends_on:$(for i in $(seq 1 "$N"); do printf "\n      - parrot-%s" "$i"; done)
    restart: "no"
    networks:
      mesh:
        ipv4_address: 172.28.0.$((N + 5))

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
  probe) gen && docker compose -f "$COMPOSE_FILE" up -d --build \
            && docker compose -f "$COMPOSE_FILE" run --rm probe \
            && docker compose -f "$COMPOSE_FILE" down ;;
  down)  docker compose -f "$COMPOSE_FILE" down -v --remove-orphans \
            && rm -f "$COMPOSE_FILE" ;;
  *) echo "未知动作：$ACTION（gen|up|probe|down）" >&2; exit 2 ;;
esac
