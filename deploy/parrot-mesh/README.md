# 本机 mesh 部署与验收

mesh 前五个节点及 parrot-stack 的宿主发布端口只绑定 `127.0.0.1`，供本机调试。容器内仍监听 `0.0.0.0:9801`，保持服务 DNS 和节点互联可用。需要远程访问时，应显式修改部署配置并配置受控网络及认证；loopback 发布不等于容器网内协议已经鉴权。

运行 `./gen-mesh.sh 5 probe` 生成五节点配置、构建并启动容器、最多等待 120 秒通过协议健康检查、执行 `parrot-probe`，最后关闭容器。验收失败也执行关闭并保留失败退出码。需要支持 `up --wait` 的 Docker Compose v2。健康检查仅证明节点本地协议就绪，P2 另外核验组网。`gen` 只生成配置；`up` 保留运行；`down` 停止并删除卷，保留生成的配置文件。

每个节点向全部前序节点配置种子，建立可双向收发的 TCP 链路。端点使用 compose 服务 DNS，支持自定义 `PARROT_MESH_NET`。这套示例验证显式 mesh 连接及单个入口的转发；不声称链式种子会自动通过 gossip 收敛，也不证明任意多级中继、节点重启恢复或网络分区一致性。节点数支持 1–249，实际资源容量须另行测量；密集 mesh 链路数随节点数平方增长。

`parrot-probe node-a=host:port node-b=host:port [--smoke | --full]` 严格解析所有参数。无参数、未知选项、重复身份、无效地址或冲突模式返回 2；解析后的连接、协议、业务、清理失败返回 1；全部所选检查及清理完成才返回 0。DNS 解析共享五秒截止时间，连接每目标共五秒并为候选地址分配机会；每项检查最多十五秒。单节点运行明确标注 P2 不适用。

- P1：所有目标的内置 echo，首节点的内置 slow，以及独立创建的 counter/kv 工厂实例。
- P2：相邻节点双向以及首尾双向的实际转发。独立 socket 只连接入口节点，完整目标 URI 发给入口；不跟随路由提示另拨目标。未组网但各自能直连的节点必须失败。覆盖的是这些选定路径，不是所有节点对的穷举。
- P3：远程创建 echo，并核验业务返回。
- P4：先确认 counter 可调用，收到管理停止回执后，再要求线上的 ActorNotFound。路由不可达、断连和超时均不能代替这个回执。
- P5：128 个并发请求分别核对关联返回，再检查专用服务端 counter 的最终增量为 128。它不证明崩溃恢复场景的持久恰好一次语义。
- P6：同一连接先发送慢 ASK 再发送 echo，要求 echo 的回执先到，并校验两个关联号和业务值。不使用固定睡眠猜测入队时刻，也不将其解释为所有调度负载下的公平性保证。
- P7：100 次预热、500 次 echo RTT 采样；p50/p99 只用于观测。十五秒场景截止时间仍是运行安全上限，慢链路可因此验收失败，不能把该结果解释为固定性能门槛的测量。

`--smoke` 执行 P1/P2；默认 `--full` 执行 P1–P7。探针跟踪自己创建的 actor，在成功、业务失败或 panic 后逐一尝试停止；清理失败使整个验收失败。管理请求超时仍可能已经产生服务端副作用，不能据客户端超时推断远端取消。传输身份由目标集合稳定派生，避免同一组重复探活每次新增成员身份；业务 actor 路径独立，支持同时运行多个探针。

本目录仍使用开发 TCP 与未鉴权管理协议，不能将通过探针视为生产安全、持久恢复、容器镜像安全或全项目质量验收。源码测试为 `cargo test --locked -p parrot-node --tests`；生成器契约为 `python3 scripts/tests/test_mesh_generator.py`。真实容器回归入口为根目录 `make test-container`：重新构建deploy/Dockerfile，在不发布宿主端口的独立网络中启动三个UID1000节点，验证镜像HEALTHCHECK、已组网两节点P1–P7、未组网节点P1可达但P2必须RouteUnreachable，以及SIGTERM后退出0；成功或失败都会清理本次容器/网络。`python3 scripts/verify_container.py --image <tag> --skip-build`仅用于明确复用已有镜像，输出镜像ID。GitHub Actions的container job运行相同入口；本机arm64实跑与CI amd64结果分别记录，不互相代替。

示例 Compose 明确使用 `PARROT_SECURITY_MODE=insecure-development`，探针使用 `--insecure-development`。镜像默认仅回环，生产环境必须配置证书和外部监听；认证探针可用 `--config=/path/to/operator.toml`。详见 [远程安全配置](../../docs/REMOTE_SECURITY.md)。
