// Parrot C++ Lite 实现（DEV_04 §5）——POSIX 阻塞 socket + Wire 1.0。
//
// 依赖：仅 libc（单线程模型：pl_ask 阻塞等待 / pl_poll 主动泵；
// 线程安全由调用方域保证）。tls_dir 预留（mTLS 由 Rust 网关侧终接，
// C ABI 走明文内网/本机形态——04 §9 混合体部署约定）。
//
// 协议对齐锚点（与 Rust/TS 逐字节一致）：
//   - HANDSHAKE/HANDSHAKE_ACK 帧：path="" type_key=""，载荷 TLV；
//   - ASK 载荷 reply_to 前缀：[u32 len]["{node_id}/lite"][payload]；
//   - 注册（receptionist）：TELL type_key="$receptionist.register:{key}"。

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
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <cstdlib>
#include <mutex>

namespace pl {
namespace {

std::atomic<uint64_t> g_cid_seed{0x9E3779B97F4A7C15ull};

uint64_t mix64(uint64_t z) {
  z += 0x9E3779B97F4A7C15ull;
  z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ull;
  z = (z ^ (z >> 27)) * 0x94D049BB133111EBull;
  return z ^ (z >> 31);
}

struct Conn {
  int fd = -1;
  std::string node_id;
  bool handshake_ok = false;
  parrot::lite::FrameDecoder decoder;
  pl_on_ask_t on_ask = nullptr;
  void *userdata = nullptr;
  // 重连后自动重注册（TS lite 同语义）
  std::vector<std::pair<std::string, std::string>> registrations;
};
Conn *g_conn = nullptr;
std::mutex g_mu;

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

// 收一批字节 → 解码器；返回 false = 连接断
bool pump_recv(Conn *c) {
  uint8_t tmp[65536];
  const ssize_t r = ::recv(c->fd, tmp, sizeof(tmp), 0);
  if (r <= 0) return false;
  c->decoder.feed(tmp, static_cast<size_t>(r));
  return true;
}

// 内部 close（调用方持锁）
void close_locked() {
  if (!g_conn) return;
  if (g_conn->fd >= 0) ::close(g_conn->fd);
  delete g_conn;
  g_conn = nullptr;
}

// 错误帧载荷 → C ABI 负数码（-101..-113 ↔ ErrCode 1..13）
int err_payload_to_code(const std::vector<uint8_t> &p) {
  if (p.size() < 4) return -4;
  const int code = p[0] | (p[1] << 8);
  return -(100 + code);
}

} // namespace
} // namespace pl

// ── C ABI 实现 ─────────────────────────────────────────
extern "C" {

int pl_connect(const char *url, const char *node_id, const char *tls_dir) {
  (void)tls_dir; // 明文形态（网关侧终接 mTLS）
  if (!url || !node_id) return -1;
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
    if (!pl::pump_recv(c)) { pl::close_locked(); return -2; }
    int err = 0;
    while (c->decoder.next(&f, &err)) {
      if (f.ft == PL_FT_HANDSHAKE_ACK) {
        c->handshake_ok = true;
        // 重注册（幂等——TS lite 同语义）
        for (const auto &r : c->registrations) {
          const auto rf = pl::encode_frame(
              PL_FT_TELL, pl::mix64(++pl::g_cid_seed), 0, r.second,
              "$receptionist.register:" + r.first, nullptr, 0);
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
  std::lock_guard<std::mutex> lk(pl::g_mu);
  if (!pl::g_conn || !pl::g_conn->handshake_ok) return -2;
  if (!target_path || !type_key || !out || !out_len) return -1;
  const uint64_t cid = pl::mix64(++pl::g_cid_seed);
  // reply_to：{node_id}/lite（Rust ingress 回程约定）
  const std::string reply_to = pl::g_conn->node_id + "/lite";
  const auto body = pl::with_reply_to(reply_to, payload, len);
  const auto f = pl::encode_frame(PL_FT_ASK, cid, 0, target_path, type_key,
                                  body.data(), body.size());
  if (!pl::send_all(pl::g_conn->fd, f.data(), f.size())) return -2;

  parrot::lite::Frame r;
  const auto deadline =
      std::chrono::steady_clock::now() +
      std::chrono::milliseconds(timeout_ms <= 0 ? 3000 : timeout_ms);
  for (;;) {
    const auto now = std::chrono::steady_clock::now();
    if (now >= deadline) return -3;
    const int remain = static_cast<int>(
        std::chrono::duration_cast<std::chrono::milliseconds>(deadline - now).count());
    if (!pl::wait_readable(pl::g_conn->fd, remain)) return -3;
    if (!pl::pump_recv(pl::g_conn)) return -2;
    int err = 0;
    while (pl::g_conn->decoder.next(&r, &err)) {
      if (r.cid != cid) continue; // 迟到 REPLY / 旁路帧——跳过
      if (r.ft == PL_FT_REPLY) {
        *out = static_cast<uint8_t *>(malloc(r.payload.size() ? r.payload.size() : 1));
        if (!*out) return -5;
        memcpy(*out, r.payload.data(), r.payload.size());
        *out_len = r.payload.size();
        return 0;
      }
      if (r.ft == PL_FT_REPLY_ERR) return pl::err_payload_to_code(r.payload);
    }
    if (err) return -4;
  }
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
  std::lock_guard<std::mutex> lk(pl::g_mu);
  if (!pl::g_conn) return;
  if (!pl::wait_readable(pl::g_conn->fd, timeout_ms)) return;
  if (!pl::pump_recv(pl::g_conn)) return;
  parrot::lite::Frame f;
  int err = 0;
  while (pl::g_conn->decoder.next(&f, &err)) {
    if ((f.ft == PL_FT_ASK || f.ft == PL_FT_TELL) && pl::g_conn->on_ask) {
      // ASK 载荷剥 reply_to 前缀后派发（与 Rust ingress 同口径）
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
      pl::g_conn->on_ask(f.path.c_str(), f.type_key.c_str(), real, real_len, f.cid);
    }
  }
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
  return pl::encode_frame(ft, cid, flags, path, type_key, payload.data(), payload.size());
}

void FrameDecoder::feed(const uint8_t *data, size_t len) {
  buf_.insert(buf_.end(), data, data + len);
}

bool FrameDecoder::next(Frame *out, int *err) {
  *err = 0;
  if (buf_.size() < 4) return false;
  uint32_t body_len = 0;
  for (int i = 0; i < 4; i++)
    body_len |= static_cast<uint32_t>(buf_[i]) << (8 * i);
  if (body_len > PL_MAX_FRAME_LEN) { *err = -4; buf_.clear(); return false; }
  if (buf_.size() < 4 + body_len) return false; // 半包不消费
  const uint8_t *b = buf_.data() + 4;
  if (b[0] != PL_WIRE_VERSION) { *err = -4; buf_.clear(); return false; }
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
  if (static_cast<size_t>(24) + path_len > body_len) { *err = -4; buf_.clear(); return false; }
  out->path.assign(reinterpret_cast<const char *>(b + 24), path_len);
  const size_t key_off = 24 + path_len;
  uint32_t key_len = 0;
  for (int i = 0; i < 4; i++)
    key_len |= static_cast<uint32_t>(b[key_off + i]) << (8 * i);
  if (key_off + 4 + key_len > body_len) { *err = -4; buf_.clear(); return false; }
  out->type_key.assign(reinterpret_cast<const char *>(b + key_off + 4), key_len);
  const size_t payload_off = key_off + 4 + key_len;
  out->payload.assign(b + payload_off, b + body_len);
  buf_.erase(buf_.begin(), buf_.begin() + 4 + body_len);
  return true;
}

uint64_t new_cid() { return pl::mix64(++pl::g_cid_seed); }

} // namespace parrot::lite
