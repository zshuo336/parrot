# 本地互操作验证命令

Rust→Python／Ray 的因果上下文接口、真实部署验证入口及尚未记录 Python span 的边界，见 [Python 因果上下文](../docs/PYTHON_CAUSAL_CONTEXT.md)。

另有 `interop-matrix` 用于 Rust／JVM 的 bincode 一致性和 RTT 门禁，共用以下会话所有权机制。先执行 `make build-jvm`（需要 JDK 21 和 Maven），再执行 `CARGO_BUILD_JOBS=2 cargo build --locked -p parrot-remote --bin interop-matrix` 和 `./target/debug/interop-matrix`。它保留17个无符号整数宽度边界、u64溢出拒绝、拒绝后的CPU恢复，以及500次预热、2000次正式echo采样；p50必须严格小于300µs，p99只报告，不替换原门槛。业务检查通过后还须完成客户端关闭和会话退休，才报告整条命令PASS。

JVM命令默认启动期限15秒、业务期限60秒、启动输出上限64KiB；支持同名的`--startup-timeout-ms`、`--business-timeout-ms`和可选端口参数。classpath优先读取`PARROT_JVM_CP`，否则使用`PARROT_JVM_JAR`与其同目录`cp.txt`；默认jar位于构建时源码的`interop/jvm/target`。移动二进制后，除监护脚本的`PARROT_SOURCE_ROOT`外，应显式配置classpath或jar位置。参数保持Unix原始字节，包括非UTF-8 classpath，不经过shell解释或有损转换。监护配置总量仍有256KiB上限，包含参数的编码开销。

启动输出限额只计算就绪标记结束前的字节；同一次pipe读取中的后续日志不应反过来使就绪失败。JVM原有仅检查直接PID的三项启动测试已改为检查真实监听后代；`interop/tests/matrix_lifecycle.py`另验证信号、异常、业务及原始参数传递，受控Wire夹具不代替真正的Java运行。

`interop-ray`、`interop-erl` 启动各自独占的本地网关，执行真实 TCP 握手以及 Ping/Add 请求，关闭 Rust 远程系统并确认监护会话内没有存活进程后才输出 `PASS`、返回 0。任一步失败返回 2；启动和业务错误不应留下网关后代。它们是开发验证命令，不是常驻服务管理器或跨机性能基准。

## 运行

在源码根目录构建（项目构建并发约定为 2）：

```sh
CARGO_BUILD_JOBS=2 cargo build --locked -p parrot-remote --bin interop-ray --bin interop-erl
./target/debug/interop-ray
./target/debug/interop-erl
```

两者均接受可选十进制端口（默认 0，由系统分配）、`--startup-timeout-ms`（默认 60000）和 `--business-timeout-ms`（默认 15000）。期限取值 1–600000 毫秒，端口取值 0–65535，重复选项和多余参数会失败。启动期限包含监护器通信；业务期限包含连接、握手和两次请求。超时后仍须完成关闭：Rust 系统关闭最多等待 10 秒，网关有 2.2 秒宽限及 10 秒进程组观察期限。这些不是对内核不可中断任务的硬实时保证。

运行要求 Linux 或 macOS、Python 3，以及当前源码中的 `tools/parrot-obs/gateway_worker.py`。Ray 还要求与 `interop/python` 依赖声明兼容的 Ray 环境；Erlang 要求 `escript` 及源码网关依赖。其它平台会明确返回不支持。默认源码位置来自构建时的 crate 路径，与运行时当前目录无关；移动二进制后应设置 `PARROT_SOURCE_ROOT` 为所配套的源码根目录。

```sh
PARROT_SOURCE_ROOT=/path/to/parrot /path/to/interop-ray 0 --startup-timeout-ms 90000
```

Ray 每次显式启动 local 集群并使用独立 `/tmp/pg-ray-*` 运行目录，覆盖环境中的 `RAY_ADDRESS`，移除继承的 `RAY_AUTH_TOKEN` 并配置本次私有凭据文件。不会使用全局 `ray stop`。Erlang 监听 `127.0.0.1`，Ray 网关同样只接受回环 TCP。固定网关身份分别为 `ray-gw-1` 和 `erl-gw-1`，不会被环境中的 `PARROT_NODE_ID` 覆盖。

## 协议与退出语义

探针使用注册的 `bin:u:Ping/Pong/Add/AddR` codec：单值为精确 8 字节小端 u64，Add 为精确 16 字节的两个小端 u64，拒绝短包和尾缀。Ray 的 Ping(100) 应返回 Pong(102)，Add(3,4) 应返回 AddR(1007)；Erlang 对应 103 和 10007。不要把网关的能力声明误称为这些消息使用 protobuf 编码。

独立 OS 线程持有未回收的监护进程，保留组／会话身份到最后一次组信号。Ray worker 会自行创建进程组；退休确认扫描整个监护会话，等待这些 worker 也退出，避免过早报告 PASS。信号只发给持有的监护进程组，扫描所得 PID 不用于发信号。其它组未及时退出时返回失败回执。SIGINT、SIGTERM、启动失败、业务失败和调用者取消均触发清理。启动器被 SIGKILL 时，私有控制 socket 的 EOF 触发监护器／内联 watchdog 退休；此时启动器本身不可能再提供成功清理回执。机制能观察同一会话内的多个进程组，不能作为不可信程序主动调用 `setsid` 后仍被约束的容器沙箱。宿主不得设置 `SIGCHLD=SIG_IGN` 或并发回收这些子进程；发现已丢失所有权时会报告失败，不再向缓存 PID 发信号。

启动输出在就绪前最多接受 256 KiB。就绪后继续排空 stdout/stderr，并按每份 2 MiB、2 份备份旋转网关日志；这不限制 Ray 自身运行日志的总量。stderr 打印本次 `parrot-probe-*` 日志目录，供失败诊断；正常确认退休后删除私有凭据。强制杀死启动器或无法确认退休时可能保留私有目录和凭据，不冒称已清理这些磁盘文件。

## 回归测试

`crates/parrot-remote/tests/interop_probe_lifecycle.rs` 调用 `interop/tests/probe_lifecycle.py`。使用真正的监护器和受控业务进程，验证静默／非法／超额输出／提前退出、业务期限、持续输出、SIGTERM 和启动器 SIGKILL；检查实际后代退出、端口释放、退出码及无关进程存活；正常返回要求当时已退休，启动器 SIGKILL 才允许异步退休。另以真实 Wire 夹具验证正确结果、错误 Ping/Add 值和错误响应类型，检查线上请求的精确载荷；18 条输入契约确保错误参数在启动网关前被拒绝。夹具测试不代替实际 Ray/Erlang 成功路径或跨机兼容性验证。

```sh
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot-remote --test interop_probe_lifecycle
CARGO_BUILD_JOBS=2 cargo test --locked -p parrot-app --features host --lib host::
```
