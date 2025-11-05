// C++ lite 性能/并发验证（DEV_08）。
//
// 用法：
//   ./test_perf <port>    —— 连 Rust echo 节点跑并发 ask 与吞吐
//   ./test_perf 0         —— 本地单元：后端探测 + 游标解码器批量帧
//
// 覆盖：
//   P1 后端选择：pl_pollset_create/backend 断言（macOS=kqueue / Linux=epoll）
//   P2 解码器：10k 帧内容逐帧校验；时间只是观测值，不能单独证明复杂度。
//   P3 并发 ask：8 线程 × 200 唯一载荷，逐请求精确配对。
//      真正在途并发由 test_transport.py 的收齐八请求再回包屏障验证。
//   P4 单线程 ask 往返延迟 p50（参考值打印）

#include "parrot_lite.h"
#include "parrot_poll.h"

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cerrno>
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>
#include <unistd.h>
#include <vector>

#include "assert_t.h" // CHECK 宏

using Clock = std::chrono::steady_clock;

// Performance floors belong to an explicit isolated release gate. Shared debug
// CI cannot infer in-flight concurrency from wall-clock throughput on one run.
static void performance_limit(const char *name, double measured, bool minimum) {
  const char *text = std::getenv(name);
  if (!text) return;
  errno = 0; char *end = nullptr;
  const double limit = std::strtod(text, &end);
  const bool valid = text[0] && end && !*end && errno == 0 && std::isfinite(limit) && limit > 0;
  CHECK(valid, "performance limit must be a positive finite number");
  if (!valid) return;
  printf("PERF LIMIT %s=%.3f measured=%.3f\n", name, limit, measured);
  CHECK(minimum ? measured >= limit : measured <= limit,
        minimum ? "requested throughput floor" : "requested decoder time ceiling");
}

static void test_backend_probe() {
  const int before = g_fail;
  pl_pollset *ps = pl_pollset_create();
  CHECK(ps != nullptr, "pollset create");
  if (!ps) return;
  const char *be = pl_pollset_backend(ps);
  printf("backend: %s (compiled: %s)\n", be, pl_backend_name());
#if defined(__APPLE__) || defined(__FreeBSD__)
  CHECK(strcmp(be, "kqueue") == 0 || strcmp(be, "poll") == 0, "bsd: kqueue or degraded poll");
#elif defined(__linux__)
  CHECK(strcmp(be, "epoll") == 0 || strcmp(be, "poll") == 0, "linux: epoll or degraded poll");
#endif
  // ctl/wait 冒烟（自 pipe 读端）
  int fds[2];
  const int created = pipe(fds);
  CHECK(created == 0, "pipe");
  if (created != 0) { pl_pollset_destroy(ps); return; }
  CHECK(pl_pollset_ctl(ps, fds[0], true) == 0, "ctl add");
  char w = 'x';
  CHECK(::write(fds[1], &w, 1) == 1, "pipe write");
  CHECK(pl_pollset_wait(ps, 100) == 1, "wait readable");
  CHECK(pl_pollset_ready_at(ps, 0) == fds[0], "ready fd");
  CHECK(pl_pollset_ctl(ps, fds[0], false) == 0, "ctl del");
  ::close(fds[0]);
  ::close(fds[1]);
  pl_pollset_destroy(ps);
  printf("P1 backend-select %s\n", g_fail == before ? "PASS" : "FAIL");
}

static void test_decoder_bulk() {
  const int before = g_fail;
  // 10k 小帧一次性 feed —— 游标式必须远快于 O(n²)
  parrot::lite::FrameDecoder dec;
  std::vector<uint8_t> wire;
  const int N = 10000;
  for (int i = 0; i < N; i++) {
    parrot::lite::Frame f;
    f.ft = PL_FT_TELL;
    f.cid = static_cast<uint64_t>(i);
    f.path = "/user/bench";
    f.type_key = "bin:u:Tick";
    f.payload = std::vector<uint8_t>(16, static_cast<uint8_t>(i & 0xFF));
    auto b = f.encode();
    wire.insert(wire.end(), b.begin(), b.end());
  }
  const auto t0 = Clock::now();
  dec.feed(wire.data(), wire.size());
  int got = 0;
  parrot::lite::Frame out;
  int err = 0;
  while (dec.next(&out, &err)) {
    CHECK(out.cid == static_cast<uint64_t>(got), "bulk cid order");
    CHECK(out.ft == PL_FT_TELL && out.path == "/user/bench" && out.type_key == "bin:u:Tick" &&
          out.payload == std::vector<uint8_t>(16, static_cast<uint8_t>(got & 0xff)), "bulk frame content");
    if (out.cid != static_cast<uint64_t>(got)) break;
    got++;
  }
  const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - t0).count();
  CHECK(err == 0, "bulk decode err");
  CHECK(got == N, "bulk all frames");
  performance_limit("PL_CPP_MAX_DECODE_MS", static_cast<double>(ms), false);
  printf("P2 decoder-bulk %s (%d frames in %lldms)\n", g_fail == before ? "PASS" : "FAIL", got, (long long)ms);
}

static void test_concurrent_ask(int port) {
  char url[64];
  snprintf(url, sizeof(url), "parrot://127.0.0.1:%d", port);
  CHECK(pl_connect(url, "cpp-perf-1", nullptr) == 0, "connect");

  const int THREADS = 8;
  // Longer explicit performance samples amortize startup/scheduling noise.
  const int PER = std::getenv("PL_CPP_MIN_QPS") ? 2000 : 200;
  std::atomic<int> ok{0};
  std::atomic<int> fail{0};

  // ── 并行组（帮助泵：同连接在途多 ask）─────────────────────
  const auto t0 = Clock::now();
  std::vector<std::thread> ts;
  for (int t = 0; t < THREADS; t++) {
    ts.emplace_back([&ok, &fail, t, PER] {
      for (int i = 0; i < PER; i++) {
        uint8_t req[8] = {static_cast<uint8_t>(t), static_cast<uint8_t>(i), static_cast<uint8_t>(i >> 8), 4, 5, 6, 7, 8};
        uint8_t *out = nullptr;
        size_t out_len = 0;
        const int rc = pl_ask("/user/echo", "bin:u:Echo", req, sizeof(req),
                              &out, &out_len, 10000);
        if (rc == 0 && out_len == 8 && memcmp(out, req, 8) == 0) {
          ok++;
          pl_free(out);
        } else {
          fail++;
          pl_free(out); // 失败也可能有分配（防泄漏路径覆盖）
        }
      }
    });
  }
  for (auto &th : ts) th.join();
  const auto par_ms =
      std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - t0).count();

  // ── 串行基线（相同请求数，逐条验证）──────────────────────────────
  const auto t1 = Clock::now();
  for (int i = 0; i < THREADS * PER; i++) {
    uint8_t req[8] = {0xff, static_cast<uint8_t>(i), static_cast<uint8_t>(i >> 8), 4, 5, 6, 7, 8};
    uint8_t *out = nullptr;
    size_t out_len = 0;
    const int rc = pl_ask("/user/echo", "bin:u:Echo", req, sizeof(req),
                          &out, &out_len, 10000);
    CHECK(rc == 0 && out_len == sizeof(req) && out && std::memcmp(out, req, sizeof(req)) == 0, "serial echo exact payload");
    pl_free(out);
  }
  const auto ser_ms =
      std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - t1).count();

  CHECK(fail == 0, "concurrent ask zero fail");
  CHECK(ok == THREADS * PER, "concurrent ask all paired");
  // Equal request counts and floating-point seconds avoid integer-ms division
  // by zero. A measured rate is reported even when no release floor is set.
  const double seconds = std::chrono::duration<double>(t1 - t0).count();
  const double qps = seconds > 0 ? (THREADS * PER) / seconds : 0;
  printf("P3 concurrent-ask %s (parallel %lldms, %.0f ask/s, serial-base %lldms)\n",
         fail == 0 && ok == THREADS * PER ? "PASS" : "FAIL", (long long)par_ms, qps, (long long)ser_ms);
  performance_limit("PL_CPP_MIN_QPS", qps, true);

  // ── p50 延迟参考（100 次单线程往返）──────────────────────
  std::vector<long long> lat;
  for (int i = 0; i < 100; i++) {
    uint8_t req[8] = {9};
    uint8_t *out = nullptr;
    size_t out_len = 0;
    const auto s = Clock::now();
    const int rc = pl_ask("/user/echo", "bin:u:Echo", req, sizeof(req), &out, &out_len, 5000);
    const auto e = Clock::now();
    CHECK(rc == 0 && out_len == sizeof(req) && out && std::memcmp(out, req, sizeof(req)) == 0, "latency echo exact payload");
    pl_free(out);
    lat.push_back(std::chrono::duration_cast<std::chrono::microseconds>(e - s).count());
  }
  std::sort(lat.begin(), lat.end());
  printf("P4 ask latency p50=%lldus p99=%lldus (ref)\n", lat[50], lat[99]);

  pl_close();
  printf("CPP-LITE CHECKS %s\n", g_fail == 0 ? "PASS" : "FAIL");
}

int main(int argc, char **argv) {
  const int port = argc > 1 ? atoi(argv[1]) : 0;
  if (port == 0) {
    test_backend_probe();
    test_decoder_bulk();
    CHECK(!std::getenv("PL_CPP_MIN_QPS"), "throughput gate requires a live peer port");
    printf("CPP-LITE CHECKS LOCAL %s\n", g_fail == 0 ? "PASS" : "FAIL");
    return g_fail == 0 ? 0 : 1;
  }
  test_backend_probe();
  test_decoder_bulk();
  test_concurrent_ask(port);
  return g_fail == 0 ? 0 : 1;
}
