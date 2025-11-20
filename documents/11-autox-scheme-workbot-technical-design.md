# AutoX Scheme 与 WorkBot 行为兼容平台技术方案

版本：0.1 设计基线。日期：2026-11-09。

状态：待实现、待验证的工程方案，不是已交付能力声明。本文新增设计不代表 AutoX、Parrot 或 Steel 已提供这些接口。示例中的新 API、DSL、目录和协议均为提案。

## 1. 决策摘要与目标

已确定采用路线 A：自研 Kotlin/Java Scheme 字节码 VM，接入 AutoX Android 宿主。主要业务用 Scheme 重写，通过宿主原语操作手机；服务端采用 Rust/Parrot，设备连接优先采用 MQTT。复刻目标是 workbot-clean 与 workbot-run 的可观察行为，而不是 JS 源码、Rhino 内部状态或 Rust Steel 产品族的所有实现细节。

建设三个彼此有独立验收标准的部分：

1. 通用语言：尽量完整实现 R7RS-small，提供明确版本的 Steel 源码兼容 profile。
2. 自动化平台：可取消、可观测、有资源预算的执行器及 AutoX 能力桥。
3. WorkBot 应用：覆盖原有 39 个分发 case、外层控制命令、任务队列、登录监控与消息上报；增加完整服务端。

“尽量兼容 R7RS”不是永久允许缺项。建立条款清单；每条标记 implemented/tested/deferred/unsupported，达到完整门禁前不宣称完整 R7RS-small。R7RS-large 是独立扩展范围，不包含在默认标准承诺内。

“与 Steel 等价”作为长远的语言源码行为目标：对固定 Steel 基线、明确可移植输入域和可观察契约逐项比较。Rust trait/过程宏、现有插件 ABI、原字节码、GC 内省、JIT 产物不自动纳入。若未来要求这些，需另立兼容工程，不以更换措辞隐藏差异。

## 2. 输入、证据与当前事实

### 2.1 参考位置

- AutoX/FlowBot：`/Users/biluochun/work/wacai/qiantang/robot/soft/FlowBot`。
- 重建基线：上述目录 `.analysis/recovered/workbot-clean`。
- 联调候选：上述目录 `.analysis/recovered/workbot-run`。
- 独立分析：`.analysis/report/FlowBot-independent-review-2026-11-06.md`。
- Steel 材料：`/Users/biluochun/Documents/ChatGPT/scheme/steel-analysis/steel-project-analysis.md` 与 `java-cpp-equivalence-analysis.md`。
- Steel 参考源码：`/Users/biluochun/work/libs/lang/steel`，研究基线 `d92320e4040e11d3b1cd06a3a3dda3dbd4fbaa2d`，0.8.3。
- Parrot：本仓库 README、`docs/CLOUD_PROXY.md`、`docs/IDEMPOTENT_SUBMISSIONS.md`、`docs/MQTT_BRIDGE.md`、`docs/REMOTE_SECURITY.md`。

### 2.2 已观察到的事实

前次独立分析复跑 workbot-clean 验证入口，5/5 汇总阶段通过；工具报告模块指纹 55/55、P8 拟真场景 43/43。workbot-run 的 57 个 JS 文件、12,220 行通过语法解析；core 为 6,135 行。55 个对应模块中，51 个仅装配包装差异，4 个存在实质变化。现有工具固定读取 clean，不能把其结果移用到 run 或未来 Scheme。

已确认运行版的风险包括：未知指令/字符串数字可能假成功、结果回执失败导致业务重执行、暂停未拦住连续调度、超时未取消旧 UI 操作、连接标记早于 onOpen、身份切换删除旧队列、默认配置指向官方云。历史报告的后续更正优先于开头“全部通过”；最终 strict-v7 的发送验收未完成，部分日志和 APK 正文缺失。

Steel 材料是研究报告而非新方言规范。它覆盖的语言与运行时远大于本项目首期需求。研究发现的沙箱进程能力、中断线程清理问题说明不能直接继承安全承诺。本方案借鉴语言与测试资产，不继承其 GC/JIT/FFI 内部结构。

Parrot 已提供 Actor、受限邮箱、持久 outbox、提交 ID、Rust 远程 mTLS 和 MQTT 入站桥等基础。完整 Android SDK、双向设备网关、租户权限与 WorkBot 调度属于新增实现。当前 JVM 网关为 Java 21/Scala/Akka/Netty 工程，不能直接视为 Android SDK。

本文不新增运行验证结论；预算、对象格式、API 和指标均是待实测设计。

## 3. 收益、代价及对应工程措施

### 3.1 发挥 Scheme 的优势

- 统一语法与 AST：实现宏、静态分析、源码映射和受控 DSL；宏必须卫生并保留来源，不采用字符串替换。
- 函数、闭包和数据组合：用纯函数构建 selector、校验策略与数据变换，副作用只发生在明确宿主入口，便于测试。
- 宏表达业务约束：`with-ui-session`、`with-deadline`、`define-workflow` 自动插入权限、取消和轨迹，不让每个业务作者手写约定。
- 可嵌入的小核心：平台能力由版本化模块注入，脚本不默认获得任意反射和全文件系统。
- 可控 VM：显式帧、异步挂起、指令预算与一致错误；将隐式 JS 回调流程变成可检查的执行契约。
- 可分析代码：在编译期提取宿主依赖、声明阶段和能力使用；动态行为不能完全推断，运行期仍强制检查。

### 3.2 弥补语言自研的劣势

- 语言维护成本：语言核、宿主与业务分离；冻结最小 profile 后才扩展；标准库可借鉴但必须审查许可和 primitive 依赖。
- 人员生态较弱：提供源码栈、类型/参数错误提示、补全、格式化、示例、REPL 和 JS→Scheme 迁移手册，而不是仅提供解释器。
- 新 VM 引入缺陷：逐层门禁、原 JS oracle、Steel 差分、R7RS 条款测试和故障注入；禁止以 AST 相似证明跨语言等价。
- 性能不确定：先测冷启动、边界调用和长驻内存；不实现 JIT 来追求未经测量的收益；RPA 延迟首先看 UI 等待。
- Android 生命周期：权限、前台服务、进程回收交给 Android 原生管理；VM 监督不能替代系统授权。
- 类型转换风险：兼容库集中实现 JS 语义，不散布于处理器；正式接口使用明确 schema。
- 安全风险：宿主白名单、签名模块、受限资源和实际负向测试；能力 manifest 不是 OS 隔离。

### 3.3 决策退出条件

PoC 同时比较 JS 与 Scheme 的修改耗时、故障定位、取消延迟、内存和行为一致性。若 Scheme 仍依赖任意 Java 反射、取消无法停止后续操作、文本流程差分无法稳定通过，暂停功能扩张，先修执行模型；必要时保留 AutoX JS 作为业务执行器，Scheme 仅作编排，不能为保住路线牺牲可靠性。

## 4. 兼容性契约与 profile

### 4.1 四种正交版本

每个脚本记录 language-profile、host-api-version、business-profile、selector-profile。不要用一个 appVersion 同时代表四者。

- `r7rs-small-v1`：标准语言基线；在完整验收前有明确缺项清单。
- `steel-source-0.8.3-v1`：指定 Steel 可移植语法、primitive 与库行为；扩展名称隔离。
- `autox-host-v1`：Android 能力接口、权限、线程和资源语义。
- `workbot-clean-2.12.5`、`workbot-run-20260930`、`workbot-corrected-v1`：业务行为配置。

profile 的名字是设计提案；冻结前需对当前材料生成摘要并确认 run 的具体基线，日期不能替代内容哈希。

### 4.2 行为观察范围

包括：接受/拒绝输入、返回值、错误分类、必要的错误文案、任务入队/出队/重试、机器人身份变化、WS/MQTT 应用帧、登录状态、UI 查询与动作、记录采集和可观测资源关闭。

差异分三类：

1. 必须相同：业务输入、协议类型、对象字段、严格匹配、顺序约束、状态转移。
2. 允许关系化比较：时间戳、随机值、对象句柄、线程 ID；按事先定义的映射比较，不能失败后临时归一化。
3. 明确不比较：Java 堆地址、Rhino/VM 内部布局、调度微秒级差异、原始 JIT 机器码；耗时与内存采用预算验收。

记录每个历史缺陷的兼容策略。clean/run 配置可在隔离环境复现假成功等历史行为；默认可部署产品使用 corrected 配置。调试配置不能静默覆盖授权策略，且不得默认连接官方云。兼容已知不安全行为要显式 opt-in，不能为了兼容开放任意权限。

### 4.3 R7RS-small 要点

标准最终目标包括 `(scheme base)`、`case-lambda`、`char`、`complex`、`cxr`、`eval`、`file`、`inexact`、`lazy`、`load`、`process-context`、`read`、`repl`、`time`、`write` 的适用接口与 `define-library`、import modifiers、include/include-ci、cond-expand。

完整数值塔按精确整数/有理数与非精确实数/复数逐步实现。哈希表、记录增强、模式匹配、spawn/await 等是扩展，不能列作 R7RS-small 原生要求。标准的 Promise 是 delay/force；异步 Future 使用不同类型与命名。

标准与权限是两层：完整宿主可提供 file/load/eval 等；AutoX 受限 profile 可拒绝具体权限操作并声明环境差异。不能以“安全”为名宣称被删除接口仍完整标准兼容。

## 5. 物理架构与工程组织

```text
Scheme Source + manifest
  -> Reader / Syntax / Libraries
  -> Hygienic Expander
  -> Binding + Core IR + Effect metadata
  -> Bytecode + Constants + DebugMap
  -> Verifier -> VM/Fiber Scheduler
  -> Host Registry -> AutoX Adapter -> Android Services

Parrot/API -> PostgreSQL -> Device Gateway -> MQTT Broker
                                            ^
Android Session -> Room Inbox -> Task Executor -> Scheme VM
                        Result/Event Outbox -> MQTT
```

建议创建独立构建的引擎仓库/模块，AutoX 通过 adapter 依赖，不先放入 Parrot Rust workspace。后续是否单仓由团队决定。

```text
scheme-reader/          纯 JVM reader、datum、span
scheme-syntax/          scope、宏展开、库解析
scheme-compiler/        IR、字节码、验证、debug map
scheme-runtime/         Value、VM、continuation、调度
scheme-stdlib/          标准模块与 Scheme 源文件
scheme-host-api/        无 Android 依赖的接口
scheme-testkit/         oracle、trace、fixtures
scheme-tools/           CLI、格式化、LSP
autox-scheme-adapter/   ScriptEngine、权限与宿主绑定
android-agent/         Room、MQTT、服务与设备会话
scheme-workbot/        clean/run/corrected 业务代码
server/                Parrot 业务组件与网关
protocol/              schema、版本与兼容向量
```

语言核心禁止依赖 Android、MQTT、Room 或 AutoX 全局对象。在桌面 JVM 运行相同 bytecode/语言测试；Android 以 D8/ART 构建验证，不把桌面 Java 21 的支持自动平移到 minSdk。首期选与目标 AutoX 工程兼容的字节码/API 范围，逐项验证 desugaring。

## 6. Reader、Datum 与源码位置

### 6.1 编码规则

脚本源统一 UTF-8，拒绝非法 UTF-8；Java 宿主字符串入口拒绝孤立代理项。保存 SourceId、revision/hash、字节 start/end；编辑器/LSP 单独转换为 UTF-16 位置。行列展示基于 Unicode scalar，换行规则固定，不能将 charAt 等同 Scheme 字符。

Reader 处理：列表/点对、单双引号、quasiquote、unquote/splicing、布尔、字符命名与十六进制、字符串转义/续行、整数基数、精确性前缀、指数、有理数/复数、向量/bytevector、行注释、嵌套块注释、datum comment、datum labels、大小写 reader directives。缺项进入标准清单。

词法、datum 构造、读取共享/循环图、可执行语法验证分开。合法循环 datum 不代表合法可执行形式，编译器必须拒绝不支持的循环代码而非无限遍历。writer/read roundtrip 包括共享结构策略。

### 6.2 类型结构

```kotlin
data class Span(val sourceId: Int, val startByte: Int, val endByte: Int)
data class Syntax(val datum: Datum, val scopes: ScopeSet, val span: Span)
```

ScopeSet 是标识符绑定身份的一部分，非普通字符串标签。Error 带 span、展开栈与宿主边界；源文件修改后诊断仍绑定原 revision。Reader 深度、token 长度、源大小、datum label 数必须有预算。

## 7. 宏、库和编译期执行

### 7.1 syntax-rules

实现 literal identifier 的绑定比较、pattern variable、嵌套 ellipsis、维度一致性检查和模板引入标识符的卫生作用域。定义位置与使用位置都保留。先通过标准宏向量再用宏实现派生语法。

必须支持标准的 `let-syntax`、`letrec-syntax`，不能继承 Steel 缺项就把它当作新引擎永久限制。错误包含规则、未匹配位置和展开栈。宏展开设置步数、输出节点数、递归与总时间限制，防止编译期耗尽。

### 7.2 library

library ID、版本、源码哈希、导入映射、导出绑定、宏环境与初始化状态分开存储。支持 only/except/prefix/rename；模块同一身份初始化次数按契约固定。循环依赖按初始化/宏阶段检测并报稳定错误，不无限等待。include/include-ci 受包根约束，不允许路径逃逸。

`cond-expand` 的 feature 来自实际 capability，不因包存在就声明可用。源码兼容层可翻译 Steel require/provide 到库 IR，但保留其不同导入规则的专用适配与测试。

### 7.3 eval 和 Steel 编译期扩展

R7RS eval 接受显式 environment，库解析与能力范围继承调用上下文，不能获得额外权限。eval、load 的编译资源计入当前任务；取消也涵盖动态编译。

Steel syntax-case 与任意编译期 Scheme 放在后期。编译期 VM 不注册 UI、网络和进程 primitive；若允许编译期读固定资源，按可重复构建声明输入哈希。动态能力检查不能仅靠静态导入分析。

## 8. 值模型、数值、集合与内存

### 8.1 值表示

Java sealed hierarchy 起步，测量后再决定 tagged long/NaN boxing，不先引入复杂布局。

```text
Boolean / Character / ExactInteger / Rational / Flonum / Complex
String / Symbol / Pair / Vector / ByteVector / Record
Procedure / Continuation / Port / LazyPromise / AsyncFuture
Unspecified / InternalUninitialized / ForeignHandle
```

InternalUninitialized 只用于 letrec 等内部状态，不暴露为 JS undefined；JS 兼容层用独立 JsUndefined 类型。Scheme 空列表、#f、未指定值与宿主 null 不混用。JSON null 也需显式表示。

### 8.2 标准与确定性选择

- 仅 #f 为假，空列表为真。
- 标准允许未指定的参数求值顺序，本实现选择从左到右并文档化；不能据此认定与所有 Steel 输入都一致。
- 多值是独立 ValueBundle；单值上下文接到零/多值按规范明确错误或未指定策略，不偷偷打包成列表。
- Symbol interning 不等于字符串身份；eq?/eqv?/equal? 分开，循环 equal? 有 visited 对集合。
- Pair、String、Vector、ByteVector 的可变性和 literal 修改规则按标准定义；不能为了持久化容器方便而全部改不可变。
- 字符为 Unicode scalar；索引 API 语义一致。UTF-16 与 UTF-8 转换在边界层做。

### 8.3 数值

小整数溢出提升 BigInteger；Rational 约分并规范分母正号；复数由实/虚分量组成。测试精确/非精确混算、round/floor/truncate、除零、负零、NaN、无穷、极大指数、打印读取。BigDecimal 不直接替代精确有理数。目标 32/64 位行为不由 Java int 隐式决定；Steel 的 isize 差异在兼容 profile 中参数化。

### 8.4 GC 与资源

普通对象交给 ART/JVM GC；逻辑身份用稳定 ID，不提供真实物理地址。外部资源显式 close，Cleaner 只兜底且不承担业务正确性。ForeignHandle 按执行代次、页面代次、所有者和类型验证。

堆预算采用语言对象与外部资源逻辑计费，加源/字节码/常量池/缓存预算；这不等于精确 RSS。宿主 API 预留后分配，超限拒绝。JVM OOM 视为宿主严重故障，不宣称能通过 Scheme guard 安全恢复。长期存活、循环对象与取消后资源释放需设备压力验证。

## 9. 编译器、字节码与缓存

### 9.1 编译流程

Reader → 库/宏展开 → 绑定分析 → Core IR → 闭包捕获与可变变量 boxing → tail-position → 常量与代码池 → bytecode → verifier。初期不做影响语义的 aggressive optimization；优化与关闭优化跑同一差分集。

Core IR 明确 Const、Ref、Assign、Lambda、Call、TailCall、If、Sequence、Values、Raise、DynamicContext。宏实现表层控制流；不能用宏绕过宿主权限。

### 9.2 字节码

```text
CONST, LOAD_LOCAL, STORE_LOCAL, LOAD_CAPTURE, STORE_CAPTURE
LOAD_GLOBAL, STORE_GLOBAL, POP, MAKE_CLOSURE
CALL, TAILCALL, RETURN, JUMP, BRANCH_FALSE
MAKE_PAIR, MAKE_VECTOR, VALUES, EXPECT_VALUES
PUSH_HANDLER, POP_HANDLER, RAISE
CAPTURE_CONT, INVOKE_CONT, DYNAMIC_ENTER, DYNAMIC_EXIT
HOST_CALL, YIELD, CANCEL_CHECK, HALT
```

具体集合在 IR 原型后冻结，标准 dynamic-wind/continuation 不能简单等价成 PUSH/POP 而忽略跨跳转。指令操作数、栈效果、错误条件、安全点都写成机器可读表，生成 disassembler/verifier 文档。

Verifier 检查版本、跳转边界、局部槽位、常量类型、栈高度、handler 区间、函数参数信息和 capability 引用。禁止从网络直接执行未验证 bytecode。文件 checksum 只能检测损坏，签名负责来源认证。

### 9.3 缓存格式

头部：magic、formatVersion、languageProfile、compilerVersion、stdlibHash、hostAbiVersion、sourceHash、dependencyHash、section lengths。代码、常量、导出、debug map 独立段；限制总大小和引用深度。

缓存不包含 Android 对象、活动线程、网络会话或 VM 栈。跨版本不兼容时丢弃缓存并从已验证源码重编译。原 Steel bootstrap/Executable 不自动兼容；首版不读取它们。

## 10. VM、continuation 与动态上下文

### 10.1 执行所有权

每个 Engine 有单一 owner dispatcher，所有 Scheme 值可变状态只在该所有者访问；MQTT/网络/Android callback 只发送恢复事件，不直接运行 Scheme 闭包。Engine 与线程数量不等同；调度器可轮转多个 Engine，但必须遵守绑定。

```text
Fiber: id, generation, frameStack, valueStack, handlerStack,
       dynamicParameters, windChain, state, budget, deadline, cancellation
Frame: codeId, pc, locals, captures, callerBase, expectedValues
```

CALL/TAILCALL 操作显式帧，不依赖 JVM 递归。闭包共享被 set! 捕获的 cell；正确支持 letrec 初始化检查。单步错误后栈与动态状态能清理；引擎是否可复用由错误等级决定。

### 10.2 call/cc 与 dynamic-wind

完整 R7RS-small 目标包含 continuation，不能永久以“首版推迟”充当完整兼容。实现为不可变/写时复制的逻辑帧、值栈和动态上下文快照，支持多次调用；明确 owner Engine 与 Fiber 规则。

调用 continuation 时计算 wind 链公共祖先，按正确顺序执行离开 after 与进入 before，再恢复目标栈与多值。异常 handler、parameterize、动态端口与 wind 联动测试；before/after 再次跳转和抛错需要重入状态机，不能使用一次性 Java finally 冒充。

捕获范围是 Scheme 栈，不捕获 Java 调用栈。宿主 primitive 必须在返回/挂起边界交还 VM 控制。不可重入 native 调用期间禁止穿透回调捕获，提供明确 host-boundary 错误；这是宿主扩展约束，标准纯语言路径不能被该限制误伤。

普通 Scheme continuation 不会撤销已发生的外部副作用。自动化任务额外采用能力代次和一次性操作 token，任务取消/结束后即使恢复旧 continuation 也不能重新获得 UI 权限。此任务模式限制需与通用语言模式分别声明，不能宣称续体天然保证业务恢复。

## 11. 异步、协程、取消与暂停

### 11.1 两种 Promise

`delay/force` 是标准惰性求值；`AsyncFuture/await` 是宿主扩展，不能混用。lazy promise 的异常缓存、递归 force、delay-force 和多值行为按条款验证。

```kotlin
sealed interface HostOutcome {
    data class Done(val values: ValueBundle) : HostOutcome
    data class Failed(val error: SchemeCondition) : HostOutcome
    data class Pending(val token: PendingToken) : HostOutcome
}
```

PendingToken 包含 engine/fiber/operation/generation、deadline、取消 hook。恢复采用 compare-and-set 的单次状态机；重复回调、超时后迟到回调和旧连接回调记录诊断后丢弃。异步恢复携带 continuation 使用规则，禁止把同一底层 I/O operation 克隆成多个操作。

### 11.2 调度与预算

指令 slice 到期 yield，编译器回边与每个 HOST_CALL 前有取消检查；VM dispatch 仍计费，不能仅依赖编译器插入点。宏编译、深层 equal?/writer/reader、BigInt 和 regex 等 native primitive 也需要预算/输入上限，防止一个指令无限耗时。

默认预算作为可调初值：源码 1 MiB、编译输出 8 MiB、逻辑语言堆 16 MiB、最多 4 个 Fiber、业务任务 180 秒。不是已证实合理值，PoC 用真实脚本调整。UI 取消后不再提交新动作是硬语义，具体响应时间在设备验收时定义测量门槛。

### 11.3 暂停与取消

- 暂停准入：立即阻止认领下一任务。
- 当前任务暂停：仅在安全点冻结，已经开始的宿主动作先等待确认或进入 uncertain。
- 取消：传播至子 Fiber、监听器、下载、定时器、UI 等待；释放所有权须在动作完成/终止确认后。
- 进程被杀：无清理保证，持久状态与重启恢复负责处理。

暂停不等于停止 MQTT 会话；控制通道应始终能接收恢复/取消，但不能绕过权限。控制 mailbox 有独立预算，Actor/VM 不能被同步长调用无限占住。

## 12. AutoX 接入与 Android 宿主桥

### 12.1 适配点

研究项目已有 ScriptSource、ScriptEngine、引擎工厂与执行服务。新增 SchemeScriptSource、SchemeScriptEngine 和 EngineFactory 注册；后缀 `.scm`、project.json entry/engine 信息路由到新引擎。具体接口签名以目标 AutoX checkout 为准，不能只新增 Kotlin interface 就算接入。

启动链：AssetsProjectLauncher/项目启动 → source 类型 → Engine 注册 → 编译或缓存读取 → capability 装配 → VM 执行 → 现有观察者收到 start/success/error → finally 按资源所有权销毁。

保留 Rhino 以支持回滚与参考测试；JS/Scheme 不共享可变全局对象，不默认做双向任意 eval。需要调用旧 JS 时仅通过版本化 RPC 边界，以序列化数据交互，并标记为过渡依赖。

### 12.2 宿主接口

```kotlin
interface SchemeHost {
    fun invoke(ctx: HostCallContext, name: HostFunctionId,
               args: List<Value>): HostOutcome
}
```

HostCallContext 带 task/attempt/engine/fiber、权限、deadline、取消、会话/页面代次与 trace。函数注册表声明参数、返回值、多值、效果类别、线程要求、是否可挂起、资源成本。允许纯函数离线测试；所有有副作用函数先验证执行 token。

模块划分：`autox/ui`、`device`、`app`、`storage`、`http`、`events`、`floaty`、`image`、`log`、`task`。JSON 与协议在独立模块，不借用 Scheme printer 作为 JSON 编码。

### 12.3 UI Driver

Selector 是不可变数据；find、wait 和操作分开。NodeHandle 只短期有效，包含 package/window/pageEpoch、driver generation 和 handle ID；页面 epoch 是宿主管理的失效策略，不声称能捕获所有 Android 树变化，使用前仍要 refresh/校验。

```scheme
(import (autox ui))
(define send-button
  (selector :class "android.widget.Button" :text "发送" :depth 16))
(ui:click (ui:wait-for send-button :timeout-ms 5000))
```

以上 `:keyword` 为拟议扩展，Reader 可先将其视为普通 Symbol，参数解析器处理，不能冒称标准 R7RS 关键字语法。

屏幕只允许一个 UiLease；所有点击/滑动/输入串行执行，读取也受页面一致性策略。不同 Android API 的线程要求分别实现：View/floaty 在主线程；无障碍查询与阻塞等待在指定 worker；VM 不阻塞主线程。

对动作区分 requested/dispatched/observed，不以 click() 返回真认定业务成功。取证记录目标包、会话复核信息、截图哈希与时间窗；可见气泡也不直接证明服务器或对端收到。

### 12.4 其他平台能力

截图按 API/权限选择 Accessibility screenshot 或 MediaProjection 等可用路径，显式处理用户授权；OCR 为辅助，不能替代目标身份验证。图片 buffer 与文件有数量/字节上限并显式关闭。

HTTP 使用成熟库及允许域名、超时、大小/重定向限制；storage 使用 Room/SQLite，不在 Scheme 重写数据库。密码算法使用平台库与 Keystore，不自行实现 AES。floaty/XML 由新适配实现，AutoX JS XML DSL 不自动兼容。

Activity/Service 生命周期由 Android 管理，脚本收到事件而不拥有系统权限。无障碍被禁用、设备锁屏、屏幕不可交互时上报 unavailable/paused，不默认继续发送任务。

## 13. WorkBot Scheme 模块设计

```text
workbot/config.scm                配置、部署与版本
workbot/lifecycle.scm             启动、退出与资源
workbot/overlay.scm               悬浮按钮
workbot/device-monitor.scm        锁屏和亮灭屏
workbot/transport.scm             旧协议 / 新网关
workbot/queue.scm                 队列策略与状态
workbot/executor.scm              认领、运行与回调
workbot/selectors/*.scm           企微版本配置
workbot/navigation.scm            搜索、返回与页面复核
workbot/messages.scm              发消息、撤回与转发
workbot/contacts.scm              好友资料与操作
workbot/groups.scm                群操作
workbot/moments.scm               朋友圈
workbot/login.scm                 登录状态与账号
workbot/history.scm               会话读取、去重与上报
workbot/compat/js.scm             JS 语义兼容
workbot/profiles/*.scm            clean / run / corrected
```

流程策略用 Scheme，Android/SQLite/网络为宿主原语。对业务作者提供高层接口，同时保留原子 UI 原语；版本变化改 selector/navigation 层，不能在 39 个处理器分别散布 depth 补丁。

## 14. 全量指令与外围行为复刻清单

以下编号按前次重建分析列入验收；最终以冻结源码分发器导出清单核对，不以函数中文名代替完整参数规范。

### 14.1 消息

- 10000：撤回，长按目标消息、菜单、时限/失败状态和历史上报。
- 10001：文本，目标定位、空内容、输入切换、发送与 historyLog。
- 10002：图片，下载、解码、分享/选图、清理与失败状态。
- 10006：文件，下载、URI/权限、系统分享与应用选择。
- 10010：快捷菜单消息。
- 10011：小程序卡片；10012：链接卡片，分别保留参数和 UI 路径。
- 10013：引用回复，目标匹配、引用内容和发送。
- 10014：批量转发，收件人数上限、选择顺序与部分失败。

### 14.2 联系人

- 20002：删除好友；20004：备注；20005：手机号添加。
- 20006：手机号搜索资料；20007：单向好友清理。
- 20008：手机号查好友/昵称头像；20009：好友分组。
- 20016：新版好友/群资料。

### 14.3 朋友圈与群

- 30000：朋友圈文本/图片及发布结果。
- 50001：建群；50008：解散/删除群；50009：@成员/所有人。
- 50010：群列表；50011/50012/50013：三种入口的群成员备注。
- 50014：群二维码；50015/50016：增删成员两入口。
- 50017：扫码进群；50018：群公告。

### 14.4 账号、登录与系统

- 100003：账号信息；100004：切换主体与复核/重启。
- 100006：登录二维码；100007：短信码；100008：登录状态。
- 100009：退出；100010：元素截图。
- CLEAR_TASK_QUEUE、READ_MSG。
- 外层 STOP_SCRIPT、START_SCRIPT、DEBUG_LOGS，及 init/config/receive/heart/close 的路由。

### 14.5 无编号但必须复刻

启动顺序、企微版本白名单、新 UI offset、机器人 ID 持久化/变更、悬浮球拖动/吸边/状态、音量键退出、登录监控、空闲收消息、autoName/autoRead/msgLevel/faster/autoJoinGroup/autoBatchSending、授权与安全存储、网络恢复、队列迁移/清理、临时文件、退出释放。

第三方 CryptoJS/Babel/regenerator 不是必须用 Scheme 逐行复刻的业务功能；比较其被使用部分的输入输出，算法交给宿主成熟库，async 编译帮助由新 VM 取代。

## 15. JS 语义兼容和 clean/run 差异

### 15.1 兼容模块

集中实现必要的 `js:truthy?`、宽松/严格比较、ToString/ToNumber、null/undefined、缺属性、数组与对象字段、JSON 丢弃/转换规则、正则与日期。枚举需要的输入域，不试图在 Scheme 内重新实现完整 ECMAScript。

旧指令使用严格 switch：数字与字符串不同；兼容 profile 保持。corrected profile 对已知数字指令允许哪些规范化需明确 schema，而合法字符串控制命令保留。JSON 内整数/浮点的类型归一不应无意影响旧比较。

Java regex 与 JS regex 在 flags、Unicode、反向引用和替换语义不同。提取所有实际模式和输入向量，必要时做显式转译或实现受限匹配器，不能直接宣称 Pattern 等价。中文显示名精确匹配、特殊正则字符和括号必须覆盖。

旧 SimplePromise 可能同步调用 then，与标准 JS Promise 微任务不同。参考 oracle 应是原载荷/Rhino实际行为，不用 Node 标准 Promise替换后认作原语义。可构造兼容事件调度器记录 happens-before；无法保证相同时声明差异并测试业务等价。

### 15.2 run 的变化

已知四个变化模块：入口增加 PROBE、锁屏熄屏保持运行、移除退出 stopAll；core 调试默认开、初始运行状态变化、部分 WS 守卫、授权失败继续/重验、退出清理修改；悬浮窗位置修正；crypto shim Android 适配。

每项写独立 feature 与测试，不能用全局 `isRunMode` 到处散落判断。区分“复现所写代码”和“复现注释意图”：例如清理表达式先将 wsConnected=false 后仍 shutdown threads，应以实际控制流为兼容基线。

### 15.3 修复 profile

corrected 的差异清单至少包括：未知指令失败、帧/schema/身份校验、结果 outbox、暂停认领守卫、超时取消、WS 状态机、严格联系人歧义拒绝、安全授权配置与默认脱敏。每个修复有旧行为用例、新行为用例和迁移说明。

## 16. 业务 DSL：发挥 Lisp 优势

示例仅为设计语法，不代表已有标准或实现：

```scheme
(define-workflow send-text-workflow
  (input task)
  (phase locate
    (let ((chat (workbot:open-exact (task-recipient task))))
      (workflow:remember 'conversation (conversation-id chat))))
  (phase verify
    (workbot:verify-target task))
  (phase send
    (with-ui-session
      (workbot:set-message (task-message task))
      (workbot:click-send)))
  (phase evidence
    (workbot:collect-evidence task)))
```

宏展开为显式状态机、span、取消检查和阶段 journal 调用；状态声明只存可序列化业务值，拒绝保存节点/port/closure/continuation。宏不是事务，不能保证 click 与落盘原子性。

每个阶段声明效果：pure/read-ui/mutate-ui/network/file；声明重试策略 safe/reconcile-only/never-automatic。UI 有副作用阶段默认 reconcile-only；补偿动作需显式定义，不能自动“撤回”来猜测恢复。

开发体验提供 macroexpand、disassemble、dry-run-driver、selector-inspect、workflow trace；生产关闭任意 eval 控制台，受控诊断只读。

## 17. 队列、持久化与不确定性

### 17.1 两套队列语义

兼容层复刻原 SQLite `remote_tasks` 双队列、唯一键、important 排序、running 恢复、最大重试、robotId 切换清理。该层只用于旧行为验收，不与修复队列共享同一文件。旧成功删除记录的长期去重缺口要保留为已知差异。

产品层采用 Room inbox、attempt journal、result/event outbox。数据库是真值，内存 Actor/VM 不是；SQL 事务把结果与待投递事件一起提交。

### 17.2 端侧 schema

```sql
CREATE TABLE task_inbox (
  task_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL,
  device_id TEXT NOT NULL, account_id TEXT NOT NULL,
  payload_hash TEXT NOT NULL, payload BLOB NOT NULL,
  schema_version INTEGER NOT NULL, business_profile TEXT NOT NULL,
  session_epoch INTEGER NOT NULL, execution_epoch INTEGER NOT NULL,
  status TEXT NOT NULL, phase TEXT, deadline_at INTEGER NOT NULL,
  received_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);
CREATE TABLE attempts (
  attempt_id TEXT PRIMARY KEY, task_id TEXT NOT NULL,
  operation_id TEXT, phase TEXT NOT NULL, state TEXT NOT NULL,
  started_at INTEGER, finished_at INTEGER, evidence_ref TEXT,
  error_code TEXT, FOREIGN KEY(task_id) REFERENCES task_inbox(task_id)
);
CREATE TABLE result_outbox (
  event_id TEXT PRIMARY KEY, task_id TEXT NOT NULL,
  payload BLOB NOT NULL, payload_hash TEXT NOT NULL,
  state TEXT NOT NULL, next_retry_at INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0
);
```

示例省略索引与迁移：必须补 status/deadline/pending 索引、FK 配置、保留策略、数据库版本和校验。任务 ID 在租户是否全局唯一要冻结；若非全局，主键改 `(tenant_id,task_id)` 并同步所有关联，不能仅按 taskId 隔离。

### 17.3 状态与恢复

```text
server_accepted -> dispatched -> device_persisted -> executing
                    -> succeeded / failed / cancelled / uncertain
result_pending -> result_acked
```

执行状态和投递状态分字段/表。API accepted 必须晚于服务器事务提交；设备 persisted 晚于 Room 提交。相同 ID 相同 hash 返回原状态，hash 不同报冲突。

点击前记录意图，点击后记录观察证据；两者之间崩溃不能判定是否发生。恢复到 uncertain，检查目标会话、消息内容、时间和历史变化；确认不足则人工处理。没有端到端 exactly-once 承诺。

结果已执行但网络失败只重投 result。服务端租约过期不能说明旧设备已停止，不立即将未确认终止任务分配给另一设备。executionEpoch/sessionEpoch 拒绝新进入的旧命令，不能逆转已经执行的动作。

### 17.4 时钟、容量与留存

任务业务 deadline 使用服务端墙钟与偏差检查；端侧操作 timeout 使用 monotonic clock。重启后 monotonic 值失效，从墙钟有效期与保守剩余预算恢复。断电、时钟回拨测试必须覆盖。

inbox/outbox/证据都有条数、字节和时间保留限制；容量满明确拒绝，不静默删除未确认结果。已完成幂等凭据保留期限要与上游重试期限一致，超过期限的 taskId 重用有明确拒绝/归档规则。

## 18. Parrot 服务端与 MQTT

### 18.1 服务组成

首期模块化单体：API/Auth、TaskService、DeviceGateway、Parrot Actors、PostgreSQL、对象存储、Webhook worker。不要为一台手机引入多个分布式调度层。

- AccountActor：账号排序、节流、暂停、登录状态。
- DeviceSessionActor：连接代次、设备能力、任务认领和健康。
- WorkflowActor：有界活跃任务与阶段推进，不为全部历史任务常驻 Actor。
- EventRouterActor：消息订阅、去重和 webhook。

单设备一个 UI 所有者。多节点扩展用数据库 claim/lease/version 做归属，Parrot discovery 只是定位工具，不能代替业务互斥。确认框架当前能力范围后再用集群恢复，不能把设计文档当已实现保证。

### 18.2 Broker 与主题

使用外部 MQTT Broker，如 EMQX/Mosquitto；Parrot bridge 不是 Broker。设备主动出站 TLS。

```text
v1/tenants/{tenant}/devices/{device}/commands
v1/tenants/{tenant}/devices/{device}/acks
v1/tenants/{tenant}/devices/{device}/results
v1/tenants/{tenant}/devices/{device}/events
v1/tenants/{tenant}/devices/{device}/status
v1/tenants/{tenant}/devices/{device}/result-acks
```

Broker ACL 将认证身份绑定租户/设备，仅允许自身上下行方向；payload 自报 deviceId 不能提供认证。多服务实例共享订阅时仍以数据库控制设备/任务归属。

### 18.3 QoS 与会话

命令和结果 QoS1；应用级去重与确认必需。QoS2 不保证 UI 恰好一次。命令不 retained，MQTT5 message expiry 配合 payload deadline；3.1.1 客户端依赖应用有效期。持久会话有 session expiry 与磁盘额度，Broker 配置重启恢复需实测。

status 可 retained + LWT，带 sessionEpoch/bootId，消费方忽略旧代次。连接在线、Agent ready、权限 ready、企微登录是独立字段，不能仅凭 LWT 显示可执行。

PUBACK 是 MQTT 一跳传输确认，不代表 Room 提交、Parrot 业务完成或企微送达。接收回调若提前 PUBACK，Room 落盘前崩溃可能丢应用消息；服务器必须在未收到 device_persisted 时以同 taskId 重投。选择客户端前验证手动 ack/持久会话语义，不假定各库一致。

### 18.4 Envelope 与编码

新协议建议 Protobuf，字段包括 protocolVersion、messageId、taskId、tenantId、deviceId、accountId、sessionEpoch、executionEpoch、attemptId、command、payloadHash、deadlineAt、traceId、scriptHash、profile。payload hash 对明确的 canonical bytes 计算；Protobuf 多种等价编码不能直接混当一致内容，规范化 schema 后统一编码或保存原始规范字节。

设备用 Java/Kotlin MQTT client + 业务协议即可，不需要完整 Parrot Wire/集群协议。未来 Android Parrot Lite 单独实现必要 Wire 子集并跑 Rust 互操作向量。现有 MQTT bridge 的 QoS1 要求 DURABLE_KEY envelope、固定 producer/session/receiver/path；不能直接拿任意 Protobuf payload 接上，也不能自动认为已覆盖下行。网关适配负责协议转换与可靠状态，不长期等待手机 UI 完成再卡住所有 MQTT ack。

Parrot CloudProxy 可用于适配 Wire 的内部流，持久接受晚于 WAL sync，但不与 PostgreSQL 自动形成一个事务。首期服务器使用 PostgreSQL task + dispatch outbox 同事务，网关发布后按业务 ack 推进；若复用 CloudProxy，通过稳定 ID 幂等桥接并测试双存储故障，不同时维护两个模糊真值。

### 18.5 服务端数据

devices、device_credentials、accounts、device_bindings、tasks、attempts、task_events、dispatch_outbox、webhook_outbox、script_packages、selector_profiles、evidence_metadata。每个写操作带租户边界；凭据轮换、设备撤销和迁移要可审计。

图片/文件使用限时授权 HTTPS URL，不放入 MQTT 大帧；限制大小、类型、哈希、重定向、可访问地址和解码预算，防止 URL 滥用及压缩炸弹。消息/证据有访问和保留策略。

## 19. 错误、观测与开发工具

错误域：reader、expand、binding、type、arity、numeric、module、permission、resource、host、timeout、cancelled、protocol、uncertain、internal。标准 condition 对象与产品错误码分层；可恢复语言错误允许 guard，internal/严重资源失败需重建执行实例。

结构化错误包含 source revision/span、展开栈、Scheme 调用栈、宿主函数、task/attempt/phase、设备代次、原因链和可重试策略；生产脱敏，不把手机号、全文聊天和凭据默认进入日志。

轨迹事件：compile、fiber yield/resume、host-call start/end、UI intent/observation、queue transition、mqtt publish/application ack、outbox retry。跨 Parrot/网关/设备使用 taskId/attemptId/traceId，不要求所有平台 tracing SDK 相同。

工具最低集合：formatter、reader/宏展开检查、disassembler、REPL、LSP 补全/定义/错误、host API metadata、selector 快照回放、任务时间线、JS→Scheme 迁移示例。LSP 位置使用 UTF-16，诊断引用原 source hash。

默认业务库隐藏低级 depth；高级用户可开启 selector inspector，不能通过抢占无障碍的外部工具破坏设备运行。历史设备对 dump 的风险按机型验证，不泛化成 Android 绝对规则。

## 20. 安全与发布

脚本包 manifest 包含入口、语言/宿主/业务 profile、依赖 hash、能力、网络/文件范围、资源预算、签名与最低 Agent 版本。生产只执行可信发布者签名包；签名防来源篡改，不证明脚本安全。

默认禁任意 Java reflection、shell/process、动态 native 插件和从网络 require。宿主 capability 每次调用检查，闭包或 continuation 不能缓存过期授权。编译期与运行期分开计费与授权。

Android Keystore 管理设备秘密；TLS 验证服务器，按部署选 mTLS 或短期设备凭据；不复制旧 SEC/XOR/固定 IV 作为安全方案。兼容授权协议仅受控连接配置，禁止默认回退官方服务。

APK/脚本升级：构建不可变包，记录版本与 SHA-256；stage 校验签名/依赖 → 编译验证 → 无 UI smoke → 安全点切换。活跃任务绑定原脚本 hash，不在中途替换。数据库迁移有前向兼容/备份策略，不将 APK 回滚等同数据库回滚。

AutoX 旧非 DEBUG 工程可能依 build_id 复用解压内容；打包必须记录并更新它，但应用数据是否重置依据真实路径，不假定改 build_id 必然清除 Room/storage。保持 JS 回滚包与 Scheme 包可追溯，不默认让两引擎同时操纵 UI。

许可证需单独梳理 AutoX、FlowBot 载荷、Steel 参考代码和依赖的实际授权；原 README 的混合许可描述不能替代合法来源检查。这不是暂停技术设计的前置审批。

## 21. 行为等价验证方案

### 21.1 Oracle 建立

固定 JS 原始明文、clean、run 文件、部署覆盖、Rhino/AutoX APK、企微版本、设备型号及摘要。现有缺失 APK/日志不伪造补齐；新采集材料标明新基线。读取 clean 代码函数体和 run 装配入口分别适配，不无意测错目标。

为每个处理器整理输入 schema、初始页面、登录/绑定状态、查询/动作、上报帧、队列变化、错误和副作用。静态 dispatch 清单自动导出与测试矩阵比对，避免遗漏外围命令。

### 21.2 三层差分

1. 纯语义：JS兼容函数与 Scheme 适配针对相同值向量对比。
2. 模型执行：JS 与 Scheme 共用外部 driver fixture、虚拟时钟、网络录制、可控调度和数据库种子，比较有序轨迹与最终状态。
3. 真机：固定版本，独立运行参考/新实现，对照业务结果和证据；破坏性操作只用专用测试联系人/群/账号。不能同时在真实聊天上重复发送来做无副作用影子测试。

页面 fixture 优先来源于独立真实采集和人工标注；不只从待测 selector 反推。加入 selector 变异、错误目标和故障树，确保测试能失败。拟真树通过不能代替真机验收。

### 21.3 比较器

匹配输入输出类型、字段缺失与 null、数字/字符串、UI动作目标、必要 happens-before、队列内容、结果和资源关闭。时钟/随机/句柄归一化规则在测试前冻结。字段顺序是否业务可见单独规定，不能为了字节相等错误保留 JSON 对象顺序要求。

原 JS 错误文案若被上游依赖则兼容；新诊断另放 diagnostics，避免污染旧协议。并发比较合法轨迹集合与顺序约束，不能把特定线程调度记录当唯一正确执行。

### 21.4 通过条件

39 分发 case 加全部外围场景逐项有 positive/negative/timeout/cancel/登录守卫/身份守卫。clean 与 run 分别通过其矩阵；corrected 每项差异都有显式验收。通过率不是普遍等价证明，报告保留未覆盖输入域、设备与竞态范围。

## 22. 语言与平台测试门禁

- Reader：Unicode、转义、共享/循环 datum、极值数字、错误恢复、property/fuzz 与打印读取往返。
- 标准：按 R7RS-small 条款建立测试和已知未指定行为策略，不只沿用 Steel 自身测试。
- 宏：卫生、嵌套 ellipsis、局部宏、重命名导入、错误位置、展开资源耗尽。
- VM：深尾递归、闭包 set!、letrec、零/多值、异常后复用、参数/dynamic-wind、multi-shot continuation。
- 异步：超时与完成竞态、重复回调、取消、迟到回调、旧 Fiber/Engine 代次、宿主同步异常。
- 资源：对象计费、缓存、native buffer、port close、监听清理、OOM/failure 后状态。
- Android：无障碍关闭、锁屏/熄屏、后台回收、Activity重建、主线程无阻塞、权限拒绝与截图 API 差异。
- 业务：严格姓名、备注括号、重名、中文/emoji、群分页、登录过期、截图和文件分享。
- 可靠性：Room提交前后崩溃、点击前后崩溃、结果补投、Broker重启、会话恢复、时钟跳变、磁盘满。
- 安全：越权导入、反射、路径逃逸、恶意媒体、编译期宏攻击、能力过期与签名失败。
- 性能：Engine冷启动、编译、首执行、稳态边界调用、UI等待、取消延迟、峰值内存、长驻泄漏。

桌面 JVM 与 Android ART 都执行核心回归；CI 不把缺设备/缺Broker记为成功。真实设备、语言 fixture、互操作和性能分开报告。新设计尚无性能数值，PoC后给设备型号与可复现阈值。

## 23. 实施计划、依赖和交付物

### P0：冻结规范与测试资产

产出 language spec、标准条款清单、Steel兼容清单、host ABI、profile差异、完整指令参数清单、oracle输入输出格式与基线manifest。先做最高风险贯通实验：AutoX新增引擎生命周期、尾调用、挂起恢复、取消后无新点击。

退出条件：可以明确回答每种结果何时持久、哪层可重试、谁拥有屏幕；不能只有架构图。

### P1：Reader、语言核和显式 VM

实现值、闭包、变量、控制流、尾调用、多值、condition、debugMap；桌面 CLI与测试driver。先不做JIT/复杂优化。交付可运行的纯语言程序和语言内存/指令预算。

### P2：R7RS-small 系统补齐

syntax-rules/局部宏、library/导入、数值塔、ports/read/write、lazy、eval/load、continuation/dynamic-wind/parameterize等按清单关闭。可与业务MVP并行，但完整标准称谓必须等待门禁。

### P3：AutoX与 Android Agent

新增source/engine注册、能力桥、UiLease、NodeHandle、Pending操作、权限与生命周期、Room/MQTT。用Scheme实现只读设备信息和无副作用selector测试，再进入专用账号发文本。

### P4：WorkBot纵向切片

配置→绑定→版本检测→搜索→严格复核→发文本→采证→结果补投。同时实现clean兼容和corrected的第一组差异测试。验收断网、重启、取消与uncertain；结果假成功或重复发送仍存在时不扩39指令。

### P5：全量行为迁移

按消息、账号登录、联系人、群、朋友圈推进；每组先模型差分再真机。迁移悬浮球/设备监控/自动消息读取等外围链路，不能只迁移dispatch。run差异在配置中单列。

### P6：工具、Steel扩展和发布

补LSP/formatter/REPL、可移植Steel库兼容、缓存、脚本签名、部署回滚、数据库升级、运维与压力验收。是否加入syntax-case、复杂contract等由真实业务与兼容清单驱动。

此前4–8周MVP、3–6个月主要功能的粗估针对“原生Agent+服务端”，不能直接套用于新增R7RS VM与Steel兼容工作。当前不承诺总周期；用P0/P1实测速度拆分语言、Android、服务端和业务迁移的人力估算。完整标准/宏/continuation需独立责任人，不能算作业务开发顺手完成。

## 24. 详细完成标准

语言完成：每条标准支持状态有证据，明确扩展/未指定政策，核心在JVM/ART通过；Steel profile有冻结输入域、版本与差分结果。

平台完成：无主线程阻塞；取消后不再提交新动作；旧句柄/旧代次拒绝；资源有界；权限能力可测试；脚本升级可回滚且数据迁移明确。

业务兼容完成：39 case与外层事件全覆盖，clean/run分别通过，历史缺陷差异有记录；协议、队列、取证与错误均纳入比较，不能只看回执success。

服务端完成：认证/ACL、任务与outbox事务、设备持久接受、结果确认、Broker恢复、容量限制、审计/备份与租户隔离通过真实集成测试。

产品可靠性完成：点击与持久化窗口明确为uncertain；已执行结果重投不重执行业务；暂停、锁屏、登录过期、进程死亡有可复现实验和运行手册。

## 25. 待冻结的具体决策

以下不阻塞方案落盘，但在编码相应模块前必须关闭：

1. 语言、字节码和产品名称及版本策略。
2. 完整标准目标与MVP子集的具体条款顺序。
3. Steel源码兼容profile包含哪些库/扩展；其错误文案与遍历顺序是否列入契约。
4. AutoX基线commit、Android min/targetSdk、首个真机/企微版本。
5. Java/Kotlin编译工具链、Android可用MQTT客户端及确认语义。
6. taskId跨租户唯一性、业务消息规范化编码、幂等凭据保留期。
7. 生产是否允许远端自定义脚本；可信签名者与能力审批如何运营。
8. corrected是否允许旧数字指令字符串规范化，以及授权策略。
9. 是否有独立采集的企微控件树与可恢复原APK；缺失部分如何重新建立基线。
10. 语言、Android、业务、服务端、测试资产的责任边界与排期。

## 26. 建议的第一组工程任务

先建立纯JVM骨架与无Android driver：reader、Value、显式帧、闭包/尾调用、HOST_CALL/Pending、取消代次、轨迹比较。并在AutoX用最小SchemeEngine执行 `(+ 1 2)`、调用日志、查询控件、取消等待与释放引擎。

随后以10001构建完整oracle：同一输入先在冻结JS执行器录制，再由Scheme回放比较。用数字、字符串、空内容、查无此人、重名、断网、超时、取消和点击后中断组成首批用例。只有贯通这一条路径，才扩大语言与业务功能。

本方案的主要收益应由这组任务证明：更明确的执行语义、更好的诊断和更可控的脚本能力。若最终只得到另一种语法而没有这些改善，就没有兑现自研Scheme VM的价值。

## 附录 A：关键接口与并发不变量

以下是接口草案，实际编码时按纯JVM/Android隔离和目标AutoX接口调整，不将示例直接视为可编译代码。

```kotlin
interface SchemeCompiler {
    fun compile(source: SourceRevision, profile: LanguageProfile,
                dependencies: LibraryResolver, budget: CompileBudget): CompileResult
}
interface SchemeEngine {
    fun start(program: VerifiedProgram, config: RunConfig): ExecutionHandle
    fun inspect(id: ExecutionId): ExecutionSnapshot
    fun pause(id: ExecutionId): ControlReceipt
    fun resume(id: ExecutionId): ControlReceipt
    fun cancel(id: ExecutionId, reason: CancelReason): ControlReceipt
    suspend fun close(): CloseReport
}
interface ExecutionHandle {
    val id: ExecutionId
    suspend fun awaitTerminal(): ExecutionResult
}
interface UiDriver {
    suspend fun query(ctx: UiContext, selector: Selector): List<NodeHandle>
    suspend fun perform(ctx: UiContext, operation: UiOperation): UiObservation
}
```

`ControlReceipt` 仅表示控制请求已接受，不能冒充取消已完成；取消终态由 awaitTerminal/独立事件确认。close 关闭准入、取消任务、等待owned操作和资源回收，报告未确认退出的操作，不无限等待也不假报清理成功。Android进程死亡时不保证close执行。

运行不变量：

1. 每个Fiber只处于一个调度队列，RUNNING只能由Engine owner持有。
2. 每个PendingToken最多一个terminal恢复事件，晚到事件不能修改已完成Fiber。
3. 每个UiLease只允许一个executionEpoch提交动作；校验与动作排队必须在同一所有权边界内完成。
4. 取消先撤销新动作准入，再触发底层cancel；释放UiLease晚于已提交操作的终止/不确定性处理。
5. Room事务中不等待MQTT或UI；网络和UI线程不持有数据库锁调用Scheme。
6. Scheme异常展开不能调用任意阻塞的Java析构；资源关闭通过有截止时间的宿主操作管理。
7. 任何异步回调都不直接触发新任务认领，认领由单一执行协调器检查暂停、登录、权限和租约。

Pending状态转换伪代码：

```text
complete(token, result):
  enqueue OwnerEvent(token.engineGeneration, token.id, result)

owner.receive(event):
  if generation mismatch: record late-event; return
  pending = registry[event.tokenId]
  if pending absent or pending.state != WAITING: return
  if cancellation revoked operation:
     transition WAITING -> CANCELLED; do not submit further UI
  else:
     transition WAITING -> COMPLETED
     restore expected value arity or raise condition
     mark fiber RUNNABLE
  dispose token-owned timer/listener exactly once
```

deadline与完成同时到达时，以owner线性化处理顺序为准，结果记录该顺序。对已经发生的外部动作，即使timeout先处理也不能把动作宣称为未发生。

## 附录 B：10001 文本任务逐阶段规格

### B.1 兼容输入和新输入

旧帧以冻结源码为准；典型输入：

```json
{
  "type": 10001,
  "robotId": "DEVICE_BOUND_ROBOT",
  "data": {
    "taskId": "task-unique-id",
    "robotId": "DEVICE_BOUND_ROBOT",
    "searchText": "联系人完整显示名",
    "message": "消息内容"
  }
}
```

新API增加tenant/account、profile、deadline、scriptHash和executionEpoch；网关做旧帧适配。兼容测试包含外层/内层robotId冲突，以原resolve逻辑为oracle；corrected模式在入库前拒绝冲突，不默默选择其中一个。

### B.2 阶段约束

1. Validate：确认任务结构、当前绑定、非空内容与长度、期限、profile和scriptHash。参数错误不得返回执行成功。
2. Admit：Room以ID/hash提交；重复相同任务返回已有状态；新任务等待UiLease和账号调度许可。
3. Prepare：检查权限、屏幕和登录；导航企微主页。版本不支持时失败/挂起，不能盲目使用最接近的selector。
4. Search：点击搜索、输入完整显示名、等待有界结果；匹配规则按profile，转义正则特殊字符，不擅自扩展为模糊匹配。
5. Verify：读取目标详情/会话上下文，拒绝歧义；若旧clean仅检查名称，兼容模式记录该弱保证，corrected增加复核。
6. Input：查询当前输入框、切换文字模式、setText后校验可见内容。空值、截断和键盘弹窗有单独失败码。
7. SendIntent：持久化attempt/operation意图，重新检查取消和执行代次，再提交点击。不能在此持有SQLite事务等待点击。
8. Observe：等待输入清空/新气泡等证据，区分动作提交与观察到发送；证据不足返回uncertain而非猜测失败后重发。
9. Collect：根据兼容profile采集historyLog，corrected把敏感内容与结果分权限上报；无法采集历史不必自动推翻已经确认的动作，结果含证据级别。
10. CommitResult：结果与result_outbox同事务提交，释放UiLease。结果重试只读outbox，不重新进入SendIntent。

### B.3 最小故障矩阵

- Validate前断开：服务器同ID重发，设备尚未接受。
- Room提交后、设备确认前崩溃：重发返回原任务，不新增。
- 搜索后取消：不点击发送，取消终态后才允许下一任务。
- 意图落盘后、点击前崩溃：恢复不能仅据意图断言已点击；按reconcile策略处理。
- 点击后、观察前崩溃：uncertain；不得自动重发。
- 结果落盘后、MQTT发送前崩溃：恢复outbox，任务不重执行。
- 服务器已保存结果、result-ack丢失：设备同eventId重投，服务器幂等返回确认。
- 迟到的旧任务UI回调：拒绝其后续动作；若观察到已发生效果，附加诊断事件，不污染新任务的结果变量。

## 附录 C：能力声明与资源示例

```json
{
  "name": "workbot-send-text",
  "packageVersion": "1.0.0",
  "entry": "main.scm",
  "languageProfile": "r7rs-small-v1",
  "hostApiVersion": "autox-host-v1",
  "businessProfile": "workbot-corrected-v1",
  "capabilities": ["ui.read", "ui.input", "ui.click", "task.storage"],
  "limits": {
    "maxSourceBytes": 1048576,
    "maxLogicalHeapBytes": 16777216,
    "maxFibers": 4,
    "maxRuntimeMs": 180000
  },
  "networkAllowlist": [],
  "fileRoots": []
}
```

资源数值为起始实验配置，不是设备通用上限。标准库、源码、编译缓存、媒体、JNI/Android对象和日志分别计费；只限制Scheme对象不能防止宿主分配失控。manifest还需依赖摘要、签名、最低Agent版本和schema版本，示例省略的字段不可在正式发布包中省略。

## 附录 D：跨实现测试资产格式

每个case至少保存：caseId、参考profile、源码/包摘要、任务字节、初始storage/数据库、设备/企微能力、页面fixture来源、虚拟时间/随机输入、网络响应、故障注入点、期望轨迹与允许归一化规则。

事件统一为versioned记录：sequence、logicalTime、source、operation、task/attempt、selector/handle关系、输入、结果/错误、资源计数、span。独立记录实际时钟用于性能，不混入确定性比较。

测试runner输出：通过case、失败首差、双方最小trace片段、最终状态差异、未执行/跳过范围及原始证据目录。支持把失败case缩减为最小语言或driver场景，避免每次排查都运行完整手机流程。

所有报告标记执行环境和事实范围：桌面纯VM、模型driver、Android引擎、真实企微、Broker集成分别列示。没有真机或Broker时测试应明确未执行；不能以mock通过替代对应门禁。
