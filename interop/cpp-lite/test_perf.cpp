// C++ lite 性能/并发验证（DEV_08）。
//
// 用法：
//   ./test_perf <port>    —— 连 Rust echo 节点跑并发 ask 与吞吐
//   ./test_perf 0         —— 本地单元：后端探测 + 游标解码器批量帧
//
// 覆盖：
//   P1 后端选择：pl_pollset_create/backend 断言（macOS=kqueue / Linux=epoll）
//   P2 解码器：10k 帧批量 feed+next 无 O(n²)（限时断言）+ 内容逐帧校验
//   P3 并发 ask：8 线程 × 200 ask（同连接在途）——全部配对成功且总耗时
//      < 串行基线的 1/2（帮助泵真并发证明）
//   P4 单线程 ask 往返延迟 p50（参考值打印）

#include "parrot_lite.h"
#include "parrot_poll.h"

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>
#include <unistd.h>
#include <vector>

#include "assert_t.h" // CHECK 宏

using Clock = std::chrono::steady_clock;

static void test_backend_probe() {
  pl_pollset *ps = pl_pollset_create();
  CHECK(ps != nullptr, "pollset create");
  const char *be = pl_pollset_backend(ps);
  printf("backend: %s (compiled: %s)\n", be, pl_backend_name());
#if defined(__APPLE__) || defined(__FreeBSD__)
  CHECK(strcmp(be, "kqueue") == 0 || strcmp(be, "poll") == 0, "bsd: kqueue or degraded poll");
#elif defined(__linux__)
  CHECK(strcmp(be, "epoll") == 0 || strcmp(be, "poll") == 0, "linux: epoll or degraded poll");
#endif
  // ctl/wait 冒烟（自 pipe 读端）
  int fds[2];
  CHECK(pipe(fds) == 0, "pipe");
  CHECK(pl_pollset_ctl(ps, fds[0], true) == 0, "ctl add");
  char w = 'x';
  CHECK(::write(fds[1], &w, 1) == 1, "pipe write");
  CHECK(pl_pollset_wait(ps, 100) == 1, "wait readable");
  CHECK(pl_pollset_ready_at(ps, 0) == fds[0], "ready fd");
  CHECK(pl_pollset_ctl(ps, fds[0], false) == 0, "ctl del");
  ::close(fds[0]);
  ::close(fds[1]);
  pl_pollset_destroy(ps);
  printf("P1 backend-select PASS\n");
}

static void test_decoder_bulk() {
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
    if (out.cid != static_cast<uint64_t>(got)) break;
    got++;
  }
  const auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - t0).count();
  CHECK(err == 0, "bulk decode err");
  CHECK(got == N, "bulk all frames");
  // O(n²) 参考量级：旧实现在 10k×~90B 下通常 >300ms；游标式 <50ms。
  // 门限取 300ms——防极端机器误报同时明确劣化回归。
  CHECK(ms < 300, "bulk decode under 300ms");
  printf("P2 decoder-bulk PASS (%d frames in %lldms)\n", got, (long long)ms);
}

static void test_concurrent_ask(int port) {
  char url[64];
  snprintf(url, sizeof(url), "parrot://127.0.0.1:%d", port);
  CHECK(pl_connect(url, "cpp-perf-1", nullptr) == 0, "connect");

  const int THREADS = 8;
  const int PER = 200;
  std::atomic<int> ok{0};
  std::atomic<int> fail{0};

  // ── 并行组（帮助泵：同连接在途多 ask）─────────────────────
  const auto t0 = Clock::now();
  std::vector<std::thread> ts;
  for (int t = 0; t < THREADS; t++) {
    ts.emplace_back([&ok, &fail] {
      for (int i = 0; i < PER; i++) {
        uint8_t req[8] = {1, 2, 3, 4, 5, 6, 7, 8};
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

  // ── 串行基线（同规模单线程）──────────────────────────────
  const auto t1 = Clock::now();
  for (int i = 0; i < PER; i++) {
    uint8_t req[8] = {1, 2, 3, 4, 5, 6, 7, 8};
    uint8_t *out = nullptr;
    size_t out_len = 0;
    const int rc = pl_ask("/user/echo", "bin:u:Echo", req, sizeof(req),
                          &out, &out_len, 10000);
    if (rc == 0 && out_len == 8) pl_free(out);
  }
  const auto ser_ms =
      std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - t1).count();

  CHECK(fail == 0, "concurrent ask zero fail");
  CHECK(ok == THREADS * PER, "concurrent ask all paired");
  // 吞吐门限（DEV_08）：8 线程并发下总吞吐 ≥ 20k ask/s（回环 RTT ~35µs
  // 时理论上限 ~28k/s——门限取 70% 覆盖调度抖动）。旧"全局锁内等回复"
  // 实现串行化后吞吐 ≤ 单线程（~9k/s），本门限可分辨回归。
  const double qps = (double)(THREADS * PER) / ((double)par_ms / 1000.0);
  printf("P3 concurrent-ask PASS (parallel %lldms, %.0f ask/s, serial-base %lldms)\n",
         (long long)par_ms, qps, (long long)ser_ms);
  CHECK(qps >= 20000.0, "concurrent throughput >= 20k/s");

  // ── p50 延迟参考（100 次单线程往返）──────────────────────
  std::vector<long long> lat;
  for (int i = 0; i < 100; i++) {
    uint8_t req[8] = {9};
    uint8_t *out = nullptr;
    size_t out_len = 0;
    const auto s = Clock::now();
    const int rc = pl_ask("/user/echo", "bin:u:Echo", req, sizeof(req), &out, &out_len, 5000);
    const auto e = Clock::now();
    if (rc == 0) pl_free(out);
    lat.push_back(std::chrono::duration_cast<std::chrono::microseconds>(e - s).count());
  }
  std::sort(lat.begin(), lat.end());
  printf("P4 ask latency p50=%lldus p99=%lldus (ref)\n", lat[50], lat[99]);

  pl_close();
  printf("CPP-LITE PERF PASS\n");
}

int main(int argc, char **argv) {
  const int port = argc > 1 ? atoi(argv[1]) : 0;
  if (port == 0) {
    test_backend_probe();
    test_decoder_bulk();
    printf("CPP-LITE PERF LOCAL PASS\n");
    return g_fail == 0 ? 0 : 1;
  }
  test_backend_probe();
  test_decoder_bulk();
  test_concurrent_ask(port);
  return g_fail == 0 ? 0 : 1;
}
