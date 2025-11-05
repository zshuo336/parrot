# ============================================================================
#  Parrot 节点运行时镜像（多阶段：builder 编译 → runtime 极简运行）
#
#  产物：/usr/local/bin/parrot-node + parrot-probe
#  运行用户：parrot(1000)——非 root
# ============================================================================

FROM rust:1.98-slim AS builder

WORKDIR /build
ENV CARGO_BUILD_JOBS=2

# Cargo resolves every workspace member, including applications.
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY tools/ tools/
COPY apps/ apps/
COPY vendor/ vendor/

# 静态偏好构建（release；目标仅节点 crate 链）
RUN cargo build --locked --release -p parrot-node

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

# 默认仅回环；外部部署必须显式配置 mTLS，或选择 insecure-development。
ENV PARROT_BIND=127.0.0.1:9801
EXPOSE 9801

HEALTHCHECK --interval=5s --timeout=3s --start-period=3s --retries=3 \
    CMD ["env", "PARROT_HEALTHCHECK=1", "/usr/local/bin/parrot-node"]

ENTRYPOINT ["/usr/local/bin/parrot-node"]
