#!/usr/bin/env bash
# ============================================================================
#  run-mesh.sh —— websearch 跨网络 docker 编排（单机多容器模拟多节点）
#
#  拓扑（bridge 网络 ws-mesh 172.29.0.0/16——容器间真实 TCP，跨"机"语义）：
#    ws-app    172.29.0.2  Rust 应用（--bind :19870 registry 模式 + Web :8080）
#    ws-erl    172.29.0.3  Erlang frontier 网关（反拨注册 → app）
#    ws-ray    172.29.0.4  Ray jieba 分词网关（反拨注册 → app）
#    ws-jvm    172.29.0.5  JVM akka 检索网关（反拨注册 → app）
#
#  与 run.sh 的区别：三网关各自独立容器/独立 IP（非本机 127.0.0.1 进程）；
#  制品经 distribute.sh 打包进各容器 /opt/parrot/websearch（file:// uri 指向
#  容器内路径——admin-v2 Deploy 跨容器远程下发）。
#
#  用法：./run-mesh.sh [--sites N] [--maxdepth D] [--pages P]
#  依赖：docker；本机已 build.sh（制品就绪）
# ============================================================================
set -uo pipefail
cd "$(dirname "$0")/../.."
ROOT="$PWD"
APP="$ROOT/apps/websearch"
NET=ws-mesh
SUBNET=172.29.0.0/16

cleanup() {
  docker rm -f ws-app ws-erl ws-ray ws-jvm 2>/dev/null
  docker network rm "$NET" 2>/dev/null
}
trap cleanup EXIT INT TERM

# ── 0. 制品就绪 ─────────────────────────────────────────────────
bash "$APP/build.sh" >/dev/null
echo "✓ 制品就绪"

# ── 1. 网络 ────────────────────────────────────────────────────
docker network inspect "$NET" >/dev/null 2>&1 || docker network create --subnet "$SUBNET" "$NET"
echo "✓ 网络 $NET（$SUBNET）"

# ── 2. 制品卷（宿主打包一次，三容器共享只读）────────────────────
STAGE=/tmp/ws-mesh-stage
rm -rf "$STAGE"; mkdir -p "$STAGE/jvm/target" "$STAGE/erlang" "$STAGE/python" "$STAGE/gw" "$STAGE/gw_erl" "$STAGE/gw_py"
cp "$APP/jvm/target/websearch-jvm-1.0.0.jar" "$STAGE/jvm/target/"
cp "$APP/erlang/frontier.beam" "$STAGE/erlang/"
cp "$APP/python/tokenizer.py" "$STAGE/python/"
cp interop/jvm/target/parrot-protocol-jvm-0.1.0.jar "$STAGE/gw/"
cp interop/jvm/target/cp.txt "$STAGE/gw/" 2>/dev/null || touch "$STAGE/gw/cp.txt"
cp interop/erlang/parrot_gw.erl "$STAGE/gw_erl/" 2>/dev/null || true
( cd interop/python && tar cf - parrot_protocol) 2>/dev/null | tar xf - -C "$STAGE/gw_py" 2>/dev/null || true
echo "✓ 制品卷 $STAGE"

# erl 网关需编译 parrot_gw（容器内有 erl 运行时——复用 deploy/Dockerfile 基镜像思路，
# 直接用 erlang 官方镜像 + 挂卷编译）
run_erl() {
  docker run -d --name ws-erl --network "$NET" --ip 172.29.0.3 \
    -v "$STAGE":/opt/parrot/websearch:ro erlang:27 \
    sh -c 'cd /tmp && cp -r /opt/parrot/websearch/gw_erl . && erlc /tmp/gw_erl/parrot_gw.erl -o /tmp \
      && erl -noshell -pa /tmp -eval '\''parrot_gw:main([0, "parrot=172.29.0.2:19870"])'\'''
}

run_ray() {
  # ray+jieba 依赖重——宿主 python 起（网络配 host.docker.internal 不通 bridge，
  # 故容器内 pip 装太慢：改用预装镜像 rayproject/ray + pip jieba）
  docker run -d --name ws-ray --network "$NET" --ip 172.29.0.4 \
    -v "$STAGE":/opt/parrot/websearch:ro python:3.11-slim \
    sh -c 'pip install -q jieba ray 2>/dev/null; \
      cd /opt/parrot/websearch/gw_py && cp -r /opt/parrot/websearch/python /tmp/app && \
      env PYTHONPATH=/opt/parrot/websearch/gw_py:/tmp/app \
      python3 -m parrot_protocol.ray_gw 0 "parrot=172.29.0.2:19870"'
}

run_jvm() {
  docker run -d --name ws-jvm --network "$NET" --ip 172.29.0.5 \
    -v "$STAGE":/opt/parrot/websearch:ro \
    -v /tmp/ws-mesh-data:/var/lib/websearch eclipse-temurin:17-jdk \
    sh -c 'mkdir -p /var/lib/websearch/index && cd /opt/parrot/websearch/gw && \
      env WS_DATA=/var/lib/websearch \
      java -cp "parrot-protocol-jvm-0.1.0.jar:$(cat cp.txt)" \
        parrot.protocol.jvm.ParrotGatewayMain 0 "parrot=172.29.0.2:19870" "node=jvm-search-1" 7200'
}

echo "==> [1/4] 应用容器先行（registry 监听）"
docker rm -f ws-app 2>/dev/null
docker run -d --name ws-app --network "$NET" --ip 172.29.0.2 \
  -v "$ROOT/target/release/websearch":/usr/local/bin/websearch:ro \
  -v /tmp/ws-mesh-app-data:/data \
  rust:1-slim /usr/local/bin/websearch \
  --bind 0.0.0.0:19870 --wait 120 \
  --node-root /opt/parrot/websearch \
  --port 8080 --data /data "$@"
docker logs -f ws-app 2>&1 | grep -m1 "组网模式" || true

echo "==> [2/4] 三网关容器（反拨注册）"
run_erl
run_jvm
run_ray

echo "==> [3/4] 等注册与爬取启动（≤150s）"
for i in $(seq 1 150); do
  if docker logs ws-app 2>&1 | grep -q "网关已全部注册"; then echo "    ✓ 注册完成"; break; fi
  sleep 1
done
docker logs ws-app 2>&1 | tail -5

echo "==> [4/4] Web 验证（宿主 8080 → 容器）"
docker exec ws-app sh -c 'command -v curl >/dev/null || echo "(容器无 curl——用 docker logs 看进度)"'
echo "    应用日志：docker logs -f ws-app"
echo "    检索验证：docker exec ws-app /usr/local/bin/websearch --serve-only --bind 0.0.0.0:19871 --port 8081 --data /data"
echo "（Ctrl-C 全部回收）"
wait
