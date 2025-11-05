// Parrot C++ Lite 实现（DEV_04 §5 / DEV_08 性能优化）——POSIX socket + Wire 1.0。
//
// 依赖：仅 libc。线程模型（DEV_08 并发化）：
//   - g_mu        ：conn/pending/decoder 状态锁（短临界区，绝不在持有期间等 IO）；
//   - g_pump_mu   ：socket 泵互斥（pl_ask 帮助泵 / pl_poll / 后台泵线程 三者互斥）；
//   - 锁序 g_pump_mu → g_mu（严禁反向）。
//   - 代际（g_gen）：pl_close/pl_connect 递增；泵循环校验代际防悬垂 fd/Conn。
//   - on_ask 回调永不持 g_mu 触发（帧收集 → 解锁 → 回调 → 回锁继续）。
//
// 性能设计（DEV_08）：
//   - FrameDecoder 游标化：next() 只推进 pos_，缓冲过半才 memmove 压缩——
//     旧实现每帧 erase(begin) 移动剩余全部字节，批量 N 帧总代价 O(N²)；
//   - pl_ask 帮助泵：发送后不持全局锁死等，而是"抢到泵权就自己收，抢不到
//     就条件变量小睡"——多线程并发 ask 在途帧真并发配对（旧实现全局锁内
//     poll 等回复 = 完全串行化）；
//   - 可选后台泵线程（pl_pump_start）：机器人主控形态免手动 pl_poll。

#include "parrot_lite.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <thread>
#include <unistd.h>

#include <atomic>
#include <algorithm>
#include <memory>
#include <stdexcept>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <cstring>
#include <map>
#include <mutex>
#include <vector>
#include <deque>

namespace pl {
namespace {

std::atomic<uint64_t> g_cid_seed{0x9E3779B97F4A7C15ull};
std::atomic<uint64_t> g_gen{1}; // 连接代际（close/connect 递增——泵循环防悬垂）

uint64_t mix64(uint64_t z) {
  z += 0x9E3779B97F4A7C15ull;
  z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
  z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
  return z ^ (z >> 31);
}

/// 在途 ask 等待项（锁外等待——DEV_08 并发化核心）
struct Pending {
  explicit Pending(std::chrono::steady_clock::time_point value) : deadline(value) {}
  const std::chrono::steady_clock::time_point deadline;
  std::mutex mu;
  std::condition_variable cv;
  std::vector<uint8_t> reply; // REPLY 载荷
  int err_code = 0;           // REPLY_ERR 映射负数码 / 连接断 -108
  bool done = false;
};

/// 延迟回调事件（解锁后触发——防死锁/防重入破坏状态）
struct Event {
  pl_on_ask_t cb;
  void *userdata;
  std::string path;
  std::string type_key;
  std::vector<uint8_t> payload;
  uint64_t cid;
  uint64_t generation;
};

struct Conn {
  int fd = -1;
  std::atomic<bool> closed{false};
  std::mutex write_mu;
  std::condition_variable write_changed;
  bool writing = false;
  std::deque<std::vector<uint8_t>> control;
  uint64_t gen = 0;
  std::string node_id;
  bool handshake_ok = false;
  parrot::lite::FrameDecoder decoder{PL_MAX_FRAME_LEN};
  uint32_t peer_max_frame_len = PL_MAX_FRAME_LEN;
  ~Conn() { if (fd >= 0) ::close(fd); }
  pl_on_ask_t on_ask = nullptr;
  void *userdata = nullptr;

  // 在途 ask 配对表（cid → Pending）
  std::map<uint64_t, std::shared_ptr<Pending>> pending;
};
std::shared_ptr<Conn> g_conn;
std::mutex g_mu;       // 状态锁
std::mutex g_pump_mu;  // 泵互斥（锁序：g_pump_mu → g_mu）

std::thread g_pump_thread;
std::atomic<bool> g_pump_run{false};
std::mutex g_thread_mu;
std::condition_variable g_thread_changed;
bool g_joining = false;
thread_local bool on_pump_thread = false;
// Constructed last, destroyed first: a live std::thread must be joined before
// its handle and the connection/locks are destroyed (also on library unload).
struct RuntimeCleanup {
  ~RuntimeCleanup() { pl_pump_stop(); pl_close(); }
};


void put_u32le(std::vector<uint8_t> *o, uint32_t v) {
  o->push_back(v & 0xFF);
  o->push_back((v >> 8) & 0xFF);
  o->push_back((v >> 16) & 0xFF);
  o->push_back((v >> 24) & 0xFF);
}

uint32_t read_u32(const uint8_t *p) {
  return uint32_t(p[0]) | (uint32_t(p[1]) << 8) |
         (uint32_t(p[2]) << 16) | (uint32_t(p[3]) << 24);
}

bool utf8(const uint8_t *p, size_t len, bool allow_nul = true) {
  size_t i = 0;
  while (i < len) {
    const auto lead = p[i++];
    if (lead < 0x80) { if (!allow_nul && lead == 0) return false; continue; }
    unsigned count; uint32_t value, minimum;
    if (lead >= 0xc2 && lead <= 0xdf) { count = 1; value = lead & 0x1f; minimum = 0x80; }
    else if (lead >= 0xe0 && lead <= 0xef) { count = 2; value = lead & 0x0f; minimum = 0x800; }
    else if (lead >= 0xf0 && lead <= 0xf4) { count = 3; value = lead & 0x07; minimum = 0x10000; }
    else return false;
    if (count > len - i) return false;
    while (count--) {
      const auto next = p[i++];
      if ((next & 0xc0) != 0x80) return false;
      value = (value << 6) | (next & 0x3f);
    }
    if (value < minimum || value > 0x10ffff || (value >= 0xd800 && value <= 0xdfff)) return false;
  }
  return true;
}
bool identity(const std::string &s) {
  return utf8(reinterpret_cast<const uint8_t *>(s.data()), s.size(), false);
}
bool known_type(uint8_t ft) {
  switch (ft) {
    case 1: case 2: case 3: case 4:
    case 0x10: case 0x11: case 0x12: case 0x13: case 0x14: case 0x15:
    case 0x20: case 0x21: case 0x22: case 0x23: case 0x24: case 0x7f: return true;
    default: return false;
  }
}
size_t payload_budget(const std::string &path, const std::string &key, size_t limit) {
  if (limit < 28 || limit > PL_MAX_FRAME_LEN || !identity(path) || !identity(key))
    throw std::invalid_argument("invalid frame identity or limit");
  size_t remaining = limit - 28;
  if (path.size() > remaining) throw std::length_error("path exceeds frame limit");
  remaining -= path.size();
  if (key.size() > remaining) throw std::length_error("key exceeds frame limit");
  return remaining - key.size();
}

bool handshake_ack(const parrot::lite::Frame &frame, uint32_t *peer_limit) {
  if (frame.ft != PL_FT_HANDSHAKE_ACK || frame.flags != 0 || !frame.path.empty()) return false;
  const auto &body = frame.payload;
  bool seen[10] = {}; uint32_t caps = 0, limit = 0; std::string chosen = "bin";
  size_t offset = 0;
  while (offset < body.size()) {
    if (body.size() - offset < 3) return false;
    const uint8_t tag = body[offset];
    const size_t length = size_t(body[offset + 1]) | (size_t(body[offset + 2]) << 8);
    offset += 3;
    if (length > body.size() - offset) return false;
    const auto *value = body.data() + offset;
    if (tag >= 1 && tag <= 9) {
      if (seen[tag]) return false;
      seen[tag] = true;
      switch (tag) {
        case 1: if (!length || !utf8(value, length, false)) return false; break;
        case 2: case 3: case 8: if (!utf8(value, length, false)) return false; break;
        case 4: if (length != 4) return false; caps = read_u32(value); break;
        case 5: if (length != 4) return false; limit = read_u32(value); break;
        case 6: if (length != 1 || value[0] > 3) return false; break;
        case 7: if (length != 1 || value[0] == 0) return false; break;
        case 9: chosen.assign(reinterpret_cast<const char *>(value), length); break;
      }
    }
    offset += length;
  }
  if (!seen[1] || !seen[4] || !seen[5] || !seen[6] || limit < 28 || !(caps & 2) || chosen != "pb") return false;
  *peer_limit = std::min(limit, uint32_t(PL_MAX_FRAME_LEN));
  return true;
}

std::vector<uint8_t> encode_frame(uint8_t ft, uint64_t cid, uint16_t flags,
                                  const std::string &path,
                                  const std::string &key,
                                  const uint8_t *payload, size_t len,
                                  uint8_t hop_count = 0, uint8_t hop_limit = PL_DEFAULT_HOP_LIMIT,
                                  uint32_t sequence = 0, size_t limit = PL_MAX_FRAME_LEN) {
  if ((!payload && len) || !known_type(ft) || !hop_limit || hop_count >= hop_limit)
    throw std::invalid_argument("invalid frame input");
  if (len > payload_budget(path, key, limit)) throw std::length_error("frame exceeds negotiated limit");
  const size_t body = 28 + path.size() + key.size() + len;
  std::vector<uint8_t> out(4 + body, 0);
  uint32_t bl = static_cast<uint32_t>(body);
  out[0] = bl & 0xFF; out[1] = (bl >> 8) & 0xFF;
  out[2] = (bl >> 16) & 0xFF; out[3] = (bl >> 24) & 0xFF;
  out[4] = PL_WIRE_VERSION;
  out[5] = ft;
  out[6] = flags & 0xFF; out[7] = (flags >> 8) & 0xFF;
  for (int i = 0; i < 8; i++) out[8 + i] = (cid >> (8 * i)) & 0xFF;
  out[16] = hop_count;
  out[17] = hop_limit;
  for (int i = 0; i < 4; ++i) out[18 + i] = (sequence >> (8 * i)) & 0xff;
  // reserved u48 @18..23 = 0（已零初始化）
  const uint32_t pl = static_cast<uint32_t>(path.size());
  out[24] = pl & 0xFF; out[25] = (pl >> 8) & 0xFF;
  out[26] = (pl >> 16) & 0xFF; out[27] = (pl >> 24) & 0xFF;
  memcpy(out.data() + 28, path.data(), pl);
  const size_t key_off = 28 + pl;
  const uint32_t kl = static_cast<uint32_t>(key.size());
  out[key_off] = kl & 0xFF; out[key_off + 1] = (kl >> 8) & 0xFF;
  out[key_off + 2] = (kl >> 16) & 0xFF; out[key_off + 3] = (kl >> 24) & 0xFF;
  memcpy(out.data() + key_off + 4, key.data(), kl);
  if (len) memcpy(out.data() + key_off + 4 + kl, payload, len);
  return out;
}

// ASK 载荷 reply_to 前缀（Rust split_reply_to / TS withReplyToPrefix 同构）
std::vector<uint8_t> with_reply_to(const std::string &reply_to,
                                   const uint8_t *payload, size_t len) {
  if ((!payload && len) || reply_to.size() > PL_MAX_FRAME_LEN - 4 ||
      len > PL_MAX_FRAME_LEN - 4 - reply_to.size()) throw std::length_error("ASK prefix exceeds limit");
  std::vector<uint8_t> out;
  out.reserve(4 + reply_to.size() + len);
  put_u32le(&out, static_cast<uint32_t>(reply_to.size()));
  out.insert(out.end(), reply_to.begin(), reply_to.end());
  if (len) out.insert(out.end(), payload, payload + len);
  return out;
}

void tlv_append(std::vector<uint8_t> *out, uint8_t tag, const uint8_t *v, size_t n) {
  if (n > 65535) throw std::length_error("TLV exceeds u16 length");
  out->push_back(tag);
  out->push_back(n & 0xFF);
  out->push_back((n >> 8) & 0xFF);
  out->insert(out->end(), v, v + n);
}

constexpr uint32_t LOCAL_MAX_FRAME_LEN = 1u << 20;

// 握手 TLV（TS handshakeBody 同构：NODE_ID/CAPS=PB/MFL=1MiB/ROLE=0/HOP=8）
std::vector<uint8_t> handshake_body(const std::string &node_id, bool ack) {
  std::vector<uint8_t> out;
  tlv_append(&out, 1, reinterpret_cast<const uint8_t *>(node_id.data()), node_id.size());
  const uint8_t caps[] = {0x02, 0x00, 0x00, 0x00};
  tlv_append(&out, 4, caps, sizeof(caps));
  uint8_t mfl[4];
  for (unsigned i = 0; i < 4; ++i) mfl[i] = (LOCAL_MAX_FRAME_LEN >> (8 * i)) & 0xff;
  tlv_append(&out, 5, mfl, sizeof(mfl));
  const uint8_t role[] = {0};
  tlv_append(&out, 6, role, sizeof(role));
  const uint8_t hop[] = {8};
  tlv_append(&out, 7, hop, sizeof(hop));
  if (ack) {
    static const char cc[] = "pb";
    tlv_append(&out, 9, reinterpret_cast<const uint8_t *>(cc), 2);
  }
  return out;
}

using Clock = std::chrono::steady_clock;
using Deadline = Clock::time_point;
int remaining_ms(Deadline deadline) {
  const auto now = Clock::now();
  if (now >= deadline) return 0;
  return static_cast<int>(std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now).count() + 1);
}
void close_locked(int error = -(100 + PL_ERR_CONNECTION_LOST));
void close_connection(const std::shared_ptr<Conn> &c) {
  std::lock_guard<std::mutex> lock(g_mu);
  if (g_conn == c) close_locked();
}
// Caller owns the connection's writer. Never blocks in send(2); poll uses the
// original operation deadline. A partial frame may never be followed by another.
int write_bytes(const std::shared_ptr<Conn> &c, const std::vector<uint8_t> &frame,
                Deadline deadline, bool try_once = false) {
  size_t offset = 0;
  int result = 0;
  while (offset < frame.size()) {
    if (c->closed.load()) { result = -2; break; }
    if (!try_once && Clock::now() >= deadline) { result = -3; break; }
    const auto n = ::send(c->fd, frame.data() + offset, frame.size() - offset, MSG_NOSIGNAL | MSG_DONTWAIT);
    if (n > 0) { offset += static_cast<size_t>(n); continue; }
    if (n < 0 && errno == EINTR) continue;
    if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
      const int remain = remaining_ms(deadline);
      if (try_once || !remain) { result = -3; break; }
      pollfd poller{c->fd, POLLOUT, 0};
      const int ready = ::poll(&poller, 1, remain);
      if (ready < 0 && errno == EINTR) continue;
      if (ready == 0) { result = -3; break; }
      if (ready < 0 || (poller.revents & (POLLERR | POLLHUP | POLLNVAL))) { result = -2; break; }
      continue;
    }
    result = -2; break;
  }
  if (result == -2 || (result && offset)) close_connection(c);
  return result;
}
struct Writer {
  std::shared_ptr<Conn> c;
  bool owned = false;
  explicit Writer(std::shared_ptr<Conn> connection) : c(std::move(connection)) {}
  int acquire(Deadline deadline, bool nonblocking) {
    std::unique_lock<std::mutex> lock(c->write_mu);
    if (nonblocking) {
      if (c->writing) return -3;
    } else if (!c->write_changed.wait_until(lock, deadline, [&] { return !c->writing || c->closed.load(); })) return -3;
    if (c->closed.load()) return -2;
    c->writing = owned = true;
    return 0;
  }
  ~Writer() {
    if (!owned) return;
    { std::lock_guard<std::mutex> lock(c->write_mu); c->writing = false; }
    c->write_changed.notify_all();
  }
};
int send_frame(const std::shared_ptr<Conn> &c, const std::vector<uint8_t> &frame, Deadline deadline) {
  Writer writer(c);
  const int admission = writer.acquire(deadline, false);
  return admission ? admission : write_bytes(c, frame, deadline);
}
// Heartbeats cannot wait behind a blocked application writer while holding the
// pump. Keep a bounded queue and retry on subsequent pump turns.
bool flush_control(const std::shared_ptr<Conn> &c) try {
  Writer writer(c);
  const int admission = writer.acquire(Clock::now(), true);
  if (admission) return admission == -3;
  for (;;) {
    std::vector<uint8_t> frame;
    {
      std::lock_guard<std::mutex> lock(g_mu);
      if (g_conn != c) return false;
      if (c->control.empty()) return true;
      frame = c->control.front();
    }
    const int result = write_bytes(c, frame, Clock::now(), true);
    if (result) return !c->closed.load();
    std::lock_guard<std::mutex> lock(g_mu);
    c->control.pop_front();
  }
} catch (...) { close_connection(c); return false; }

bool wait_readable(int fd, int timeout_ms) {
  struct pollfd pfd;
  pfd.fd = fd;
  pfd.events = POLLIN;
  pfd.revents = 0;
  const int r = ::poll(&pfd, 1, timeout_ms);
  return r > 0 && (pfd.revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL));
}

// 错误帧载荷 → C ABI 负数码（-101..-113 ↔ ErrCode 1..13）
int err_payload_to_code(const std::vector<uint8_t> &p) {
  if (p.size() < 4) return -4;
  const int code = p[0] | (p[1] << 8);
  return -(100 + code);
}

// A structural frame is not yet a valid executable envelope. On any violation
// the owning pump closes this generation and fails all of its pending callers.
bool dispatch_frames(Conn *c, std::vector<Event> *out) {
  parrot::lite::Frame f;
  int err = 0;
  while (c->decoder.next(&f, &err)) {
    if (!identity(f.path) || !identity(f.type_key)) return false;
    // ERROR is connection-terminal, even when its diagnostic payload is malformed.
    if (f.ft == PL_FT_ERROR) return false;
    if (f.ft == PL_FT_HEARTBEAT || f.ft == PL_FT_HEARTBEAT_ACK) {
      if (f.flags || !f.payload.empty()) return false;
      if (f.ft == PL_FT_HEARTBEAT_ACK) continue;
      const auto reply = encode_frame(PL_FT_HEARTBEAT_ACK, f.cid, 0, "", "", nullptr, 0);
      if (c->control.size() >= 64) return false;
      c->control.push_back(reply);
      continue;
    }
    if (f.ft == PL_FT_HANDSHAKE || f.ft == PL_FT_HANDSHAKE_ACK) return false;
    if (f.ft == PL_FT_REPLY || f.ft == PL_FT_REPLY_ERR || f.ft == PL_FT_ASK || f.ft == PL_FT_TELL) {
      if (f.flags & ~(PL_FLAG_TRACING | PL_FLAG_URGENT)) return false;
    }
    if (f.ft == PL_FT_REPLY || f.ft == PL_FT_REPLY_ERR) {
      if (f.ft == PL_FT_REPLY_ERR &&
          (f.payload.size() < 4 || f.payload[2] || f.payload[3] ||
           f.payload[1] != 0 || f.payload[0] < 1 || f.payload[0] > 13 ||
           !utf8(f.payload.data() + 4, f.payload.size() - 4))) return false;
      auto it = c->pending.find(f.cid);
      if (it == c->pending.end()) continue;
      auto p = it->second;
      c->pending.erase(it);
      {
        std::lock_guard<std::mutex> lk(p->mu);
        // Completion is decided at pairing, independently of delayed callback return.
        if (Clock::now() >= p->deadline) p->err_code = -3;
        else if (f.ft == PL_FT_REPLY) p->reply = std::move(f.payload);
        else p->err_code = err_payload_to_code(f.payload);
        p->done = true;
      }
      p->cv.notify_one();
      continue;
    }
    if (f.ft == PL_FT_ASK || f.ft == PL_FT_TELL) {
      const uint8_t *real = f.payload.data();
      size_t real_len = f.payload.size();
      if (f.ft == PL_FT_ASK) {
        if (real_len < 4) return false;
        const size_t reply_len = read_u32(real);
        if (reply_len > real_len - 4 || !utf8(real + 4, reply_len, false)) return false;
        real += 4 + reply_len;
        real_len -= 4 + reply_len;
      }
      if (f.type_key == "parrot:durable#v1") return false;
      if (!c->on_ask) continue;
      Event e;
      e.cb = c->on_ask;
      e.userdata = c->userdata;
      e.path = std::move(f.path);
      e.type_key = std::move(f.type_key);
      e.cid = f.cid;
      e.generation = c->gen;
      if (real_len) e.payload.assign(real, real + real_len);
      out->push_back(std::move(e));
    }
  }
  return err == 0;
}

/// 泵一次（持 g_pump_mu 调用）：收一批 → 配对/收集 → 解锁触发回调。
/// 返回 false = 连接已断/代际失效。events 由调用方在释放 g_mu 后触发。
bool pump_once(uint64_t gen, int timeout_ms, std::vector<Event> *events) {
  std::shared_ptr<Conn> connection;
  {
    std::lock_guard<std::mutex> lk(g_mu);
    if (!g_conn || g_conn->gen != gen || !g_conn->handshake_ok) return false;
    connection = g_conn;
    // A handshake read may already have buffered subsequent complete frames.
    try {
      if (!dispatch_frames(connection.get(), events)) {
        close_locked(-(100 + PL_ERR_PROTOCOL_VIOLATION)); events->clear(); return false;
      }
    } catch (...) {
      close_locked(-(100 + PL_ERR_PROTOCOL_VIOLATION)); events->clear(); return false;
    }
  }
  if (!flush_control(connection)) return false;
  if (!events->empty()) return true;
  const int fd = connection->fd;
  if (!wait_readable(fd, timeout_ms)) return true;
  uint8_t tmp[65536];
  const ssize_t received = ::recv(fd, tmp, sizeof(tmp), 0);
  {
    std::lock_guard<std::mutex> lk(g_mu);
    if (g_conn != connection) return false;
    if (received < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) return true;
    if (received <= 0) { close_locked(); return false; }
    try {
      connection->decoder.feed(tmp, static_cast<size_t>(received));
      if (!dispatch_frames(connection.get(), events)) {
        close_locked(-(100 + PL_ERR_PROTOCOL_VIOLATION)); events->clear(); return false;
      }
    } catch (...) {
      close_locked(-(100 + PL_ERR_PROTOCOL_VIOLATION)); events->clear(); return false;
    }
  }
  return flush_control(connection);
}

/// 触发收集的回调事件（无锁上下文——回调可安全重入 pl_ask/pl_tell）。
void fire_events(std::vector<Event> *events) {
  for (auto &e : *events) {
    {
      std::lock_guard<std::mutex> lock(g_mu);
      if (!g_conn || g_conn->gen != e.generation) break;
    }
    try {
      e.cb(e.path.c_str(), e.type_key.c_str(),
           e.payload.empty() ? nullptr : e.payload.data(), e.payload.size(), e.cid);
    } catch (...) {
      std::lock_guard<std::mutex> lock(g_mu);
      if (g_conn && g_conn->gen == e.generation) close_locked();
      break;
    }
  }
  events->clear();
}

// 内部 close（调用方持 g_mu 且已持/无需 g_pump_mu——见 pl_close）
void close_locked(int error) {
  // close also cancels connection admission before a socket exists.
  g_gen.fetch_add(1);
  if (!g_conn) return;
  {
    std::lock_guard<std::mutex> lock(g_conn->write_mu);
    g_conn->closed.store(true);
  }
  g_conn->write_changed.notify_all();
  // Wake any retained reader; its shared owner keeps the descriptor alive.
  if (g_conn->fd >= 0) ::shutdown(g_conn->fd, SHUT_RDWR);
  for (auto &kv : g_conn->pending) {
    auto p = kv.second;
    {
      std::lock_guard<std::mutex> lk(p->mu);
      p->err_code = error;
      p->done = true;
    }
    p->cv.notify_one();
  }
  g_conn.reset();
}

// Every return/exception retires registration; no map entry borrows an ASK stack.
struct PendingRegistration {
  uint64_t cid = 0, generation = 0;
  ~PendingRegistration() {
    std::lock_guard<std::mutex> lk(g_mu);
    if (g_conn && g_conn->gen == generation) g_conn->pending.erase(cid);
  }
};

struct ConnectAdmission {
  std::shared_ptr<Conn> connection;
  bool committed = false;
  ~ConnectAdmission() { if (!committed) close_connection(connection); }
};

// POSIX getaddrinfo has no portable cancellation primitive. At most one owned
// resolver worker exists; timed-out callers abandon the result, not the thread.
// Later calls wait within their own deadline until it can be joined and reused.
struct ResolveJob {
  std::mutex mu;
  std::condition_variable changed;
  bool done = false;
  int error = EAI_FAIL;
  addrinfo *addresses = nullptr;
  ~ResolveJob() { if (addresses) ::freeaddrinfo(addresses); }
};
struct Resolver {
  std::mutex mu;
  std::shared_ptr<ResolveJob> current;
  std::thread worker;
  ~Resolver() { if (worker.joinable()) worker.join(); }
};
Resolver resolver;
RuntimeCleanup runtime_cleanup;
bool current_generation(uint64_t gen) { return g_gen.load() == gen; }
bool parse_address(const char *url, std::string *host, unsigned *port) {
  size_t prefix;
  if (std::strncmp(url, "parrot://", 9) == 0) prefix = 9;
  else if (std::strncmp(url, "tcp://", 6) == 0) prefix = 6;
  else return false;
  const std::string authority(url + prefix);
  if (authority.empty() || authority.find_first_of("/?#@\\%") != std::string::npos) return false;
  for (const unsigned char byte : authority)
    if (byte <= 0x20 || byte == 0x7f) return false;
  std::string parsed_host, port_text;
  if (authority[0] == '[') {
    const auto close = authority.find(']');
    if (close == std::string::npos || close + 1 >= authority.size() || authority[close + 1] != ':') return false;
    parsed_host = authority.substr(1, close - 1);
    in6_addr numeric{};
    if (::inet_pton(AF_INET6, parsed_host.c_str(), &numeric) != 1) return false;
    port_text = authority.substr(close + 2);
  } else {
    const auto colon = authority.find(':');
    if (colon == std::string::npos || colon != authority.rfind(':') ||
        authority.find_first_of("[]") != std::string::npos) return false;
    parsed_host = authority.substr(0, colon);
    port_text = authority.substr(colon + 1);
  }
  if (parsed_host.empty() || !identity(parsed_host) || port_text.empty()) return false;
  unsigned parsed_port = 0;
  for (const char digit : port_text) {
    if (digit < '0' || digit > '9' || parsed_port > 6553) return false;
    parsed_port = parsed_port * 10 + unsigned(digit - '0');
    if (parsed_port > 65535) return false;
  }
  if (!parsed_port) return false;
  *host = std::move(parsed_host); *port = parsed_port;
  return true;
}
std::shared_ptr<ResolveJob> resolve(const std::string &host, const std::string &port,
                                  uint64_t gen, Deadline deadline) {
  std::shared_ptr<ResolveJob> job;
  // Numeric peers remain connectable even while an abandoned DNS query is
  // still inside libc. These flags prohibit resolver/network service lookup.
  in6_addr numeric{};
  if (::inet_pton(AF_INET, host.c_str(), &numeric) == 1 ||
      ::inet_pton(AF_INET6, host.c_str(), &numeric) == 1) {
    job = std::make_shared<ResolveJob>();
    addrinfo hints{}; hints.ai_family = AF_UNSPEC; hints.ai_socktype = SOCK_STREAM;
    hints.ai_flags = AI_NUMERICHOST | AI_NUMERICSERV;
    job->error = ::getaddrinfo(host.c_str(), port.c_str(), &hints, &job->addresses);
    job->done = true;
    return job;
  }
  while (Clock::now() < deadline && current_generation(gen)) {
    {
      std::lock_guard<std::mutex> lock(resolver.mu);
      bool reusable = !resolver.current;
      if (resolver.current) {
        std::lock_guard<std::mutex> result(resolver.current->mu);
        reusable = resolver.current->done;
      }
      if (reusable) {
        if (resolver.worker.joinable()) resolver.worker.join();
        job = std::make_shared<ResolveJob>();
        resolver.worker = std::thread([job, host, port] {
          addrinfo hints{}; hints.ai_family = AF_UNSPEC; hints.ai_socktype = SOCK_STREAM;
          addrinfo *addresses = nullptr;
          const int error = ::getaddrinfo(host.c_str(), port.c_str(), &hints, &addresses);
          { std::lock_guard<std::mutex> result(job->mu);
            job->addresses = addresses; job->error = error; job->done = true; }
          job->changed.notify_all();
        });
        resolver.current = job;
        break;
      }
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(5));
  }
  if (!job) return nullptr;
  std::unique_lock<std::mutex> result(job->mu);
  while (!job->done && Clock::now() < deadline && current_generation(gen))
    job->changed.wait_until(result, std::min(deadline, Clock::now() + std::chrono::milliseconds(5)));
  return job->done && current_generation(gen) && Clock::now() < deadline ? job : nullptr;
}

/// 后台泵线程体：机器人主控形态（pl_pump_start 启用）。
void pump_thread_main() {
  on_pump_thread = true;
  std::vector<Event> events;
  while (g_pump_run.load(std::memory_order_acquire)) {
    uint64_t gen = g_gen.load(std::memory_order_acquire);
    bool connected;
    { std::lock_guard<std::mutex> lk(g_mu); connected = bool(g_conn); }
    if (!connected) {
      std::this_thread::sleep_for(std::chrono::milliseconds(20));
      continue;
    }
    std::unique_lock<std::mutex> pump(g_pump_mu, std::try_to_lock);
    if (!pump.owns_lock()) {
      std::this_thread::sleep_for(std::chrono::milliseconds(1));
      continue;
    }
    if (!pump_once(gen, 20, &events)) {
      std::this_thread::sleep_for(std::chrono::milliseconds(50)); // 断连退避
      continue;
    }
    pump.unlock();
    fire_events(&events);
  }
}

} // namespace
} // namespace pl

// ── C ABI 实现 ─────────────────────────────────────────
extern "C" {

const char *pl_backend_name(void) {
#if defined(PL_FORCE_POLL)
  return "poll";
#elif defined(PL_FORCE_EPOLL)
  return "epoll";
#elif defined(PL_FORCE_KQUEUE)
  return "kqueue";
#elif defined(__linux__)
  return "epoll-or-poll";
#elif defined(__APPLE__) || defined(__FreeBSD__) || defined(__OpenBSD__) || \
    defined(__NetBSD__) || defined(__DragonFly__)
  return "kqueue-or-poll";
#else
  return "poll";
#endif
}

int pl_connect(const char *url, const char *node_id, const char *tls_dir) try {
  // The dependency-free client does not implement TLS. Never downgrade a
  // caller's explicit TLS request into a plaintext connection.
  if (!url || !node_id || tls_dir || !*node_id || !pl::identity(node_id) || std::strlen(node_id) > 65535) return -1;
  const auto deadline = pl::Clock::now() + std::chrono::seconds(5);
  std::string host;
  unsigned port = 0;
  if (!pl::parse_address(url, &host, &port)) return -1;

  uint64_t gen;
  {
    std::lock_guard<std::mutex> lock(pl::g_mu);
    pl::close_locked();
    gen = pl::g_gen.load();
  }
  auto result = pl::resolve(host, std::to_string(port), gen, deadline);
  if (!result) return pl::current_generation(gen) ? -3 : -2;
  if (result->error || !result->addresses) return -2;
  for (auto *address = result->addresses; address; address = address->ai_next) {
    if (!pl::current_generation(gen)) return -2;
    if (pl::Clock::now() >= deadline) return -3;
    auto c = std::make_shared<pl::Conn>();
    c->fd = ::socket(address->ai_family, address->ai_socktype, address->ai_protocol);
    if (c->fd < 0) continue;
    if (::fcntl(c->fd, F_SETFL, O_NONBLOCK) < 0) continue;
    c->gen = gen; c->node_id = node_id;
    {
      std::lock_guard<std::mutex> lock(pl::g_mu);
      if (!pl::current_generation(gen)) return -2;
      pl::g_conn = c;
    }
    pl::ConnectAdmission admission{c};
    int connected = ::connect(c->fd, address->ai_addr, address->ai_addrlen);
    if (connected < 0 && errno == EINPROGRESS) {
      for (;;) {
        if (c->closed.load()) return -2;
        const int remain = pl::remaining_ms(deadline);
        if (!remain) return -3;
        pollfd poller{c->fd, POLLOUT, 0};
        const int ready = ::poll(&poller, 1, std::min(remain, 20));
        if (ready < 0 && errno == EINTR) continue;
        if (!ready) continue;
        int error = 0; socklen_t size = sizeof(error);
        connected = ready > 0 && ::getsockopt(c->fd, SOL_SOCKET, SO_ERROR, &error, &size) == 0 && !error ? 0 : -1;
        break;
      }
    }
    if (connected < 0) {
      // Retire this address without invalidating admission for subsequent ones.
      std::lock_guard<std::mutex> lock(pl::g_mu);
      if (pl::g_conn != c || c->closed.load()) return -2;
      c->closed.store(true); pl::g_conn.reset(); admission.committed = true;
      continue;
    }
    int one = 1;
    ::setsockopt(c->fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
    const auto body = pl::handshake_body(node_id, false);
    const auto hello = pl::encode_frame(PL_FT_HANDSHAKE, pl::mix64(++pl::g_cid_seed), 0,
                                        "", "", body.data(), body.size());
    const int sent = pl::send_frame(c, hello, deadline);
    if (sent) return sent;
    while (pl::Clock::now() < deadline) {
      if (c->closed.load()) return -2;
      if (!pl::wait_readable(c->fd, std::min(pl::remaining_ms(deadline), 20))) continue;
      uint8_t bytes[65536];
      const auto n = ::recv(c->fd, bytes, sizeof(bytes), 0);
      if (n < 0 && (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK)) continue;
      if (n <= 0) return -2;
      c->decoder.feed(bytes, static_cast<size_t>(n));
      parrot::lite::Frame frame; int error = 0;
      if (c->decoder.next(&frame, &error)) {
        if (!pl::handshake_ack(frame, &c->peer_max_frame_len)) return -4;
        c->peer_max_frame_len = std::min(c->peer_max_frame_len, pl::LOCAL_MAX_FRAME_LEN);
        if (!c->decoder.set_max_frame_len(c->peer_max_frame_len)) return -4;
        std::lock_guard<std::mutex> lock(pl::g_mu);
        if (pl::g_conn != c || c->closed.load() || !pl::current_generation(gen)) return -2;
        c->handshake_ok = true; admission.committed = true;
        return 0;
      }
      if (error) return -4;
    }
    return -3;
  }
  return -2;
} catch (const std::bad_alloc &) { return -5; }
catch (const std::exception &) { return -1; }

int pl_register(const char *key, const char *path, const char *caps_json) try {
  (void)caps_json;
  const auto deadline = pl::Clock::now() + std::chrono::seconds(3);
  std::shared_ptr<pl::Conn> c;
  { std::lock_guard<std::mutex> lock(pl::g_mu);
    if (!pl::g_conn || !pl::g_conn->handshake_ok) return -2;
    c = pl::g_conn; }
  if (!key || !path) return -1;
  const auto frame = pl::encode_frame(PL_FT_TELL, pl::mix64(++pl::g_cid_seed), 0, path,
      std::string("$receptionist.register:") + key, nullptr, 0,
      0, PL_DEFAULT_HOP_LIMIT, 0, c->peer_max_frame_len);
  return pl::send_frame(c, frame, deadline);
} catch (const std::bad_alloc &) { return -5; }
catch (const std::exception &) { return -1; }

int pl_ask(const char *target_path, const char *type_key,
           const uint8_t *payload, size_t len,
           uint8_t **out, size_t *out_len, int timeout_ms) try {
  if (out) *out = nullptr;
  if (out_len) *out_len = 0;
  if (!target_path || !type_key || !out || !out_len) return -1;
  if ((!payload && len) || len > PL_MAX_FRAME_LEN) return -1;
  const auto deadline = pl::Clock::now() + std::chrono::milliseconds(timeout_ms <= 0 ? 3000 : timeout_ms);
  auto p = std::make_shared<pl::Pending>(deadline);
  pl::PendingRegistration registration;
  std::shared_ptr<pl::Conn> c;
  { std::lock_guard<std::mutex> lock(pl::g_mu);
    if (!pl::g_conn || !pl::g_conn->handshake_ok) return -2;
    c = pl::g_conn; }
  const auto cid = pl::mix64(++pl::g_cid_seed);
  const auto reply_to = c->node_id + "/lite";
  const size_t budget = pl::payload_budget(target_path, type_key, c->peer_max_frame_len);
  if (reply_to.size() > budget || budget - reply_to.size() < 4 || len > budget - reply_to.size() - 4) return -1;
  const auto body = pl::with_reply_to(reply_to, payload, len);
  const auto frame = pl::encode_frame(PL_FT_ASK, cid, 0, target_path, type_key,
      body.data(), body.size(), 0, PL_DEFAULT_HOP_LIMIT, 0, c->peer_max_frame_len);
  {
    std::lock_guard<std::mutex> lock(pl::g_mu);
    if (pl::g_conn != c) return -2;
    // Register before publishing bytes: another pump may receive an immediate reply.
    c->pending[cid] = p;
    registration.cid = cid; registration.generation = c->gen;
  }
  const int sent = pl::send_frame(c, frame, deadline);
  if (sent) return sent;

  std::vector<pl::Event> events;
  for (;;) {
    {
      std::lock_guard<std::mutex> lk(p->mu);
      if (p->done) break;
    }
    auto now = std::chrono::steady_clock::now();
    if (now >= deadline) {
      return -3; // PendingRegistration retires only the original generation.
    }
    const int remain = static_cast<int>(
        std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now).count());
    std::unique_lock<std::mutex> pump(pl::g_pump_mu, std::try_to_lock);
    if (pump.owns_lock()) {
      uint64_t my_gen;
      {
        std::lock_guard<std::mutex> g2(pl::g_mu);
        if (!pl::g_conn || pl::g_conn->gen != registration.generation) {
          std::lock_guard<std::mutex> result(p->mu);
          if (p->done) break;
          return -2;
        }
        my_gen = registration.generation;
      }
      if (!pl::pump_once(my_gen, std::min(remain, 10), &events)) {
        // A later frame failure cannot revoke an earlier completed request.
        std::lock_guard<std::mutex> lk(p->mu);
        if (p->done) break; // Deliver any completed reply through the common result path.
        return -2;
      }
      pump.unlock();
      pl::fire_events(&events);
      continue;
    }
    // 他人持泵：cv 小睡等配对通知
    std::unique_lock<std::mutex> lk(p->mu);
    p->cv.wait_for(lk, std::chrono::milliseconds(std::min(remain, 5)),
                  [&] { return p->done; });
  }

  if (p->err_code != 0) return p->err_code;
  *out = static_cast<uint8_t *>(malloc(p->reply.size() ? p->reply.size() : 1));
  if (!*out) return -5;
  if (!p->reply.empty()) memcpy(*out, p->reply.data(), p->reply.size());
  *out_len = p->reply.size();
  return 0;
} catch (const std::bad_alloc &) { return -5; }
catch (const std::exception &) { return -1; }

int pl_tell(const char *target_path, const char *type_key,
            const uint8_t *payload, size_t len) try {
  const auto deadline = pl::Clock::now() + std::chrono::seconds(3);
  std::shared_ptr<pl::Conn> c;
  { std::lock_guard<std::mutex> lock(pl::g_mu);
    if (!pl::g_conn || !pl::g_conn->handshake_ok) return -2;
    c = pl::g_conn; }
  if (!target_path || !type_key || (!payload && len) || len > PL_MAX_FRAME_LEN) return -1;
  const auto frame = pl::encode_frame(PL_FT_TELL, pl::mix64(++pl::g_cid_seed), 0,
      target_path, type_key, payload, len, 0, PL_DEFAULT_HOP_LIMIT, 0, c->peer_max_frame_len);
  return pl::send_frame(c, frame, deadline);
} catch (const std::bad_alloc &) { return -5; }
catch (const std::exception &) { return -1; }

void pl_poll(int timeout_ms) {
  std::vector<pl::Event> events;
  std::unique_lock<std::mutex> pump(pl::g_pump_mu, std::try_to_lock);
  if (!pump.owns_lock()) {
    // 后台泵/他人持泵：只等一小会让事件被消费（回调由持泵方触发）
    std::this_thread::sleep_for(std::chrono::milliseconds(
        timeout_ms > 0 ? std::min(timeout_ms, 5) : 0));
    return;
  }
  uint64_t gen;
  {
    std::lock_guard<std::mutex> lk(pl::g_mu);
    if (!pl::g_conn) return;
    gen = pl::g_conn->gen;
  }
  if (!pl::pump_once(gen, timeout_ms, &events)) return;
  pump.unlock();
  pl::fire_events(&events);
}

int pl_pump_start(void) {
  std::unique_lock<std::mutex> lock(pl::g_thread_mu);
  // A callback may stop its own pump, but cannot replace a still-running
  // thread object. An external caller joins it before starting another owner.
  if (pl::on_pump_thread) return pl::g_pump_run.load() ? 0 : -2;
  pl::g_thread_changed.wait(lock, [] { return !pl::g_joining; });
  if (pl::g_pump_run.load()) return 0;
  if (pl::g_pump_thread.joinable()) {
    pl::g_joining = true;
    auto previous = std::move(pl::g_pump_thread);
    lock.unlock(); previous.join(); lock.lock();
    pl::g_joining = false;
    pl::g_thread_changed.notify_all();
  }
  try {
    pl::g_pump_run.store(true);
    pl::g_pump_thread = std::thread(pl::pump_thread_main);
  } catch (...) {
    pl::g_pump_run.store(false);
    return -5;
  }
  return 0;
}

void pl_pump_stop(void) {
  std::unique_lock<std::mutex> lock(pl::g_thread_mu);
  pl::g_pump_run.store(false);
  // Leave the joinable handle owned. The next external stop/start joins it;
  // detaching would allow a second pump to race a still-running callback.
  if (pl::on_pump_thread) return;
  pl::g_thread_changed.wait(lock, [] { return !pl::g_joining; });
  pl::g_pump_run.store(false);
  if (!pl::g_pump_thread.joinable()) return;
  pl::g_joining = true;
  auto previous = std::move(pl::g_pump_thread);
  lock.unlock(); previous.join(); lock.lock();
  pl::g_joining = false;
  pl::g_thread_changed.notify_all();
}

void pl_set_on_ask(pl_on_ask_t cb, void *userdata) {
  std::lock_guard<std::mutex> lk(pl::g_mu);
  if (pl::g_conn) {
    pl::g_conn->on_ask = cb;
    pl::g_conn->userdata = userdata;
  }
}

void pl_free(void *p) { ::free(p); }

void pl_close(void) {
  std::lock_guard<std::mutex> lk(pl::g_mu);
  pl::close_locked();
}

} // extern "C"

// ── C++ 层实现 ─────────────────────────────────────────
namespace parrot::lite {

std::vector<uint8_t> Frame::encode() const {
  // 注意：这里编的是"净帧"（无 reply_to 前缀）——C++ 客户端侧构造数据帧用
  return pl::encode_frame(ft, cid, flags, path, type_key, payload.data(), payload.size(), hop_count, hop_limit, sequence);
}

void FrameDecoder::feed(const uint8_t *data, size_t len) {
  // 游标后追加（pos_ 前的已消费前缀仅在读过头时才压缩——见 next()）
  if (!len) return;
  if (!data) throw std::invalid_argument("null frame input");
  buf_.insert(buf_.end(), data, data + len);
}

bool FrameDecoder::next(Frame *out, int *err) {
  *err = 0;
  const size_t avail = buf_.size() - pos_;
  if (avail < 4) { compact(); return false; }
  const uint8_t *h = buf_.data() + pos_;
  uint32_t body_len = 0;
  for (int i = 0; i < 4; i++)
    body_len |= static_cast<uint32_t>(h[i]) << (8 * i);
  if (body_len < 28 || body_len > max_frame_len_) { *err = -4; buf_.clear(); pos_ = 0; return false; }
  if (avail < 4 + body_len) { compact(); return false; } // 半包不消费
  const uint8_t *b = h + 4;
  if (b[0] != PL_WIRE_VERSION) { *err = -4; buf_.clear(); pos_ = 0; return false; }
  const auto reject = [&]() { *err = -4; buf_.clear(); pos_ = 0; return false; };
  switch (b[1]) {
    case 1: case 2: case 3: case 4:
    case 0x10: case 0x11: case 0x12: case 0x13: case 0x14: case 0x15:
    case 0x20: case 0x21: case 0x22: case 0x23: case 0x24: case 0x7f: break;
    default: return reject();
  }
  if (b[13] == 0 || b[12] >= b[13] || b[18] != 0 || b[19] != 0) return reject();
  out->ft = b[1];
  out->flags = static_cast<uint16_t>(b[2] | (b[3] << 8));
  out->cid = 0;
  for (int i = 0; i < 8; i++) out->cid |= static_cast<uint64_t>(b[4 + i]) << (8 * i);
  out->hop_count = b[12];
  out->hop_limit = b[13];
  out->sequence = pl::read_u32(b + 14);
  // Low 32 bits carry sequence; high 16 reserved bits were checked above.
  uint32_t path_len = 0;
  for (int i = 0; i < 4; i++)
    path_len |= static_cast<uint32_t>(b[20 + i]) << (8 * i);
  // Reserve the key length field before any variable-field access.
  if (path_len > body_len - 28 || !pl::utf8(b + 24, path_len)) return reject();
  out->path.assign(reinterpret_cast<const char *>(b + 24), path_len);
  const size_t key_off = 24 + path_len;
  uint32_t key_len = 0;
  for (int i = 0; i < 4; i++)
    key_len |= static_cast<uint32_t>(b[key_off + i]) << (8 * i);
  if (key_len > body_len - key_off - 4 || !pl::utf8(b + key_off + 4, key_len)) return reject();
  out->type_key.assign(reinterpret_cast<const char *>(b + key_off + 4), key_len);
  const size_t payload_off = key_off + 4 + key_len;
  out->payload.assign(b + payload_off, b + body_len);
  pos_ += 4 + body_len;
  compact();
  return true;
}

/// 已消费前缀超过半区（或耗尽）才 memmove——批量 N 帧总代价 O(N)。
void FrameDecoder::compact() {
  if (pos_ == 0) return;
  if (pos_ >= buf_.size()) {
    buf_.clear();
    pos_ = 0;
  } else if (pos_ * 2 >= buf_.size()) {
    buf_.erase(buf_.begin(), buf_.begin() + static_cast<long>(pos_));
    pos_ = 0;
  }
}

uint64_t new_cid() { return pl::mix64(++pl::g_cid_seed); }

} // namespace parrot::lite
