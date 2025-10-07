# ============================================================================
#  parrot 网关运行时镜像——Ray/Python 方言
#
#  职责：python3 + ray + jieba（分词依赖为 websearch/crawler-lab 共用形态；
#  其他 app 可 COPY 自带 requirements 覆盖）+ parrot_protocol（框架协议层）
#  app 制品（py 模块）经 volume 挂载——镜像不含任何 app 代码。
#
#  app compose 引用契约：
#    build:
#      context: ../../..
#      dockerfile: deploy/images/gw-ray.Dockerfile
#    volumes:
#      - ./python:/app/python:ro   # app 模块目录（ray working_dir）
#  运行（由 app compose command 指定）：
#    env PYTHONPATH=/gw:/app/python python3 -m parrot_protocol.ray_gw <port> [parrot=...]
#
#  基镜像加速：同 gw-erl.Dockerfile（REGISTRY_PREFIX build-arg）。
# ============================================================================
ARG REGISTRY_PREFIX=""
FROM ${REGISTRY_PREFIX}python:3.11-slim

RUN pip install --no-cache-dir 'ray>=2.9' jieba

WORKDIR /gw
# 框架协议层（单一真源）
COPY interop/python/parrot_protocol /gw/parrot_protocol

# app 模块只读挂载点（deploy uri: file:///app/python）
RUN mkdir -p /app/python
VOLUME ["/app/python"]

CMD ["python3", "-m", "parrot_protocol.ray_gw", "19873"]
