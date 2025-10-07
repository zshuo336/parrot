# ============================================================================
#  parrot 网关运行时镜像——JVM/Akka 方言
#
#  职责：JDK17 + parrot-protocol-jvm（框架网关宿主 + akka 依赖，COPY 进镜像）
#  app 制品（业务 jar）经 volume 挂载——镜像不含任何 app 代码。
#
#  app compose 引用契约：
#    build:
#      context: ../../..
#      dockerfile: deploy/images/gw-jvm.Dockerfile
#    volumes:
#      - ./jvm/target/app.jar:/app/app.jar:ro   # app 业务 jar
#      - ws-jvm-data:/app/data                  # 索引落盘（WS_DATA）
#  运行（由 app compose command 指定 main 类/jar/node 名）：
#    java -cp "/gw/parrot-protocol-jvm-0.1.0.jar:/gw/libs/*:/app/app.jar" \
#      parrot.protocol.jvm.ParrotGatewayMain <port> parrot=<注册目标> node=<id> <ttl>
#
#  注意：宿主 jar 需先构建（mvn -f interop/jvm）；镜像构建时若缺失会失败——
#  先 make build-jvm。
#
#  基镜像加速：同 gw-erl.Dockerfile（REGISTRY_PREFIX build-arg）。
# ============================================================================
ARG REGISTRY_PREFIX=""
FROM ${REGISTRY_PREFIX}eclipse-temurin:21-jdk

WORKDIR /gw
# 框架网关宿主 fat（协议层——单一真源）
COPY interop/jvm/target/parrot-protocol-jvm-0.1.0.jar .
# akka/scala 运行时依赖（cp.txt = 绝对路径清单——转 COPY 需相对化，改用
# maven dependency 副本方案：构建期由 scripts 保证 libs/ 就绪）
# 说明：libs/ 由本 Dockerfile 同目录 libs-prep.sh 在 context 外准备不可行，
# 故采用 build-arg 传入宿主机 m2 坐标集合的替代——见下方 RUN。
ARG LIBS_TAR=gw-jvm-libs.tar
COPY ${LIBS_TAR} .
RUN mkdir -p libs && tar xzf ${LIBS_TAR} -C libs && rm ${LIBS_TAR}

# app jar 只读挂载点（deploy uri: file:///app/app.jar）
RUN mkdir -p /app/data
VOLUME ["/app/data"]

CMD ["java", "-cp", "/gw/parrot-protocol-jvm-0.1.0.jar:/gw/libs/*", "parrot.protocol.jvm.ParrotGatewayMain", "19872"]
