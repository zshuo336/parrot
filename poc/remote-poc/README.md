# remote-poc —— 远程/集群/联邦 Actor 框架 POC 实验场

**隔离声明**：本目录是独立 workspace（`poc/Cargo.toml` 自带 `[workspace]`，不进根 workspace），不依赖也不修改 `parrot` 正式 crate（只读 path 引用）。全部产物（文档/代码/测试）仅存于此。

## 目录（2026-10-04 调整后）

```
poc/
├── Mx-poc/                  # M1-M6 语义 POC 汇总目录（见下方说明）
│   ├── core-lab/            # 核心升级六项 POC（POC1-POC6，独立 workspace，14 测试）
│   │   ├── derive-decouple/ #   POC1: derive 宏反向依赖解耦
│   │   ├── supervision/     #   POC2: 监督执行器 + DeathWatch
│   │   ├── api-neutral/     #   POC3: parrot-api 中立化（"框架的框架"）
│   │   ├── erased-alloc/    #   POC4: 单块内存擦除（一 alloc 模式）
│   │   ├── typed-static/    #   POC5: 静态类型双轨 ActorRef
│   │   └── beam-sched/      #   POC6: BEAM 抢占式调度借鉴
│   ├── src/                 # parrot-remote 最小实现（frame/transport/codec/
│   │                        #   remote_ref/ingress/node/swim）
│   └── tests/               # p1_remote / p2_cluster / p3{,b,c}_interop
├── remote-poc/              # 远程/集群/联邦 POC 汇总目录（本 README 所在）
│   ├── akka-gw/             #   JVM 网关（纯 JDK 实现 Parrot Wire）
│   ├── ray-adapter/         #   Ray(Python) 网关
│   ├── erlang-gw/           #   Erlang/OTP 网关（OTP 29）
│   └── docs/                #   两份实证版技术详细设计
```

## 运行

```bash
# 前置：rust + java(JDK 17+) + python3(装了 ray) + erlang(OTP 25+)
cd poc/Mx-poc
cargo test                 # 全部 13 个测试（自动拉起 JVM/Python/Erlang 子进程）
# 网关源码位于 ../remote-poc/{akka-gw,ray-adapter,erlang-gw}/
cargo test --test p1_remote_poc      # 仅远程层
cargo test --test p2_cluster_poc     # 仅集群层
cargo test --test p3_akka_interop    # 仅 JVM 互通
cargo test --test p3b_ray_interop    # 仅 Ray 互通
cargo test --test p3c_erlang_interop # 仅 Erlang 互通
```

## 验证矩阵（2026-10-03 实测全绿）

- L0 帧四语言一致（Rust/Java/Python/Erlang 逐字节同构，28B 头）
- L1 内存 + 真实 TCP 双形态
- L2 RemoteActorRef = ActorRef trait 实现（位置透明）；ask/tell/超时/错误映射
- 远程消息落到真实 parrot ThreadActorSystem actor
- SWIM：疑罪从有合并 / refute / 间接探测 / suspect→dead / gossip 传播
- Receptionist：注册/订阅推送 + 与远程层联动
- Rust↔Akka(JVM) / Rust↔Ray(Python) / Rust↔Erlang(OTP) 双向 ask + 方言断言 + 错误路径

详见 `remote-poc/docs/TECH_DESIGN_REMOTE_CLUSTER_FEDERATION_POC.md`。
