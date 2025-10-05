# ============================================================================
#  Parrot 节点运行时镜像（多阶段：builder 编译 → runtime 极简运行）
#
#  产物：/usr/local/bin/parrot-node + parrot-probe
#  运行用户：parrot(1000)——非 root
# ============================================================================

FROM rust:1.98-slim AS builder

WORKDIR /build

# 依赖层缓存（先拷 manifest 再拷源码——cargo 依赖不变时跳过重编译）
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY tools/ tools/

# 静态偏好构建（release；目标仅节点 crate 链）
RUN cargo build --release -p parrot-node

# ── runtime：与 builder 同代基系（trixie）——glibc 一致性 ──────────────────
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home parrot

COPY --from=builder /build/target/release/parrot-node /usr/local/bin/parrot-node
COPY --from=builder /build/target/release/parrot-probe /usr/local/bin/parrot-probe

USER parrot
WORKDIR /home/parrot

# 契约：PARROT_BIND（缺省 0.0.0.0:9801）；healthcheck 走二进制内置 TCP 探测
ENV PARROT_BIND=0.0.0.0:9801
EXPOSE 9801

HEALTHCHECK --interval=5s --timeout=3s --start-period=3s --retries=3 \
    CMD ["/usr/local/bin/parrot-node"]

ENTRYPOINT ["/usr/local/bin/parrot-node"]
