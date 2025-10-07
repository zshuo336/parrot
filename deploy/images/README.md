# deploy/images —— 框架通用网关运行时镜像

app 的 docker compose **引用**这些镜像（`build.context` = 仓库根 +
`dockerfile` = 本目录路径），自身目录保持完备——不复制、不侵入框架。

| 镜像 Dockerfile | 内容 | app 制品挂载点 |
|---|---|---|
| `gw-erl.Dockerfile` | erlang:27 + `parrot_gw`（编译好） | `/app/erlang`（beam 目录） |
| `gw-jvm.Dockerfile` | temurin:17 + `parrot-protocol-jvm` + libs | `/app/app.jar` + `/app/data` |
| `gw-ray.Dockerfile` | python:3.11 + ray + jieba + `parrot_protocol` | `/app/python`（模块目录） |
| `gw-app.Dockerfile` | debian + app 二进制（ARG BIN） | —（构建期 COPY） |

## 约定

1. **镜像 = 运行时 + 框架协议层**；**app 制品 = volume 挂载**（beam/jar/py）。
   app 重新构建不重建镜像；框架协议升级重建镜像即可，全部 app 同时受益。
2. JVM 镜像构建前先跑 `deploy/images/prep-jvm-libs.sh`（把 `~/.m2` 依赖
   汇成 `gw-jvm-libs.tar` 供 COPY——cp.txt 绝对路径不可 COPY）。
3. app deploy uri（admin-v2 `file://`）指**容器内路径**（挂载点），即上表列。

## app 侧使用样例（websearch）

```yaml
services:
  erl-gw:
    build: { context: ../../.., dockerfile: deploy/images/gw-erl.Dockerfile }
    volumes: [ "../../erlang:/app/erlang:ro" ]
    command: ["erl","-noshell","-pa","/gw","-pa","/app/erlang",
              "-eval","parrot_gw:main([0, \"parrot=app:19870\"])"]
```

完整样例见 `apps/websearch/deploy/docker-compose.yml`。
