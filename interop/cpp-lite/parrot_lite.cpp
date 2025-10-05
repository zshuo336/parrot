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
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <map>
#include <mutex>
#include <vector>

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
};

struct Conn {
  int fd = -1;
  uint64_t gen = 0;
  std::string node_id;
  bool handshake_ok = false;
  parrot::lite::FrameDecoder decoder;
  pl_on_ask_t on_ask = nullptr;
  void *userdata = nullptr;
  // 重连后自动重注册（TS lite 同语义）
  std::vector<std::pair<std::string, std::string>> registrations;
  // 在途 ask 配对表（cid → Pending）
  std::map<uint64_t, Pending *> pending;
};
Conn *g_conn = nullptr;
std::mutex g_mu;       // 状态锁
std::mutex g_pump_mu;  // 泵互斥（锁序：g_pump_mu → g_mu）

std::thread g_pump_thread;
std::atomic<bool> g_pump_run{false};

void put_u32le(std::vector<uint8_t> *o, uint32_t v) {
  o->push_back(v & 0xFF);
  o->push_back((v >> 8) & 0xFF);
  o->push_back((v >> 16) & 0xFF);
  o->push_back((v >> 24) & 0xFF);
}

std::vector<uint8_t> encode_frame(uint8_t ft, uint64_t cid, uint16_t flags,
                                  const std::string &path,
                                  const std::string &key,
                                  const uint8_t *payload, size_t len) {
  const size_t body = 28 + path.size() + key.size() + len;
  std::vector<uint8_t> out(4 + body, 0);
  uint32_t bl = static_cast<uint32_t>(body);
  out[0] = bl & 0xFF; out[1] = (bl >> 8) & 0xFF;
  out[2] = (bl >> 16) & 0xFF; out[3] = (bl >> 24) & 0xFF;
  out[4] = PL_WIRE_VERSION;
  out[5] = ft;
  out[6] = flags & 0xFF; out[7] = (flags >> 8) & 0xFF;
  for (int i = 0; i < 8; i++) out[8 + i] = (cid >> (8 * i)) & 0xFF;
  out[16] = 0; // hop_count
  out[17] = PL_DEFAULT_HOP_LIMIT;
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
  std::vector<uint8_t> out;
  put_u32le(&out, static_cast<uint32_t>(reply_to.size()));
  out.insert(out.end(), reply_to.begin(), reply_to.end());
  if (len) out.insert(out.end(), payload, payload + len);
  return out;
}

void tlv_append(std::vector<uint8_t> *out, uint8_t tag, const uint8_t *v, size_t n) {
  out->push_back(tag);
  out->push_back(n & 0xFF);
  out->push_back((n >> 8) & 0xFF);
  out->insert(out->end(), v, v + n);
}

// 握手 TLV（TS handshakeBody 同构：NODE_ID/CAPS=PB/MFL=1MiB/ROLE=0/HOP=8）
std::vector<uint8_t> handshake_body(const std::string &node_id, bool ack) {
  std::vector<uint8_t> out;
  tlv_append(&out, 1, reinterpret_cast<const uint8_t *>(node_id.data()), node_id.size());
  const uint8_t caps[] = {0x02, 0x00, 0x00, 0x00};
  tlv_append(&out, 4, caps, sizeof(caps));
  const uint8_t mfl[] = {0x00, 0x00, 0x10, 0x00}; // 1 MiB LE
  tlv_append(&out, 5, mfl, sizeof(mfl));
  const uint8_t role[] = {0};
  tlv_append(&out, 6, role, sizeof(role));
  const uint8_t hop[] = {8};
  tlv_append(&out, 7, hop, sizeof(hop));
  if (ack) {
    static const char cc[] = "pb";
    tlv_append(&out, 8, reinterpret_cast<const uint8_t *>(cc), 2);
  }
  return out;
}

bool send_all(int fd, const uint8_t *p, size_t n) {
  size_t off = 0;
  while (off < n) {
    const ssize_t w = ::send(fd, p + off, n - off, MSG_NOSIGNAL);
    if (w <= 0) {
      if (errno == EINTR) continue;
      return false;
    }
    off += static_cast<size_t>(w);
  }
  return true;
}

bool wait_readable(int fd, int timeout_ms) {
  struct pollfd pfd;
  pfd.fd = fd;
  pfd.events = POLLIN;
  pfd.revents = 0;
  const int r = ::poll(&pfd, 1, timeout_ms);
  return r > 0 && (pfd.revents & POLLIN);
}

// 错误帧载荷 → C ABI 负数码（-101..-113 ↔ ErrCode 1..13）
int err_payload_to_code(const std::vector<uint8_t> &p) {
  if (p.size() < 4) return -4;
  const int code = p[0] | (p[1] << 8);
  return -(100 + code);
}

/// 解码并配对/收集（调用方持 g_mu；回调事件进 *out 待解锁后触发）。
void dispatch_frames(Conn *c, std::vector<Event> *out) {
  parrot::lite::Frame f;
  int err = 0;
  while (c->decoder.next(&f, &err)) {
    if (f.ft == PL_FT_REPLY || f.ft == PL_FT_REPLY_ERR) {
      auto it = c->pending.find(f.cid);
      if (it == c->pending.end()) continue; // 迟到/超时后的旁路帧——丢弃
      Pending *p = it->second;
      c->pending.erase(it);
      {
        std::lock_guard<std::mutex> lk(p->mu);
        if (f.ft == PL_FT_REPLY) {
          p->reply = std::move(f.payload);
        } else {
          p->err_code = err_payload_to_code(f.payload);
        }
        p->done = true;
      }
      p->cv.notify_one();
      continue;
    }
    if ((f.ft == PL_FT_ASK || f.ft == PL_FT_TELL) && c->on_ask) {
      Event e;
      e.cb = c->on_ask;
      e.userdata = c->userdata;
      e.path = std::move(f.path);
      e.type_key = std::move(f.type_key);
      e.cid = f.cid;
      // ASK 载荷剥 reply_to 前缀（与 Rust ingress 同口径）
      const uint8_t *real = f.payload.data();
      size_t real_len = f.payload.size();
      if (f.ft == PL_FT_ASK && f.payload.size() >= 4) {
        const uint32_t rl = static_cast<uint32_t>(f.payload[0]) |
                            (static_cast<uint32_t>(f.payload[1]) << 8) |
                            (static_cast<uint32_t>(f.payload[2]) << 16) |
                            (static_cast<uint32_t>(f.payload[3]) << 24);
        if (4 + rl <= f.payload.size()) {
          real = f.payload.data() + 4 + rl;
          real_len = f.payload.size() - 4 - rl;
        }
      }
      e.payload.assign(real, real + real_len);
      out->push_back(std::move(e));
    }
  }
  if (err) {
    // 坏帧：断连处理（保守——协议违规不给部分语义）
    c->handshake_ok = false;
  }
}

/// 泵一次（持 g_pump_mu 调用）：收一批 → 配对/收集 → 解锁触发回调。
/// 返回 false = 连接已断/代际失效。events 由调用方在释放 g_mu 后触发。
bool pump_once(uint64_t gen, int timeout_ms, std::vector<Event> *events) {
  int fd = -1;
  {
    std::lock_guard<std::mutex> lk(g_mu);
    if (!g_conn || g_conn->gen != gen || g_conn->fd < 0) return false;
    fd = g_conn->fd;
  }
  if (!wait_readable(fd, timeout_ms)) return true; // 空转（连接仍在）
  uint8_t tmp[65536];
  const ssize_t r = ::recv(fd, tmp, sizeof(tmp), 0);
  if (r <= 0) return false;
  {
    std::lock_guard<std::mutex> lk(g_mu);
    if (!g_conn || g_conn->gen != gen) return false; // 竞态重连——弃本批
    g_conn->decoder.feed(tmp, static_cast<size_t>(r));
    dispatch_frames(g_conn, events);
  }
  return true;
}

/// 触发收集的回调事件（无锁上下文——回调可安全重入 pl_ask/pl_tell）。
void fire_events(std::vector<Event> *events) {
  for (auto &e : *events) {
    e.cb(e.path.c_str(), e.type_key.c_str(),
         e.payload.empty() ? nullptr : e.payload.data(), e.payload.size(), e.cid);
  }
  events->clear();
}

// 内部 close（调用方持 g_mu 且已持/无需 g_pump_mu——见 pl_close）
void close_locked() {
  if (!g_conn) return;
  if (g_conn->fd >= 0) ::close(g_conn->fd);
  for (auto &kv : g_conn->pending) {
    Pending *p = kv.second;
    {
      std::lock_guard<std::mutex> lk(p->mu);
      p->err_code = -(100 + PL_ERR_CONNECTION_LOST);
      p->done = true;
    }
    p->cv.notify_one();
  }
  delete g_conn;
  g_conn = nullptr;
  g_gen.fetch_add(1);
}

/// 后台泵线程体：机器人主控形态（pl_pump_start 启用）。
void pump_thread_main() {
  std::vector<Event> events;
  while (g_pump_run.load(std::memory_order_acquire)) {
    uint64_t gen = g_gen.load(std::memory_order_acquire);
    {
      std::lock_guard<std::mutex> lk(g_mu);
      if (!g_conn) {
        std::this_thread::sleep_for(std::chrono::milliseconds(20));
        continue;
      }
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
    fire_events(&events); // 无 g_mu/g_pump_mu 持有期间的子集？——pump 仍持有，
                          // 但回调重入 pl_ask 走 try_lock 泵路径不死锁。
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

int pl_connect(const char *url, const char *node_id, const char *tls_dir) {
  (void)tls_dir; // 明文形态（网关侧终接 mTLS）
  if (!url || !node_id) return -1;
  std::lock_guard<std::mutex> pump(pl::g_pump_mu);
  std::lock_guard<std::mutex> lk(pl::g_mu);
  pl::close_locked();
  if (strncmp(url, "parrot://", 9) != 0 && strncmp(url, "tcp://", 6) != 0) return -1;
  const char *hostport = url + (url[0] == 'p' ? 9 : 6);
  const char *colon = strrchr(hostport, ':');
  if (!colon) return -1;
  const std::string host(hostport, colon - hostport);
  const int port = atoi(colon + 1);
  if (port <= 0 || port > 65535) return -1;

  struct addrinfo hints{}, *res = nullptr;
  hints.ai_family = AF_UNSPEC;
  hints.ai_socktype = SOCK_STREAM;
  const std::string ports = std::to_string(port);
  if (::getaddrinfo(host.c_str(), ports.c_str(), &hints, &res) != 0 || !res) return -2;
  int fd = -1;
  for (auto *p = res; p; p = p->ai_next) {
    fd = ::socket(p->ai_family, p->ai_socktype, p->ai_protocol);
    if (fd < 0) continue;
    if (::connect(fd, p->ai_addr, p->ai_addrlen) == 0) break;
    ::close(fd);
    fd = -1;
  }
  ::freeaddrinfo(res);
  if (fd < 0) return -2;

  int one = 1;
  ::setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

  auto *c = new pl::Conn{};
  c->fd = fd;
  c->gen = pl::g_gen.load();
  c->node_id = node_id;
  pl::g_conn = c;

  // HANDSHAKE（path="" type_key=""——Rust/TS 同锚点）
  const auto body = pl::handshake_body(node_id, false);
  const auto hs = pl::encode_frame(PL_FT_HANDSHAKE, pl::mix64(++pl::g_cid_seed), 0,
                                   "", "", body.data(), body.size());
  if (!pl::send_all(fd, hs.data(), hs.size())) { pl::close_locked(); return -2; }

  // 等 HANDSHAKE_ACK（5s 预算）
  parrot::lite::Frame f;
  for (int waited = 0; waited < 5000; waited += 50) {
    if (!pl::wait_readable(fd, 50)) continue;
    uint8_t tmp[65536];
    const ssize_t r = ::recv(fd, tmp, sizeof(tmp), 0);
    if (r <= 0) { pl::close_locked(); return -2; }
    c->decoder.feed(tmp, static_cast<size_t>(r));
    int err = 0;
    while (c->decoder.next(&f, &err)) {
      if (f.ft == PL_FT_HANDSHAKE_ACK) {
        c->handshake_ok = true;
        // 重注册（幂等——TS lite 同语义）
        for (const auto &r2 : c->registrations) {
          const auto rf = pl::encode_frame(
              PL_FT_TELL, pl::mix64(++pl::g_cid_seed), 0, r2.second,
              "$receptionist.register:" + r2.first, nullptr, 0);
          pl::send_all(fd, rf.data(), rf.size());
        }
        return 0;
      }
    }
    if (err) { pl::close_locked(); return -4; }
  }
  pl::close_locked();
  return -3;
}

int pl_register(const char *key, const char *path, const char *caps_json) {
  (void)caps_json; // 载荷留空（桥侧约定——TS lite 同口径）
  std::lock_guard<std::mutex> lk(pl::g_mu);
  if (!pl::g_conn) return -2;
  if (!key || !path) return -1;
  pl::g_conn->registrations.push_back({key, path});
  if (!pl::g_conn->handshake_ok) return 0; // 连上后补发
  const auto f = pl::encode_frame(
      PL_FT_TELL, pl::mix64(++pl::g_cid_seed), 0, path,
      std::string("$receptionist.register:") + key, nullptr, 0);
  return pl::send_all(pl::g_conn->fd, f.data(), f.size()) ? 0 : -2;
}

int pl_ask(const char *target_path, const char *type_key,
           const uint8_t *payload, size_t len,
           uint8_t **out, size_t *out_len, int timeout_ms) {
  if (!target_path || !type_key || !out || !out_len) return -1;
  pl::Pending p;
  uint64_t cid = 0;
  {
    std::lock_guard<std::mutex> lk(pl::g_mu);
    if (!pl::g_conn || !pl::g_conn->handshake_ok) return -2;
    cid = pl::mix64(++pl::g_cid_seed);
    // reply_to：{node_id}/lite（Rust ingress 回程约定）
    const std::string reply_to = pl::g_conn->node_id + "/lite";
    const auto body = pl::with_reply_to(reply_to, payload, len);
    const auto f = pl::encode_frame(PL_FT_ASK, cid, 0, target_path, type_key,
                                    body.data(), body.size());
    if (!pl::send_all(pl::g_conn->fd, f.data(), f.size())) return -2;
    pl::g_conn->pending[cid] = &p;
  }

  // ── 帮助泵等待（DEV_08 核心）：抢到泵权自己收（顺带配对他人回复），
  //    抢不到则 cv 小睡——绝不持锁等 IO，多线程并发 ask 真并发。
  const auto deadline = std::chrono::steady_clock::now() +
      std::chrono::milliseconds(timeout_ms <= 0 ? 3000 : timeout_ms);
  std::vector<pl::Event> events;
  for (;;) {
    {
      std::lock_guard<std::mutex> lk(p.mu);
      if (p.done) break;
    }
    auto now = std::chrono::steady_clock::now();
    if (now >= deadline) {
      std::lock_guard<std::mutex> g2(pl::g_mu);
      if (pl::g_conn) pl::g_conn->pending.erase(cid);
      return -3;
    }
    const int remain = static_cast<int>(
        std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now).count());
    std::unique_lock<std::mutex> pump(pl::g_pump_mu, std::try_to_lock);
    if (pump.owns_lock()) {
      uint64_t my_gen;
      {
        std::lock_guard<std::mutex> g2(pl::g_mu);
        if (!pl::g_conn) return -2; // 连接在等待间被关闭
        my_gen = pl::g_conn->gen;
      }
      if (!pl::pump_once(my_gen, std::min(remain, 10), &events)) {
        // 连接断：本 ask 失败（close_locked 已唤醒所有 pending——含本者）
        std::lock_guard<std::mutex> lk(p.mu);
        if (p.done && p.err_code != 0) return p.err_code;
        return -2;
      }
      pl::fire_events(&events); // 泵权在手、状态锁已放——回调重入安全
      continue;
    }
    // 他人持泵：cv 小睡等配对通知
    std::unique_lock<std::mutex> lk(p.mu);
    p.cv.wait_for(lk, std::chrono::milliseconds(std::min(remain, 5)),
                  [&] { return p.done; });
  }

  if (p.err_code != 0) return p.err_code;
  *out = static_cast<uint8_t *>(malloc(p.reply.size() ? p.reply.size() : 1));
  if (!*out) return -5;
  memcpy(*out, p.reply.data(), p.reply.size());
  *out_len = p.reply.size();
  return 0;
}

int pl_tell(const char *target_path, const char *type_key,
            const uint8_t *payload, size_t len) {
  std::lock_guard<std::mutex> lk(pl::g_mu);
  if (!pl::g_conn || !pl::g_conn->handshake_ok) return -2;
  if (!target_path || !type_key) return -1;
  const auto f = pl::encode_frame(PL_FT_TELL, pl::mix64(++pl::g_cid_seed), 0,
                                  target_path, type_key, payload, len);
  return pl::send_all(pl::g_conn->fd, f.data(), f.size()) ? 0 : -2;
}

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
  pl::fire_events(&events);
}

int pl_pump_start(void) {
  if (pl::g_pump_run.exchange(true)) return 0; // 已在跑（幂等）
  try {
    pl::g_pump_thread = std::thread(pl::pump_thread_main);
  } catch (...) {
    pl::g_pump_run.store(false);
    return -5;
  }
  return 0;
}

void pl_pump_stop(void) {
  if (!pl::g_pump_run.exchange(false)) return;
  if (pl::g_pump_thread.joinable()) pl::g_pump_thread.join();
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
  std::lock_guard<std::mutex> pump(pl::g_pump_mu);
  std::lock_guard<std::mutex> lk(pl::g_mu);
  pl::close_locked();
}

} // extern "C"

// ── C++ 层实现 ─────────────────────────────────────────
namespace parrot::lite {

std::vector<uint8_t> Frame::encode() const {
  // 注意：这里编的是"净帧"（无 reply_to 前缀）——C++ 客户端侧构造数据帧用
  return pl::encode_frame(ft, cid, flags, path, type_key, payload.data(), payload.size());
}

void FrameDecoder::feed(const uint8_t *data, size_t len) {
  // 游标后追加（pos_ 前的已消费前缀仅在读过头时才压缩——见 next()）
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
  if (body_len > PL_MAX_FRAME_LEN) { *err = -4; buf_.clear(); pos_ = 0; return false; }
  if (avail < 4 + body_len) { compact(); return false; } // 半包不消费
  const uint8_t *b = h + 4;
  if (b[0] != PL_WIRE_VERSION) { *err = -4; buf_.clear(); pos_ = 0; return false; }
  out->ft = b[1];
  out->flags = static_cast<uint16_t>(b[2] | (b[3] << 8));
  out->cid = 0;
  for (int i = 0; i < 8; i++) out->cid |= static_cast<uint64_t>(b[4 + i]) << (8 * i);
  out->hop_count = b[12];
  out->hop_limit = b[13];
  // reserved u48 @14..20——不动（向前兼容）
  uint32_t path_len = 0;
  for (int i = 0; i < 4; i++)
    path_len |= static_cast<uint32_t>(b[20 + i]) << (8 * i);
  if (static_cast<size_t>(24) + path_len > body_len) { *err = -4; buf_.clear(); pos_ = 0; return false; }
  out->path.assign(reinterpret_cast<const char *>(b + 24), path_len);
  const size_t key_off = 24 + path_len;
  uint32_t key_len = 0;
  for (int i = 0; i < 4; i++)
    key_len |= static_cast<uint32_t>(b[key_off + i]) << (8 * i);
  if (key_off + 4 + key_len > body_len) { *err = -4; buf_.clear(); pos_ = 0; return false; }
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
