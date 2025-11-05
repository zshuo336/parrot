// Parrot C++ Lite（DEV_04 §5 / 06 P4.3 / 07 §9 混合体路径）。
//
// 协议子集同 TS lite：HANDSHAKE/HANDSHAKE_ACK/HEARTBEAT/HEARTBEAT_ACK/
// ASK/REPLY/REPLY_ERR/TELL + receptionist 注册。单头文件 + 单实现，
// 帧解析使用有界字段读取（固定头 28B 布局，07 §2.1）——与
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

/// 连接 parrot 节点（url 如 "parrot://node:7000"）。当前实现只支持明文；
/// parrot:// 和 tcp:// 均支持 host:port 或 [IPv6]:port，端口范围 1..65535。
/// 不接受用户信息、路径、查询、片段、空白或 IPv6 zone；无效 URL 返回 -1，
/// 不解析域名、不替换已有连接。合法的新连接尝试仍会取消旧连接。
/// tls_dir 非 NULL 返回 -1，绝不静默降级。建立连接成功须通过完整 Wire 握手。
/// DNS、所有地址连接和握手共享 5 秒预算；pl_close 或新连接取消旧尝试。
/// 最多一个 DNS 工作线程；系统解析不可强制取消，退出可能等待它完成。
int pl_connect(const char *url, const char *node_id, const char *tls_dir);

/// receptionist 注册（key/path 幂等；caps_json 当前未发送）。
/// 本客户端不自动重连；显式 pl_connect 后调用方须重新注册。
/// 排队及写入共享 3 秒预算，超时返回 -3。
int pl_register(const char *key, const char *path, const char *caps_json);

/// ASK（同步等待 REPLY）。成功时 *out 指向 malloc 内存，调用方 pl_free。
/// 失败清空出参。NULL payload 仅在 len=0 时合法，编码须符合协商帧上限。
/// timeout_ms 覆盖编码、写锁排队、发送及回复；<=0 使用 3 秒。
/// 未写任何字节的超时保留连接；半帧超时关闭该代，不重试，交付状态未知。
/// 同步用户回调的运行时间不可由库强行中断。
int pl_ask(const char *target_path, const char *type_key,
           const uint8_t *payload, size_t len,
           uint8_t **out, size_t *out_len, int timeout_ms);

/// TELL（fire-and-forget，至多一次）。排队及写入共享 3 秒预算。
/// 成功仅表示帧已写入本地 socket；超时/断连不代表远端没有执行。
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

/// 停止后台泵线程。外部调用等待线程退出；泵回调内调用只请求停止，
/// 下次外部 stop/start 回收旧线程，期间回调内 start 返回 -2。
void pl_pump_stop(void);

/// 编译期轮询后端描述（"epoll"/"kqueue"/"poll"——auto 时含降级提示，
/// 如 "kqueue-or-poll"；运行时实际生效以 pl_pollset_backend() 为准）。
const char *pl_backend_name(void);

/// 注册 on_ask 回调（pl_poll 派发用；NULL 清除）。
void pl_set_on_ask(pl_on_ask_t cb, void *userdata);

/// 释放 pl_ask 出参，NULL 可用；每个非空指针仅允许释放一次。
void pl_free(void *p);

/// 断开（幂等），取消当前连接尝试，唤醒在途请求及等待/正在发送的调用。
void pl_close(void);

#ifdef __cplusplus
} // extern "C"
#endif

#ifdef __cplusplus
// ── C++ 层（可选——C ABI 之上的薄便利层）────────────────
#include <functional>
#include <stdexcept>
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
  uint32_t sequence = 0;

  // 线上编码（4B body_len 前缀 + 28B 固定头 + TLV 变段）
  std::vector<uint8_t> encode() const;
};

// 帧解析器（游标式半包——DEV_08：消费只推进游标，压缩惰性摊销）
class FrameDecoder {
public:
  // Bounds include the fixed body header; invalid construction is a caller error.
  explicit FrameDecoder(uint32_t max_frame_len = PL_MAX_FRAME_LEN) : max_frame_len_(max_frame_len) {
    if (max_frame_len < 28 || max_frame_len > PL_MAX_FRAME_LEN)
      throw std::invalid_argument("frame limit must be between 28 bytes and 16 MiB");
  }
  // Preserve buffered ACK tails; invalid limits leave the decoder unchanged.
  bool set_max_frame_len(uint32_t limit) {
    if (limit < 28 || limit > PL_MAX_FRAME_LEN) return false;
    max_frame_len_ = limit;
    return true;
  }
  void feed(const uint8_t *data, size_t len);
  // 完整帧 → true（out 填充）；半包 → false 不消费；非法帧 → false且err=-4
  bool next(Frame *out, int *err);

private:
  uint32_t max_frame_len_;
  std::vector<uint8_t> buf_;
  size_t pos_ = 0;      // 已消费游标
  void compact();       // 前缀过半才 memmove（批量帧 O(N) 总代价）
};

uint64_t new_cid(); // 随机（splitmix64 种子——仅客户端用途）

} // namespace parrot::lite
#endif // __cplusplus

#endif // PARROT_LITE_H
