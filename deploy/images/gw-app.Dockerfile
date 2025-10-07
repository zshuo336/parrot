# ============================================================================
#  parrot 应用主程序镜像——Rust 应用通用形态
#
#  职责：极简运行时 + app 二进制（构建期 COPY——release 产物宿主机 cargo
#  构建后放入 context；镜像不做 cargo build，保持秒级构建）。
#  适用：websearch / crawler-lab 等 Rust 主程序（编排器形态）。
#
#  app compose 引用契约：
#    build:
#      context: ../../..          # 仓库根（能取到 target/release/<bin>）
#      dockerfile: apps/<app>/deploy/Dockerfile
#  app 自己的 Dockerfile 内容（一行 include 语义）：
#    FROM parrot-gw-app:latest    # 或 build 指向本文件 + build-arg BIN
#
#  二进制形态（三选一，按存在性优先）：
#    1. target/<triple>/release/<bin>   宿主交叉编译产物（cargo zigbuild——推荐：
#                                       免容器内装 rust，秒级构建）
#    2. target/release/<bin>            容器内同平台构建产物
#  ARG TRIPLE 传入目标三元组（aarch64-unknown-linux-musl 等；空 = 本平台）。
#
#  基镜像加速：同 gw-erl.Dockerfile（REGISTRY_PREFIX build-arg）。
# ============================================================================
ARG REGISTRY_PREFIX=""
FROM ${REGISTRY_PREFIX}debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# ARG 由 app Dockerfile 传入（二进制名 + 目标三元组）
ARG BIN=websearch
ARG TRIPLE=aarch64-unknown-linux-musl
COPY target/${TRIPLE}/release/${BIN} /usr/local/bin/app
ENTRYPOINT ["/usr/local/bin/app"]
