# ============================================================================
#  parrot 网关运行时镜像——Erlang 方言
#
#  职责：erl 运行时 + parrot_gw（框架的 Erlang 网关宿主，源码 COPY 进镜像）
#  app 制品（beam）经 volume 挂载——镜像不含任何 app 代码。
#
#  app compose 引用契约：
#    build:
#      context: ../../..            # 仓库根
#      dockerfile: deploy/images/gw-erl.Dockerfile
#    volumes:
#      - ./erlang:/app/erlang:ro    # app beam 目录（deploy 时 file:// 指此）
#  运行（网关宿主监听 + 可选反拨注册，由 app compose command 指定）：
#    erl -noshell -pa /app/erlang -eval 'parrot_gw:main([...])'
#
#  基镜像加速（registry-1.docker.io 不可达环境）：app compose 传 build-arg
#    args: { REGISTRY_PREFIX: "docker.1ms.run/" }   # 官方缺省空
# ============================================================================
ARG REGISTRY_PREFIX=""
FROM ${REGISTRY_PREFIX}erlang:27-slim

WORKDIR /gw
# 框架网关宿主（协议层——单一真源，随框架升级）
COPY interop/erlang/parrot_gw.erl .
RUN erlc parrot_gw.erl

# app beam 只读挂载点（deploy uri: file:///app/erlang）
RUN mkdir -p /app/erlang
VOLUME ["/app/erlang"]

CMD ["erl", "-noshell", "-pa", "/gw", "-pa", "/app/erlang", "-eval", "parrot_gw:main([\"19871\"])"]
