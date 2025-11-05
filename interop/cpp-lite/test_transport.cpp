// A real peer drives this binary from test_transport.py. Keep dangerous probes
// in subprocesses so a sanitizer finding cannot kill the test orchestrator.
#include "parrot_lite.h"
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <limits>
#include <mutex>
#include <string>
#include <thread>
#include <sys/socket.h>
#include <netinet/in.h>
#include <unistd.h>

static std::atomic<unsigned> callbacks{0};
static bool payload_ok = false;
static std::string current_mode;
static int callback_result = 0;
static std::mutex gate_mu;
static std::condition_variable gate_changed;
static bool gate_entered = false, gate_release = false, gate_paired = false;
static bool gate_cancelled = false, gate_failed = false;
static void on_message(const char *path, const char *key, const uint8_t *p,
                       size_t len, uint64_t cid) {
  if (current_mode.rfind("reply-deadline-", 0) == 0) {
    std::unique_lock<std::mutex> lock(gate_mu);
    if (std::strcmp(path, "/hold") == 0) {
      gate_entered = true; gate_changed.notify_all();
      if (!gate_changed.wait_for(lock, std::chrono::seconds(3), [] { return gate_release; }))
        gate_failed = true;
    } else if (std::strcmp(path, "/paired") == 0) {
      gate_paired = true; gate_changed.notify_all();
    } else gate_failed = true;
    return;
  }
  payload_ok = std::string(path) == "/target" && std::string(key) == "pb:k" &&
               len == 3 && p && p[0] == 7 && p[1] == 8 && p[2] == 9 && cid == 42;
  if (current_mode == "callback-close") pl_close();
  if (current_mode == "callback-stop-pump") pl_pump_stop();
  if (current_mode == "callback-ask") {
    uint8_t *reply = nullptr; size_t size = 0;
    callback_result = pl_ask("/nested", "pb:k", nullptr, 0, &reply, &size, 500);
    if (callback_result == 0 && (size != 1 || reply[0] != 99)) callback_result = -99;
    pl_free(reply);
  }
  ++callbacks;
}
static int fail(const char *what, int rc = 0) {
  std::fprintf(stderr, "FAIL %s (rc=%d)\n", what, rc);
  return 1;
}
// Constrain only this fixture's connected socket. A legal 512 KiB frame must
// still exercise partial writes; production does not expose test-only knobs.
static bool constrain_send_buffer(const char *url) {
  const auto port = std::stoul(std::string(url).substr(std::string(url).rfind(':') + 1));
  const long descriptor_limit = ::sysconf(_SC_OPEN_MAX);
  unsigned matched = 0;
  for (int fd = 3; fd < descriptor_limit; ++fd) {
    sockaddr_in peer{}; socklen_t size = sizeof(peer);
    if (::getpeername(fd, reinterpret_cast<sockaddr *>(&peer), &size) ||
        peer.sin_family != AF_INET || ntohs(peer.sin_port) != port) continue;
    const int requested = 4096;
    if (::setsockopt(fd, SOL_SOCKET, SO_SNDBUF, &requested, sizeof(requested))) return false;
    int actual = 0; size = sizeof(actual);
    if (::getsockopt(fd, SOL_SOCKET, SO_SNDBUF, &actual, &size) || actual <= 0 || actual > 65536) return false;
    ++matched;
  }
  return matched == 1;
}
int main(int argc, char **argv) {
  if (argc != 3) return 2;
  const std::string mode = argv[1];
  current_mode = mode;
  if (mode == "close-handshake") {
    int rc = 0;
    std::thread connecting([&] { rc = pl_connect(argv[2], "cpp-contract", nullptr); });
    if (std::getchar() != 'g') std::abort(); // peer received HELLO
    const auto begin = std::chrono::steady_clock::now();
    pl_close(); connecting.join();
    const auto elapsed = std::chrono::steady_clock::now() - begin;
    if (rc == 0 || elapsed > std::chrono::milliseconds(600)) return fail("close must cancel handshake", rc);
    return 0;
  }
  const int connected = pl_connect(argv[2], "cpp-contract", mode == "tls" ? "/requested-tls" : nullptr);
  if (mode == "reject-handshake" || mode == "tls") {
    if (connected == 0) { pl_close(); return fail("invalid handshake/TLS request succeeded"); }
    if (pl_tell("/x", "pb:k", nullptr, 0) != -2) return fail("failed connect published connection");
    return 0;
  }
  if (connected != 0) return fail("valid handshake", connected);
  if (mode.rfind("reply-deadline-", 0) == 0) {
    pl_set_on_ask(on_message, nullptr);
    const bool late = mode.find("late-") != std::string::npos;
    const bool error = mode.find("-error") != std::string::npos;
    const auto begin = std::chrono::steady_clock::now();
    std::thread poller([&] {
      {
        std::unique_lock<std::mutex> lock(gate_mu);
        if (!gate_changed.wait_for(lock, std::chrono::seconds(2), [] { return gate_entered || gate_cancelled; }) || gate_cancelled) {
          gate_failed = true; gate_release = true; gate_changed.notify_all(); return;
        }
      }
      if (late) std::this_thread::sleep_until(begin + std::chrono::milliseconds(800));
      std::putchar('r'); std::fflush(stdout);
      const auto limit = std::chrono::steady_clock::now() + std::chrono::seconds(2);
      for (;;) {
        {
          std::lock_guard<std::mutex> lock(gate_mu);
          if (gate_paired || gate_cancelled) break;
          if (std::chrono::steady_clock::now() >= limit) { gate_failed = true; break; }
        }
        pl_poll(10);
      }
      if (!late) std::this_thread::sleep_until(begin + std::chrono::milliseconds(800));
      std::lock_guard<std::mutex> lock(gate_mu);
      gate_release = true; gate_changed.notify_all();
    });
    uint8_t *out = reinterpret_cast<uint8_t *>(1); size_t len = 99;
    const int rc = pl_ask("/deadline", "pb:k", nullptr, 0, &out, &len, 500);
    const auto returned = std::chrono::steady_clock::now();
    {
      std::lock_guard<std::mutex> lock(gate_mu);
      gate_cancelled = true; gate_changed.notify_all();
    }
    poller.join();
    const int expected = late ? -3 : (error ? -103 : 0);
    const bool correct = rc == expected && (expected == 0 ? (out && len == 1 && out[0] == 42) : (!out && len == 0));
    pl_free(out); out = nullptr; len = 0;
    // Exercise connection recovery even when the original implementation returns the wrong status.
    const int next = pl_ask("/after-deadline", "pb:k", nullptr, 0, &out, &len, 1000);
    const bool recovered = next == 0 && out && len == 1 && out[0] == 99;
    pl_free(out); pl_close();
    if (gate_failed || !gate_entered || !gate_paired || returned < begin + std::chrono::milliseconds(800))
      return fail("reply deadline barriers were not satisfied", rc);
    if (!recovered) return fail("reply deadline must preserve connection", next);
    return correct ? 0 : fail("reply deadline completion status/output", rc);
  }
  if (mode.rfind("negotiated-", 0) == 0) {
    const bool coalesced = mode.find("coalesced") != std::string::npos;
    const size_t limit = static_cast<size_t>(std::stoul(mode.substr(mode.rfind('-') + 1)));
    if (!coalesced) {
      std::vector<uint8_t> payload(limit - 28 + 1, 42);
      if (pl_tell("", "", payload.data(), payload.size()) != -1) return fail("negotiated outbound limit ignored");
      if (pl_tell("", "", payload.data(), payload.size() - 1) != 0) return fail("exact limit or connection poisoned");
    }
    int state = 0;
    for (int n=0; n<100 && state==0; ++n) {
      pl_poll(10);
      state=pl_tell("", "", nullptr, 0);
    }
    pl_close();
    return state == -2 ? 0 : fail("oversized inbound prefix did not close connection", state);
  }
  if (mode.rfind("error-code-", 0) == 0) {
    const int code = std::stoi(mode.substr(11));
    uint8_t *out = reinterpret_cast<uint8_t *>(1); size_t len = 99;
    const int rc = pl_ask("/error", "pb:k", nullptr, 0, &out, &len, 1500);
    if (rc != -(100 + code)) return fail("valid error must preserve code", rc);
    if (out != nullptr || len != 0) return fail("error must clear reply output");
    const int next = pl_ask("/after-error", "pb:k", nullptr, 0, &out, &len, 1500);
    const bool recovered = next == 0 && len == 1 && out && out[0] == 99;
    pl_free(out); pl_close();
    return recovered ? 0 : fail("valid error must preserve connection", next);
  }
  if (mode == "disconnect" || mode == "background-disconnect") {
    if (mode == "background-disconnect" && pl_pump_start() != 0) return fail("start pump");
    uint8_t *out = reinterpret_cast<uint8_t *>(1);
    size_t len = 99;
    const int rc = pl_ask("/target", "pb:k", nullptr, 0, &out, &len, 1500);
    pl_pump_stop();
    // This used to dereference a returned pl_ask stack frame after peer EOF.
    pl_close();
    if (rc != -(100 + PL_ERR_CONNECTION_LOST)) return fail("EOF must fail pending ask", rc);
    if (out != nullptr || len != 0) return fail("failure must clear output parameters");
    return 0;
  }
  if (mode == "ask-write-deadline" || mode == "close-write" || mode == "queued-deadline" || mode == "queued-recovery" || mode == "heartbeat-backpressure" || mode == "reconnect-write" || mode == "tell-write-deadline" || mode == "register-write-deadline") {
    if (!constrain_send_buffer(argv[2])) return fail("fixture send buffer");
    std::vector<uint8_t> bytes(512 * 1024, 0x5a);
    int sending = 0;
    const auto begin = std::chrono::steady_clock::now();
    std::thread writer([&] {
      if (mode == "ask-write-deadline") {
        uint8_t *out = reinterpret_cast<uint8_t *>(1); size_t len = 99;
        sending = pl_ask("/large", "pb:k", bytes.data(), bytes.size(), &out, &len, 300);
        if (out || len) std::abort();
      } else if (mode == "register-write-deadline") {
        std::string path(bytes.size(), 'p');
        sending = pl_register("key", path.c_str(), nullptr);
      } else sending = pl_tell("/large", "pb:k", bytes.data(), bytes.size());
    });
    if (std::getchar() != 'g') std::abort(); // peer has first length prefix
    if (mode == "heartbeat-backpressure") pl_poll(10);
    if (mode == "reconnect-write") {
      int connected = pl_connect(argv[2], "cpp-replacement", nullptr);
      writer.join();
      if (connected || sending == 0) return fail("reconnect did not cancel old writer", connected);
      uint8_t *out = nullptr; size_t len = 0;
      int rc = pl_ask("/replacement", "pb:k", nullptr, 0, &out, &len, 1000);
      const bool correct = rc == 0 && len == 1 && out[0] == 99;
      pl_free(out); pl_close();
      return correct ? 0 : fail("old writer affected new generation", rc);
    }
    if (mode == "tell-write-deadline" || mode == "register-write-deadline") {
      writer.join();
      if (sending != -3 || std::chrono::steady_clock::now() - begin > std::chrono::milliseconds(3800))
        return fail("default write deadline", sending);
      if (pl_tell("/after", "pb:k", nullptr, 0) != -2) return fail("partial timed-out frame kept alive");
      return 0;
    }
    int queued = -3;
    const auto cancel_begin = std::chrono::steady_clock::now();
    if (mode == "queued-deadline" || mode == "queued-recovery" || mode == "heartbeat-backpressure") {
      uint8_t *out = nullptr; size_t len = 0;
      queued = pl_ask("/queued", "pb:k", nullptr, 0, &out, &len, 150);
      if (out || len) std::abort();
    }
    if (mode == "queued-recovery" || mode == "heartbeat-backpressure") { std::putchar('r'); std::fflush(stdout); }
    else if (mode != "ask-write-deadline") pl_close();
    writer.join();
    const auto elapsed = std::chrono::steady_clock::now() - (mode == "ask-write-deadline" ? begin : cancel_begin);
    if (elapsed > std::chrono::milliseconds(1200)) return fail("write/lock deadline exceeded", sending);
    if (mode == "ask-write-deadline" && sending != -3) return fail("write deadline status", sending);
    if (mode == "queued-recovery" || mode == "heartbeat-backpressure") {
      if (queued != -3 || sending != 0) return fail("queued timeout poisoned owner write", queued);
      if (mode == "heartbeat-backpressure") pl_poll(10);
      if (pl_tell("/after", "pb:k", nullptr, 0) != 0) return fail("zero-byte timeout poisoned connection");
      pl_close(); return 0;
    }
    if (mode != "ask-write-deadline" && sending == 0) return fail("cancelled write succeeded");
    if (queued != -3) return fail("queued ASK deadline", queued);
    if (pl_tell("/after", "pb:k", nullptr, 0) != -2) return fail("partial write reused connection");
    pl_close(); return 0;
  }
  if (mode == "inflight-barrier") {
    std::atomic<unsigned> failures{0};
    std::vector<std::thread> callers;
    for (uint8_t i=0; i<8; ++i) callers.emplace_back([&, i] {
      uint8_t *out = nullptr; size_t size = 0;
      int rc = pl_ask("/barrier", "pb:k", &i, 1, &out, &size, 1500);
      if (rc || size != 1 || !out || out[0] != i) ++failures;
      pl_free(out);
    });
    for (auto &caller : callers) caller.join();
    pl_close();
    return failures.load() ? fail("eight requests must be in flight before any reply", failures.load()) : 0;
  }
  if (mode == "concurrent-frames") {
    if (pl_pump_start()) return fail("start concurrent pump");
    std::atomic<unsigned> failures{0};
    std::vector<std::thread> callers;
    for (unsigned i=0; i<8; ++i) callers.emplace_back([&, i] {
      std::vector<uint8_t> data(128 * 1024, static_cast<uint8_t>(i));
      for (int n=0; n<3; ++n) {
        uint8_t *out = nullptr; size_t len = 0;
        int rc = pl_ask("/parallel", "pb:k", data.data(), data.size(), &out, &len, 2000);
        if (rc || len != 1 || !out || out[0] != i) ++failures;
        pl_free(out);
      }
    });
    for (auto &caller : callers) caller.join();
    pl_pump_stop(); pl_close();
    return failures.load() ? fail("concurrent frames/replies corrupted", failures.load()) : 0;
  }
  if (mode == "exit-pump") return pl_pump_start();
  if (mode == "limit") {
    uint8_t bytes[100] = {};
    if (pl_tell("/target", "pb:k", bytes, sizeof(bytes)) != -1) return fail("peer limit ignored");
    if (pl_tell("/target", "pb:k", bytes, 1) != 0) return fail("limit rejection poisoned connection");
    pl_close(); return 0;
  }
  if (mode == "timeout-recovery") {
    uint8_t *reply = nullptr; size_t size = 0;
    int rc = pl_ask("/timeout", "pb:k", nullptr, 0, &reply, &size, 80);
    if (rc != -3) return fail("first ask should time out", rc);
    rc = pl_ask("/second", "pb:k", nullptr, 0, &reply, &size, 1000);
    bool correct = rc == 0 && size == 1 && reply[0] == 99;
    pl_free(reply); pl_close(); return correct ? 0 : fail("late reply corrupted new ask", rc);
  }
  if (mode == "outbound-bounds") {
    uint8_t byte = 1, *out = nullptr; size_t len = 0;
    if (pl_tell("/target", "pb:k", nullptr, 1) != -1) return fail("null nonempty tell");
    if (pl_ask("/target", "pb:k", nullptr, 1, &out, &len, 100) != -1) return fail("null nonempty ask");
    if (pl_tell("/target", "pb:k", &byte, std::numeric_limits<size_t>::max()) != -1) return fail("overflowing tell size");
    if (pl_ask("/target", "pb:k", &byte, std::numeric_limits<size_t>::max(), &out, &len, 100) != -1) return fail("overflowing ask size");
    if (pl_tell("/target", "pb:k", &byte, 1) != 0) return fail("invalid caller input poisoned connection");
    pl_close(); return 0;
  }
  pl_set_on_ask(on_message, nullptr);
  if (pl_tell("/ready", "pb:ready", nullptr, 0) != 0) return fail("ready barrier");
  if (mode == "callback-stop-pump") {
    if (pl_pump_start() != 0) return fail("start callback pump");
    for (int i=0; i<200 && !callbacks.load(); ++i) std::this_thread::sleep_for(std::chrono::milliseconds(5));
    pl_pump_stop(); pl_close();
    if (callbacks.load() != 1) return fail("self-stop callback did not finish");
    if (pl_pump_start() != 0) return fail("restart stopped pump");
    pl_pump_stop(); return 0;
  }
  int state = 0;
  for (int i = 0; i < 100; ++i) {
    pl_poll(10);
    state = pl_tell("/probe", "pb:probe", nullptr, 0);
    if (callbacks.load() || state != 0) break;
  }
  const unsigned calls = callbacks.load();
  pl_close();
  if (mode == "callback-close") return calls == 1 && state == -2 ? 0 : fail("callback close deadlocked or leaked");
  if (mode == "callback-ask") return calls == 1 && callback_result == 0 ? 0 : fail("callback nested ask", callback_result);
  if (mode == "valid-message" || mode == "coalesced") return calls == 1 && payload_ok && state == 0 ? 0 : fail("valid callback");
  return calls == 0 && state == -2 ? 0 : fail("malformed frame reached callback or left connection usable", state);
}
