# ============================================================================
#  parrot 网关运行时镜像——Ray/Python 方言
#
#  职责：python3 + ray + jieba（分词依赖为 websearch/crawler-lab 共用形态；
#  其他 app 应单独锁定并验证附加依赖）+ parrot_protocol（框架协议层）
#  app 制品（py 模块）经 volume 挂载——镜像不含任何 app 代码。
#
#  app compose 引用契约：
#    build:
#      context: ../../..
#      dockerfile: deploy/images/gw-ray.Dockerfile
#    init: true
#    volumes:
#      - ./python:/app/python:ro   # app 模块目录（ray working_dir）
#  直接 docker run 时必须加 --init，以回收崩溃后成为孤儿的 Ray 子进程。
#  运行（由 app compose command 指定）：
#    env PYTHONPATH=/gw:/app/python python3 -m parrot_protocol.ray_gw <port> [parrot=...]
#
#  基镜像加速：同 gw-erl.Dockerfile（REGISTRY_PREFIX build-arg）。
# ============================================================================
ARG REGISTRY_PREFIX=""
FROM ${REGISTRY_PREFIX}python:3.11-slim

COPY deploy/images/requirements-ray-build.lock deploy/images/requirements-ray.lock \
     deploy/images/install-ray.sh /opt/parrot-deps/
RUN sh /opt/parrot-deps/install-ray.sh

WORKDIR /gw
# 框架协议层（单一真源）
COPY interop/python/parrot_protocol /gw/parrot_protocol

# app 模块只读挂载点（deploy uri: file:///app/python）
RUN mkdir -p /app/python
VOLUME ["/app/python"]

CMD ["python3", "-m", "parrot_protocol.ray_gw", "19873"]
