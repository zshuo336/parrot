// Parrot C++ Lite（DEV_04 §5 / 06 P4.3 / 07 §9 混合体路径）。
//
// 协议子集同 TS lite：HANDSHAKE/HANDSHAKE_ACK/HEARTBEAT/HEARTBEAT_ACK/
// ASK/REPLY/REPLY_ERR/TELL + receptionist 注册。单头文件 + 单实现，
// 帧解析 memcpy+reinterpret（固定头 28B 布局，07 §2.1）——与
// Rust/JVM/Python/Erlang/TS 逐字节一致（golden vectors 冻结）。
//
// C ABI（机器人主控混合体：Rust 主控 + C++ 执行器进程内直连）：
//   符号零破坏演进——只增不改不删；出参内存调用方 pl_free。

#ifndef PARROT_LITE_H
#define PARROT_LITE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// ── 版本 ────────────────────────────────────────────────
#define PARROT_LITE_VERSION_MAJOR 1
#define PARROT_LITE_VERSION_MINOR 0
#define PARROT_LITE_VERSION_PATCH 0

// ── Wire 1.0 常量（与 Rust frame.rs 冻结值一致）─────────
#define PL_WIRE_VERSION 0x01
#define PL_MAX_FRAME_LEN (16u * 1024u * 1024u) // 16 MiB
#define PL_DEFAULT_HOP_LIMIT 8

// frame_type
#define PL_FT_HANDSHAKE 0x01
#define PL_FT_HANDSHAKE_ACK 0x02
#define PL_FT_HEARTBEAT 0x03
#define PL_FT_HEARTBEAT_ACK 0x04
#define PL_FT_ASK 0x10
#define PL_FT_REPLY 0x11
#define PL_FT_REPLY_ERR 0x12
#define PL_FT_TELL 0x13
#define PL_FT_STOP 0x14
#define PL_FT_ERROR 0x7F

// flags
#define PL_FLAG_COMPRESSED_ZSTD 0x0001
#define PL_FLAG_TRACING 0x0002
#define PL_FLAG_BATCH 0x0004
#define PL_FLAG_URGENT 0x0008
#define PL_FLAG_TELL_ACK 0x0010
#define PL_FLAG_APP_ENCRYPTED 0x0020

// 错误码（ErrCode——与 Rust error.rs 一致）
#define PL_ERR_ACTOR_NOT_FOUND 1
#define PL_ERR_TIMEOUT 2
#define PL_ERR_STOPPED 3
#define PL_ERR_NOT_REMOTABLE 4
#define PL_ERR_CODEC_ERROR 5
#define PL_ERR_UNKNOWN_TYPE_KEY 6
#define PL_ERR_ROUTE_UNREACHABLE 7
#define PL_ERR_CONNECTION_LOST 8
#define PL_ERR_DIRECTORY_STALE 9
#define PL_ERR_OVERLOADED 10
#define PL_ERR_NO_COMMON_CODEC 11
#define PL_ERR_PROTOCOL_VIOLATION 12
#define PL_ERR_FORBIDDEN 13

// ── C ABI（pl_* 前缀——符号表冻结，只增不改不删）────────
// 返回 0 成功；负数为错误码（-1 参数错 / -2 未连接 / -3 超时 /
// -4 协议错 / -5 内存）。

/// 连接 parrot 节点（url 如 "parrot://node:7000"；tls_dir 可 NULL=明文）。
int pl_connect(const char *url, const char *node_id, const char *tls_dir);

/// receptionist 注册（key/path/caps_json——幂等，重连后自动重注册）。
int pl_register(const char *key, const char *path, const char *caps_json);

/// ASK（同步等待 REPLY）。成功时 *out 指向 malloc 内存，调用方 pl_free。
int pl_ask(const char *target_path, const char *type_key,
           const uint8_t *payload, size_t len,
           uint8_t **out, size_t *out_len, int timeout_ms);

/// TELL（fire-and-forget，至多一次）。
int pl_tell(const char *target_path, const char *type_key,
            const uint8_t *payload, size_t len);

/// 事件泵：收 TELL/ASK 派发给 on_ask 回调（阻塞至 timeout_ms 或事件到）。
typedef void (*pl_on_ask_t)(const char *from_path, const char *type_key,
                            const uint8_t *payload, size_t len,
                            uint64_t cid);
void pl_poll(int timeout_ms);

/// 启动后台泵线程（机器人主控形态：免手动 pl_poll；回调线程=泵线程）。
/// 幂等；返回 0 成功 / -5 线程创建失败。
int pl_pump_start(void);

/// 停止后台泵线程（幂等；pl_close 前调用更优雅）。
void pl_pump_stop(void);

/// 编译期轮询后端描述（"epoll"/"kqueue"/"poll"——auto 时含降级提示，
/// 如 "kqueue-or-poll"；运行时实际生效以 pl_pollset_backend() 为准）。
const char *pl_backend_name(void);

/// 注册 on_ask 回调（pl_poll 派发用；NULL 清除）。
void pl_set_on_ask(pl_on_ask_t cb, void *userdata);

/// 释放 pl_ask 出参（防双 free——07 §9 事故点）。
void pl_free(void *p);

/// 断开（幂等）。
void pl_close(void);

#ifdef __cplusplus
} // extern "C"
#endif

#ifdef __cplusplus
// ── C++ 层（可选——C ABI 之上的薄便利层）────────────────
#include <functional>
#include <string>
#include <vector>

namespace parrot::lite {

struct Frame {
  uint8_t ft = 0;
  uint16_t flags = 0;
  uint64_t cid = 0;
  uint8_t hop_count = 0;
  uint8_t hop_limit = PL_DEFAULT_HOP_LIMIT;
  std::string path;
  std::string type_key;
  std::vector<uint8_t> payload;

  // 线上编码（4B body_len 前缀 + 28B 固定头 + TLV 变段）
  std::vector<uint8_t> encode() const;
};

// 帧解析器（游标式半包——DEV_08：消费只推进游标，压缩惰性摊销）
class FrameDecoder {
public:
  void feed(const uint8_t *data, size_t len);
  // 完整帧 → true（out 填充）；半包 → false 不消费
  bool next(Frame *out, int *err);

private:
  std::vector<uint8_t> buf_;
  size_t pos_ = 0;      // 已消费游标
  void compact();       // 前缀过半才 memmove（批量帧 O(N) 总代价）
};

uint64_t new_cid(); // 随机（splitmix64 种子——仅客户端用途）

} // namespace parrot::lite
#endif // __cplusplus

#endif // PARROT_LITE_H
