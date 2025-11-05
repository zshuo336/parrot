# Remote POC：远程调用与跨语言实验

Rust 实验代码在 `poc/Mx-poc`，自带 `[workspace]`，引用 `../../crates/parrot*`。
它不属于根 Cargo workspace；根 workspace 测试通过不能代替这里的测试。
`poc/Mx-poc/core-lab` 又是独立 workspace，必须单独运行。

本目录保存历史 POC 的三种网关，不是 `interop/` 的生产协议实现。
历史 `akka-gw/AkkaGw.java` 是 **纯 JDK 服务**，没有使用 Akka actor；
Ray 版本实际调用私有本地 Ray runtime 中的 worker；Erlang 版本在一个 OTP gen_server
中串行执行业务。Rust 测试另外通过 p4a 验证 Parrot thread/Actix 双引擎。

从仓库根运行（JDK 17+、Erlang、Python 3 + Ray、Rust）：

```bash
cargo test --locked --manifest-path poc/Mx-poc/Cargo.toml --no-fail-fast
cargo clippy --locked --manifest-path poc/Mx-poc/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path poc/Mx-poc/Cargo.toml --all -- --check
cargo run --locked --manifest-path poc/Mx-poc/Cargo.toml --example cluster
python3 poc/remote-poc/test_gateway_contracts.py -v
cargo test --locked --manifest-path poc/Mx-poc/core-lab/Cargo.toml --workspace
bash scripts/check_rust_dependencies.sh
```

Rust 网关测试每次在私有目录构建 JVM/Erlang，绑定 loopback 动态端口。
`owned_gateway.py` 复用共享进程 guardian；正常退出、panic、启动取消或管理进程消失
均回收本次拥有的进程组，不按名称杀进程，不使用预先存在的 class/beam。
Ray 强制 `address='local'` 并使用私有 `_temp_dir`，不连接环境中的 RAY_ADDRESS。
Python 边界测试还检查进程创建身份、实际 Ray 子进程和无关进程存活。

协议保留历史布局：4 字节 LE 长度前缀，body 中 28 字节固定字段加 path/key/payload；
body 最大 1 MiB。只接受 version=1、flags=0、reserved=0 和已知帧类型，字符串严格 UTF-8。
这套布局不等同于正式 Wire1，不能直接替代 `interop/` 网关。
Rust `FrameSender::send` 在等待队列容量前校验帧类型与完整 body 大小；内存和 TCP 路径一致。
非法本地输入立即返回错误，不交付给对端、不占队列槽位、不关闭已有连接；精确 1 MiB 的合法帧仍可传输。
这个上限包含固定头、path、type key 的 UTF-8 字节和 payload；这里没有正式 Wire1 的 HELLO/ACK 协商。

业务断言：

- Rust 发 ASK，分别验证 JVM `Ping+1 / Add(a*10+b)`、Ray `Ping+2 / Add(a+b+1000)`、
  Erlang `Ping+3 / Add(a+b+10000)`；u64 运算按 64 位回绕。
- `Probe` 触发网关向 Rust actor 发 Ping(100)，只有收到正确类型及长度的 Pong 后才完成原始请求。
  Rust 同时记录实际 actor 调用；无需定时 sleep 或日志文本推断成功。
- 有效 TELL 执行后才递增 `TellCount`；同连接后续 ASK 是处理 barrier，非法 TELL 不增加计数。
- JVM 的 `Relay` / `RelayTell` 经 Rust hub 到 Erlang，再用目标端实际结果完成原请求。
  各出站连接持有自己的 callback registry；路径路由按 segment 匹配最长前缀。
- 单次 write 中的 32 帧必须全部完成；还有分片、畸形长度/flags/UTF-8、错误反向回复及进程回收测试。

这些网关是单连接实验服务，path 是路由元数据，网关内部没有多 actor 路径注册表；STOP
不提供网关 actor 生命周期协议。反向请求表最多 128 项，JVM/Ray 未完成项随连接退出清理，
Erlang 另有 5 秒期限。这些边界不等于生产就绪，也不能作为 WAN、鉴权或性能验收。

第225批的集群 POC 使用实际收发循环和关联 ACK：直接链路分区时经 helper 探测，helper
必须收到目标回复才能报告成功。bus 每节点队列最多64帧，单节点最多128个未完成探测、
32个 helper 任务；超时预算由请求方传递。runtime 的 Drop 请求清理，shutdown 等待
接收循环和 helper 任务结束。成员按 incarnation 和状态等级单调合并，自身反驳超过
收到的 incarnation，旧超时不能改变新 incarnation 的状态。

Receptionist 返回带 revision 的当前快照：初次 recv 包括现有服务；慢消费者合并中间
版本并保留最新状态，不提供每次历史变更的日志。register/unregister 返回是否实际改变；
remove_node 撤销该节点所有服务；重复注册不增版本。订阅 Drop 回收观察者及空 key。
示例 `poc/Mx-poc/examples/cluster.rs` 实际执行直接失败、间接成功、完整分区失败和服务撤销。
这是进程内协议实验，调用方显式安排 probe/gossip 和 Suspect/Dead 决策；没有周期探测、
自动 suspicion 定时器或真实 WAN/鉴权认证，不能当作生产集群认证。

第226批已修复 core-lab 的 beam-sched：真实预算轮转、消息内可恢复处理、高优插入及低优
防饥饿、每actor串行、回调不持全局锁、死亡释放heap与队列，以及shutdown唤醒并join worker。
可运行 `cargo run --locked --manifest-path poc/Mx-poc/core-lab/Cargo.toml -p beam-sched --example resumable`，
观察同一actor的高优查询在求和完成前读取进度。同步任意Rust回调仍不能被指令级抢占；
消息内部让出需要实现可恢复step并遵守ReductionBudget，完整说明见core-lab设计文档第6节。

第227批修复 erased-alloc：标准组合布局、panic展开仍释放信封块、实际双层装箱基线，
线程局部计数覆盖ZST/过度对齐/错类型/跨线程所有权。普通Box擦除不额外分配；此独立POC
未接入生产mailbox，也不代表所有消息的性能改善。验证命令：
`cargo +nightly miri test --locked --manifest-path poc/Mx-poc/core-lab/Cargo.toml -p erased-alloc`
（设置 `MIRIFLAGS=-Zmiri-strict-provenance`）。

第228批修复 supervision：独占工厂/worker，实际重启、限额退避、目标及generation绑定的
终态watch、Escalate停整个受监督集合、有限邮箱、排队ask期限与shutdown join。
`cargo run --locked --manifest-path poc/Mx-poc/core-lab/Cargo.toml -p supervision --example restart`
实际演示状态重建与兄弟退休。工厂初始化是异步的；spawn只确认准入，ask确认业务就绪。

第229批修复 typed-static：静态/动态共享owned worker和路径、有限准入、ask期限、panic
退休及shutdown join。实际 `typed_actor!` 宏让Add/Get/Describe的类型化port共享一个实例，
不再用两个spawn冒充共享状态；具体消息不经过Any。运行
`cargo run --locked --manifest-path poc/Mx-poc/core-lab/Cargo.toml -p typed-static --example protocols`。
旧 `#[typed_actor]` 属性语法没有实现；当前是上面的函数式宏，尚未接入生产后端。

第230批修复 api-neutral/derive-decouple：保留泛型/where、接受dyn runtime、配置重命名crate路径；
可选生命周期和拥有式工厂注册实际接线。新增core-engines提供每actor独立线程与共享reduction
两个真实后端。运行 `cargo run --locked --manifest-path poc/Mx-poc/core-lab/Cargo.toml -p derive-decouple --example neutral`。
注册token退出撤销具名工厂；注册不是全局runtime目录。当前独立workspace为7个crate。

待继续修复和验证：core-lab的生产接线、异步/local接口、远程与性能边界，以及
`bench/actix-bench` 的业务正确性和测量窗口。
旧 2026-10-03 “全绿”陈述不能证明这些能力。

本轮证据和范围见 `docs/review/independent-2026-10-08/standalone-poc-224/VERIFICATION.md`。
集群修复证据见 `docs/review/independent-2026-10-08/poc-cluster-225/VERIFICATION.md`。
