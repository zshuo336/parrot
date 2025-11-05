# crawler-lab

可复现的合成爬取、词频索引和查询示例：Erlang frontier、Ray 索引、JVM 搜索，以及 Props／显式信任的 Dylib／沙箱 Wasm 业务内核。

```sh
make build-crawler-business build-crawler-components build-ts
apps/crawler-lab/deploy/run-lab.sh
```

三形态测试统一验证当前源码的构建回执，使用回执中的制品路径和预期摘要；缺失或过期时应先重新构建。自定义 target/native 路径及失效规则见[业务制品构建回执](../../docs/CRAWLER_BUSINESS_ARTIFACTS.md)。

需要 Rust、wasm-tools 1.261.0、`wasm32-unknown-unknown` target、Java/Maven、Erlang、Node/npm，以及当前 Python 环境中的 Ray 和 virtualenv。启动器默认 `LAB_MODE=manifest`，使用 `crawler.app.toml`：60 页、depth=2、fanout=3、batch=32。只有显式的 `--pages/--depth/--fanout/--batch` 才覆盖 manifest 业务配置；`--pages 0` 是有效的空工作负载。

```sh
apps/crawler-lab/deploy/run-lab.sh --manifest /absolute/path/custom.app.toml --pages 17
apps/crawler-lab/deploy/run-lab.sh --hub-wasm apps/crawler-lab/wasm/crawler-hub.wasm --hub-sha256 HEX64
apps/crawler-lab/deploy/run-lab.sh --hub-dylib /absolute/path/libcrawler_lab_dylib.dylib --trust-native-sha256 HEX64
```

请将 HEX64 替换为实际制品 SHA-256。原生代码需要操作者显式摘要授权；manifest 自己声明 digest 不授予执行信任。原生依赖也在进程内的信任边界中。Wasm 沿用相同业务协议，网络 I/O 由应用宿主执行。

标准入口使用 `AssemblingContext`、`LocalDeployer`、`ProcessGatewayFactory` 和 `ThreadPropsSpawner`。manifest 中的相对制品路径以该文件所在目录解析，转换为真实的 `file:///` URI；也接受绝对本地文件 URI。所有组件都由标准宿主拥有。业务配置放在 crawler 组件的 `[components.config].settings` JSON 中；`config_overlay` 仅接受框架配置，其优先级高于 CLI bind。

应用要求恰好一个 singleton Parrot 业务组件，默认工厂是 `crawler.business`。组件可改名；宿主通过该组件的 `/user/frontier`、`/user/index`、`/user/search` 三个 source alias 获取 `wired_ref`，使用声明的具体目标。目标组件的 name、路径、依赖、版本和配置均由 manifest 提供。示例使用局域网连接；不提供跨组件持久事务或分布式恢复保证。

TypeScript 查询在业务对账后、teardown 前运行，经应用 front door 路由到声明的搜索组件。其结果必须与 Rust 独立词频模型生成的完整 top-5 一致。启动器只在 TS 实际进程成功后写入 query acknowledgement；应用等待 acknowledgement，完成 teardown 后才输出 PASS。查询失败、超时或中断都不能成功。`--skip-ts` 会明确记录跳过，不算五运行时验证通过。

单独运行 Rust 入口时，默认完成业务后立即回收；外部查询者可以选择有界查询阶段：

```sh
target/debug/crawler-lab --insecure-development --manifest apps/crawler-lab/crawler.app.toml \
  --query-ready /private/run/ready.json --query-done /private/run/done.txt --wait 60
```

协调文件必须不存在且位于本次运行的私有目录。ready 是原子发布的 JSON，包含 endpoint、搜索 path 和独立 expected_hits。查询者核对结果后原子写入内容恰为 `ok\n` 的 done 普通文件；无回执或错误回执会导致失败及清理。示例显式使用开发用明文 TCP。

`LAB_MODE=direct|registry` 保留旧网关接入／故障注入入口。它们仍采用旧宿主部署路径，不属于标准 manifest 迁移；registry 兼容入口没有 TS 查询。默认入口不依赖应用退出后遗留的 JVM 状态。

registry启动只接受所需身份的已握手、当前未关闭直连。静态种子、gossip地址、hub路由及其它身份的连接数量均不能代替注册；已关闭但尚未异步移除的sender也不算就绪。应用等待共用一个`--wait`期限，不再在地址入表后额外等待五秒并无条件继续；超时列出缺少直连的身份并失败退出。websearch的serve-only只需jvm-search-1，其余模式及crawler-lab需erl-gw-1、ray-gw-1和jvm-search-1。

这是启动时的本地连接观察，不是组件健康检查或持续可用租约；连接随后仍可能断开，后续部署和业务操作必须分别处理失败。没有新增自动拨号、权限授予、业务重放或网关重启能力。


```sh
make test-crawler-manifest # 三形态、改名/配置、错误路由、查询与回收
make test-crawler-business # 旧兼容入口、实际三形态与历史 golden
make test-crawler-app      # 协议故障与 CLI 场景
```

历史 golden 文件保持不变，比较固定业务计数和四组排名。它不包含原始 CID 轨迹，不能据此声称 CID／消息字节顺序与历史完全相同。当前真实迁移测试也不证明历史 CID 等价、网络恢复原子性或生产性能指标。

Ray 2.51.2 在内部随机端口尝试耗尽时可能返回已被另一内部服务分配的端口。网关初始化期间对其 `_get_unused_port` 返回值做排除集检查，必要时通过有界的内核 socket 探测选取不同端口，初始化结束后恢复原方法；不修改安装包，也不重试已部分启动的集群。这依赖 Ray 的私有初始化接口，需要版本兼容测试；返回端口到实际 bind 之间仍有普通跨进程竞争窗口，并非全局端口预留机制。
