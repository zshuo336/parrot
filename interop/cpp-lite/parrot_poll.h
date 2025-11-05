// Parrot 跨平台轮询后端（DEV_08 性能优化）——poll/epoll/kqueue 三态选择。
//
// 选择策略（§性能适配矩阵）：
//   1. 编译期宏 PL_FORCE_POLL / PL_FORCE_EPOLL / PL_FORCE_KQUEUE 强制；
//   2. 未指定强制宏时：__linux__ → epoll，BSD/macOS → kqueue，
//      其余 → poll（POSIX 兜底——嵌入式/机器人 RTOS 通用）；
//   3. 运行时探测（pl_pollset_backend() 可观测）：epoll/kqueue 创建失败
//      （fd 上限/内核裁剪）自动降级 poll。
//
// 适用性（手机/机器人/边缘设备）：
//   - poll：任何 POSIX（含无 epoll 的 musl 精简内核）；
//   - epoll：Linux 服务器/Android 主流内核；
//   - kqueue：macOS/iOS/BSD。
// 所有后端共享 pl_pollset_ctl/pl_pollset_wait API。

#ifndef PARROT_POLL_H
#define PARROT_POLL_H

#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define PL_POLL_MAX_FDS 64   // 网关侧连接规模（lite 客户端形态足够）
#define PL_POLL_TIMEOUT_INF -1

typedef struct pl_pollset pl_pollset;

/// 创建轮询集（backend 自动探测；失败返回 NULL）。
pl_pollset *pl_pollset_create(void);

/// 销毁。
void pl_pollset_destroy(pl_pollset *ps);

/// want_read=true 添加/更新可读关注，false 删除；重复操作幂等。
int pl_pollset_ctl(pl_pollset *ps, int fd, bool want_read);

/// 等待事件（timeout_ms：-1 永久）。返回就绪 fd 数；-1 错误。
/// EINTR 返回 0；每次等待先清空上次结果，硬错误保留系统 errno。
/// poll 后端将 POLLNVAL 报为就绪，调用方应删除已失效的登记。
int pl_pollset_wait(pl_pollset *ps, int timeout_ms);

/// 第 i 个就绪 fd（i < wait 返回值）。无则 -1。
int pl_pollset_ready_at(pl_pollset *ps, int i);

/// 当前生效后端名（"epoll" / "kqueue" / "poll"——可观测/测试断言用）。
const char *pl_pollset_backend(pl_pollset *ps);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // PARROT_POLL_H
