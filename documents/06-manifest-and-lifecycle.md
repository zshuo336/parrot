# 06 清单装配与生命周期

本章解释为什么真实 Parrot 应用不只是“启动几个进程”。清单还要决定谁先启动、业务引用怎样注入、失败后谁负责清理，以及什么时刻才能停止网关。

## 1. 运行原始清单

完成第一章构建后，保持本地实验网站运行：

```bash
./target/release/websearch --insecure-development \
  --manifest apps/websearch/websearch.app.toml \
  --data "$PWD/.local-websearch-manifest" \
  --crawl-origin http://127.0.0.1:18080 \
  --pages 10 --sites 10 --maxdepth 2 --port 0 \
  http://127.0.0.1:18080/
```

此入口由应用内置宿主创建所需网关，与 `run.sh` 的外部监督路径不同。不要同时在同一数据目录启动另一份应用。命令中的 Rust 二进制路径需与实际 Cargo 构建位置一致。

## 2. 顶层身份与组件声明

[原始清单](../apps/websearch/websearch.app.toml) 的顶层 name/version 描述应用。组件分别声明 name、engine、artifact、deps、hooks 和 config。

```toml
[[components]]
name = "frontier"
engine = "erlang"
artifact = { Beam = { app = "frontier", uri = "erlang/" } }
```

这里 `Beam` 决定制品方言，`uri` 指明业务文件位置。清单相对路径以清单目录解析，不是随意以当前 shell 目录推断。这个差别是移动清单、打包安装时最常见的错误来源之一。

Python 声明使用 `PyModule`，指定 `module="tokenizer"` 和 `uri="python/"`；JVM 声明使用 `Jvm`，指定 `main_class="websearch.search.SearchComponent"` 和业务 jar 路径。语言宿主必须支持对应制品方言。

## 3. deps 是装配顺序，不是数据流

清单中 tokenizer 依赖 frontier，search 依赖 tokenizer，crawler 依赖三个远端角色。装配器据此规划启动次序和失败回滚。

`deps` 不代表 frontier 会自动把数据发送给 tokenizer。实际数据流仍是 Rust 调用 frontier、Rust 调用 tokenizer、Rust 提交 search。引用路由需要 wiring，业务调用还需要真实代码。

增加一个依赖不会自动调用新组件。把能力写进清单但没有注入引用和执行路径，也不算功能完成。

## 4. Rust crawler 为什么是 Props

```toml
[[components]]
name = "crawler"
engine = "parrot"
artifact = { Props = { factory = "websearch.crawler" } }
deps = ["frontier", "tokenizer", "search"]
hooks = { on_start = ".", on_drain = ".", on_stop = "." }
```

`websearch.crawler` 是已链接进 Websearch 宿主的真实工厂。应用注册工厂后，宿主在 thread 引擎中创建 workflow Actor，注入配置、远程路由和生命周期。

它不是动态库路径，不是 Wasm 模块，也不是任意 `parrot-app` 二进制都认识的内置工厂。仅复制 TOML 到一个没有这个工厂的通用宿主，无法凭名字创建业务。

这演示了 Props 的适用场景：业务与宿主一起编译，启动时选择配置和拓扑。若要改成插件部署，需要实现对应制品加载与生命周期，不应只改 artifact 名称。

## 5. 配置为什么分两层

```toml
[components.config]
settings = '{"sites":100,"maxdepth":10,"pages":50000,"port":8080,"data":"./data"}'
```

这段是 crawler 自己解释的业务配置。当前实现要求 `settings` JSON，使用严格字段检查。显式 CLI 参数覆盖相应清单业务值，未显式提供的值来自清单与默认配置。

框架配置另放 `config_overlay`，例如：

```toml
[config_overlay.thread]
shared_pool_size = 4
```

线程池配置属于框架，抓取页数属于业务。把 sites 塞进框架 overlay 不会让 crawler 自动读到它；反之，调整业务 JSON 也不等于调整运行时调度器。

## 6. wiring 如何把逻辑角色接到真实引用

```toml
[[wiring]]
from = "crawler:/user/search"
to = "search:/user/search"
qos = "lan"
```

左端是 crawler 要使用的逻辑出口，右端是目标组件的逻辑入口。装配器获得真实部署地址后建立路由，业务使用注入引用访问，而不是硬编码一次运行的随机组件名。

frontier 和 tokenizer 有对应两条 wiring。`qos="lan"` 描述组件间的链路配置意图，不限制 crawler 访问公网，也不保证业务 exactly-once。HTTP 抓取范围由 crawler 参数控制。

接线验证不仅要看 TOML 能解析，还要验证真实目标收到业务消息。把三个引用都错误接到同一组件，管理部署可能仍然成功，但业务会在类型或载荷验证阶段失败。

## 7. 从解析到业务运行的阶段

```text
解析参数与清单
  -> 检查路径、制品与拓扑
  -> 计算有效配置和依赖计划
  -> 启动拥有的网关并完成握手
  -> 部署远端业务组件
  -> 创建 Rust workflow Actor 与接线
  -> 生命周期启动，进入抓取和 Web 服务
  -> 排空业务与持久化
  -> 停止组件与回收网关
```

`--validate-only` 用于提前检查输入；它不执行真实业务、不能证明 jar 一定成功加载，也不能证明远端 handler 的语义正确。它应在网络绑定和数据修改前返回。

清单模式不能与手工 direct 地址、`--node-root` 随意混用。`--runtime-root` 指本机完整运行资源树，必须是绝对路径；业务制品根和通用运行资源根不是同一个概念。

## 8. 停止、排空与资源所有权

on_start 启动业务。on_drain 请求停止产生新工作，结束当前批次并完成必要持久化。on_stop 负责终止业务资源。精确先后顺序由实际宿主与 workflow 实现保证，不能在 hook 名称里写“drain”就当成已实现。

搜索组件仍需要接收最后的检查点，因此不能先关 JVM 网关再等待抓取器落盘。HTTP 在途请求也需要考虑，不能把页面服务 socket 关闭等同于任务完成。

独立编排器记录本次尝试部署的组件，即使部署回执丢失也会尝试精确回收。`Components::retire` 并发请求各节点停止；一个节点没有回复不应阻塞其他节点收到停止请求。组件已不存在可作为幂等成功，其它拒绝和超时必须暴露。

## 9. 为什么组件名带身份和代际

当前组件名形如 `ws-<identity>-<role>-<run>`。同一个网关上可以有多次应用运行，固定 `/user/search` 容易让第二个进程误停第一个进程的业务。

应用把精确部署意图写入 `remote.components`，重启后只清理自己记录的旧实例。随机 identity 来自自己的数据目录；丢掉 identity 后不能凭一个角色名安全认领旧组件。

这解决示例中的归属和回收问题，不是面向恶意租户的认证或分布式 fencing。共享网关的权限边界仍需单独设计。

## 10. 建议的学习实验

先用独立数据目录分别运行 direct 与 manifest，比较搜索结果、启动日志、组件路径和停止过程。再在复制到独立实验目录的清单中故意写错一个工厂或制品路径，检查启动失败与资源回收。

实验判断依据应是实际进程、实际组件状态、退出码与错误日志。只看到“计划生成成功”不能证明组件已运行；只看到 TCP 断开也不能证明远端组件已停止。
