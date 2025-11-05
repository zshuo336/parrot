# parrot-obs

以真实管理回复和业务回复观测 Erlang、Ray 与 JVM 演示网关。`help`、无参数及 `--help` 不建立网络连接。节点使用数字 IP:port，可用 jvm@node-id=IP:port（erl/ray 同样支持）显式选择非默认身份；IPv6 地址加方括号。参数可分开传入，也可放在一个空格／逗号分隔字符串中。未知命令、重复节点、缺失／非法数值和零工作量均报错。

```sh
cargo build --locked -p parrot-obs
cargo run --locked -p parrot-obs -- --help
./target/debug/parrot-obs ask erl=127.0.0.1:19871 ray=127.0.0.1:19873 jvm=127.0.0.1:19872 --rounds 5 --insecure-development
./target/debug/parrot-obs load ray=127.0.0.1:19873 --seconds 10 --conc 4 --insecure-development
```

`--insecure-development` 显式启用这些演示网关的无加密连接，不是生产认证设置。此 CLI 尚未提供 mTLS 配置入口。

`ask` 与 `load` 验证协议身份和精确业务值：Erlang 返回 `Ping+3`，Ray 返回 `Ping+2`，JVM 原值回显。错误类型、错误值、畸形载荷和请求失败都算失败；任何失败令有限命令非零退出。管理 `ping` 验证管理回复，不代表业务路径正常。有限命令无论成功、错误或中断都执行远程系统关闭。

`status`、`metrics`、`ping`、`watch`、`trace` 和 Web 均请求 measured_metrics；不支持新协议的对端明确报错，不回退猜测旧快照。未置 availability 的字段显示 `n/a`，已测零照常显示；processes 附带单位，组件数与实例状态数独立。`watch` 与 `trace` 使用每个节点实际收到快照的单调时钟间隔。首个样本、字段不可用、epoch 未知／变化、计数回退和非正间隔显示 `n/a`。网络或采集失败不会伪装为零速率。`trace` 目前是计数器差分命令，不是分布式追踪或 trace exporter。

`load` 限制 1..128 并发工作任务，期限后停止发送并等待已发送请求结束；每个请求有 10 秒期限。吞吐分母包含这段真实收尾时间。工作任务由 JoinSet 持有，取消命令会取消任务，任务失败传回调用者。延迟使用固定 65 桶直方图，每个工作任务和合并结果均为固定容量；P50/P95/P99 明确显示为桶上界，不宣称精确分位值。失败回复不混入成功延迟或成功吞吐。没有有效回复也是失败。

验证：

```sh
cargo test --locked -p parrot-obs --all-targets
cargo clippy --locked -p parrot-obs --all-targets -- -D warnings
node --test tools/parrot-obs/tests/dashboard.test.cjs
# 需先构建 JVM / Erlang 网关并安装 interop/python[ray]：
python3 tools/parrot-obs/tests/run_gateways.py
```

集成脚本使用动态端口、真实就绪回执及本脚本拥有的子进程组，逐一执行 status、metrics、ping、ask、trace、load、watch、web，并验证断开网关返回失败。缺少工具或制品不会隐式跳过。它不按进程名清理网关。原有 tests/e2e.sh 已作为此脚本的兼容入口，可用 PYTHON 显式选择安装了 Ray 的 Python。watch 验证第二轮速率和 Ctrl-C 关闭；web 验证实际三网关快照、u64 字符串、脚本资源／CSP和错误 Host 拒绝。

Web 服务还提供标准 Prometheus 文本端点 `GET /metrics`。每个节点使用 `node`、`runtime`、`epoch` 和 `process_kind` 标签；不可用字段被省略，采集失败只导出 `parrot_node_up 0`，不会伪造零值。端点与 dashboard 一样仅绑定 loopback、拒绝请求体并受同一请求期限和 Host 校验保护。该端点导出已有 measured snapshot，不声称提供分布式 trace 或 OTel span。

当前 wire 快照仍有部分 unavailable 以 0 编码的历史字段；Prometheus 端点按 availability 屏蔽这些槽位。CLI 测量修复不等于 trace context 传播或完整 OTel 能力。


Web 控制台只绑定 `127.0.0.1`。每个连接统一 15 秒期限（读头、远程采集、响应写入），最多拥有 64 个处理任务，HTTP 请求头最多 8192 字节；容量耗尽时暂停 accept，由有限内核 backlog 承受连接压力。取消服务时中止其拥有的连接任务。它是有限的 HTTP/1.1 GET 查看器，不支持请求体、代理请求、TLS 终止或公网部署。

Host 必须匹配本端口的 localhost 或 127.0.0.1，缺失／重复 Host、错误方法、声明请求体和 transfer-encoding 均拒绝。返回 no-store、nosniff、frame 拒绝和 CSP；脚本从本站独立资源加载。远端字符串经 HTML 文本转义，所有数值严格解析。JSON API 的 u64 指标编码为十进制字符串，避免浏览器在 2^53 以上丢失精度；这是本工具 HTTP 输出约定，未改变网关 Wire 协议。

前端 uptime 仅在 ts_ms 与 uptime_start_ms 均可用且时序合法时显示。速率使用 API 中各节点的 sampled_at_ns 与 sample_clock，即采集端实际接收回复的单调时刻；不用浏览器收到整个 HTTP 响应的时间代替。epoch／采集时钟变化、字段失效、错误及计数倒退会清理旧基线；小图最多保留 61 个点、120 秒窗口。API u64 为十进制字符串，未知字段为 null；availability 未置位时即使槽位非零也不展示。已测 RSS=0 显示 0 B，未采集显示 n/a。终端远端文本转义控制字符，网页保持文本转义。


## 拥有明确生命周期的开发网关

```sh
make check-jvm check-erl
# 使用安装了 interop/python[ray] 的 Python；不隐式安装依赖：
PYTHON=/path/to/python bash tools/parrot-obs/gw.sh up
bash tools/parrot-obs/gw.sh status
bash tools/parrot-obs/gw.sh log jvm
bash tools/parrot-obs/gw.sh down
```

up 启动常驻管理进程，由私有 Unix socket 接收 status/down。同一配置重复 up 加入现有会话；不同配置明确报错，必须先停止本工具拥有的会话，不会重启或终止机器上的同名进程。网关就绪来自本次子进程的实际端口回执；端口被其它服务占用不能充当就绪。任何网关启动失败／超时或后来退出都会触发整组回收。down 的成功回执在实际等待进程退出后返回。

每个网关由独立进程组中的 Python guardian 持有。管理进程在最后一次组终止信号前不回收 guardian，避免复用旧进程组编号；控制通道 EOF（包括管理进程被 SIGKILL）会令 guardian 自行终止其进程组。正常关闭先发送 SIGTERM，给约 2 秒清理时间，再向仍持有未回收子进程身份的组发最后一次终止信号。管理器与应用启动器随后观察整个会话，包括同会话中另建进程组的 worker；连续两次确认没有活进程／线程，才回收 guardian 并返回成功。观察有共同的 10 秒期限，超时或无法确认返回错误。扫描所得 PID 只用于观察，不授权发信号；发现外部 reaper 已回收 guardian 时拒绝使用缓存组号。Linux 的僵尸主线程若仍有活线程，也不能算清理完成。它是本地 Unix 进程管理，不是隔离恶意插件的进程沙箱；主动另建 session 的第三方子进程仍须由对应网关运行时回收。

默认状态目录按 UID 与仓库路径隔离，位于 /tmp，目录权限必须为 0700。GW_RUN_DIR 可选择其它短路径的私有目录；拒绝共享权限目录、符号链接叶目录和超长 Unix socket 路径。锁文件与 socket 协调并发启动，磁盘 state 仅作诊断，不按其中的 PID 发信号。网关日志每份 2 MiB，保留两份滚动备份，单条超长日志会截断。管理进程不存在时 status 表示 owner 已停止；管理进程异常死亡后的后代退出由 guardian 的 EOF 清理负责，缺失管理进程不是可用的逐后代退出回执。

GW_ERL_PORT / GW_JVM_PORT / GW_RAY_PORT 默认 19871 / 19872 / 19873；设 0 可由 OS 分配动态端口，指定的非零端口必须互异。GW_JVM_NODE 默认 jvm-search-1，可指定合法自定义身份。up 输出包含显式方言与身份的 NODES 字符串，仍须给 parrot-obs 加 --insecure-development 才连接这些无 TLS 演示网关。PYTHON 同时选择辅助脚本和 Ray 网关的解释器。

```sh
python3 -m unittest discover -s tools/parrot-obs/tests -p 'test_gateway*.py' -v
# 有完整三语言制品和 Ray 后，验证真实守护化入口：
python3 tools/parrot-obs/tests/run_owned_gateways.py
```

控制请求最多 4096 字节，单次读头总期限 250 ms，逐字节发送不会续期。状态文件写入失败仍执行组终止、wait 和通道关闭，并通过仍存活的控制连接返回失败；诊断写入不能阻止资源回收。guardian 继承 owner 锁，管理进程异常死亡后，在旧组退出前新 up 不能越过退休中的所有权。

八命令集成测试 run_gateways.py 同样通过正式 gateway_manager／guardian 启停真实网关；不会因直接子进程先退出就遗漏同组后代。测试发生业务断言异常时仍依次清理 CLI 和整个网关会话；新增两条真实进程回归验证忽略 SIGTERM 的后代、重复关闭和异常路径端口回收。

真实 owner 验收同时覆盖 Ray 组件跨连接登记、业务 ASK、停止回执及重新部署；契约说明见 [Ray 网关生命周期](../../docs/RAY_GATEWAY_LIFECYCLE.md)。


Ray 所有权补充（第185批）：正式配置使用独立 `ray_gateway.py` 入口，显式创建 local Ray 和每次唯一的短路径 `/tmp/pg-ray-*`，不会受 RAY_ADDRESS 影响加入已有集群，也不复用共享 `/tmp/ray`。日志中的 PARROT_RAY_DIRECTORY 记录实际目录；Ray 自身日志保留在该目录，2 MiB 旋转上限只适用于 guardian 收集的网关 stdout，不是全部 Ray 日志的总额度。Erlang／Ray 身份固定，JVM 使用 GW_JVM_NODE，环境中的 PARROT_NODE_ID 不能覆盖它们。

Ray 在 guardian 的 session leader 主线程运行，避免 macOS Ray 启动 reaper 时 setpgrp 逃离外层进程组。独立 watchdog 在运行时的致命信号处理器或直接退出后仍持有会话 lease，直到组清理结束；它显式报告 runtime 死亡，不能因为其副本 control socket 仍打开而将已死的网关继续显示为running。管理器被 SIGKILL 后通过 EOF 独立清理；正常down必须取得真实guardian终态。此前仅终止外层guardian可能留下活跃Ray，该路径不能用历史正常up/down日志追认为正确。

`python3 tools/parrot-obs/tests/run_owner_retirement.py` 使用实际 Erlang/JVM/Ray 与 parrot-obs，验证幂等公开up/down、三方业务值、Ray制品部署／跨连接状态／停止与重新部署、管理器SIGKILL、Ray运行时SIGKILL、并发私有集群和停止一个owner不影响另一个。采样进程PID＋创建时间，结束后独立核对活进程、端口与lease；输出目录及原始日志保留。外层使用明确异常判据，`python -O` 不会移除这些检查；现有Ray业务helper另用未启用优化的子进程执行。该真实矩阵加入Linux CI，本机与Linux的实际验收状态以修复台账为准。

最后组信号失败也不能中断其他组清理或跳过guardian的wait。管理器保存每个guardian的实际退出码，无法完成的信号／等待明确记为failed，并继续关闭控制通道。macOS上Ray自带reaper先清空进程组时，最后信号可能报EPERM；该错误仍保留，不伪装为成功，后续回收／终态写入必须完成。
