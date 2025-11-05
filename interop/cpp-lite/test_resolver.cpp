// Deterministic system-resolver seam: exercise the production connection path
// while libc name resolution is held behind an explicit condition-variable gate.
#include <netdb.h>
#include <atomic>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <thread>
static int resolver_fixture(const char *, const char *, const addrinfo *, addrinfo **);
#define getaddrinfo resolver_fixture
#include "parrot_lite.cpp"
#undef getaddrinfo

static std::mutex fixture_mu;
static std::condition_variable fixture_changed;
static bool entered = false, released = false;
static std::atomic<unsigned> attempts{0};
static int resolver_fixture(const char *host, const char *port, const addrinfo *hints, addrinfo **out) {
  if (hints->ai_flags & AI_NUMERICHOST) return ::getaddrinfo(host, port, hints, out);
  ++attempts;
  std::unique_lock<std::mutex> lock(fixture_mu);
  entered = true; fixture_changed.notify_all();
  fixture_changed.wait(lock, [] { return released; });
  *out = nullptr;
  return EAI_NONAME;
}
int main(int argc, char **argv) {
  if (argc != 2 && argc != 3) return 2;
  const bool delayed_observer = std::strcmp(argv[1], "timeout-observer-delay") == 0;
  const bool timeout = std::strcmp(argv[1], "timeout") == 0 || delayed_observer;
  if (!timeout && std::strcmp(argv[1], "cancel") != 0 && std::strcmp(argv[1], "dns-numeric") != 0) return 2;
  int result = 0;
  std::chrono::steady_clock::time_point call_begin, call_end;
  std::thread caller([&] {
    call_begin = std::chrono::steady_clock::now();
    result = pl_connect("tcp://resolver.invalid:1", "resolver", nullptr);
    call_end = std::chrono::steady_clock::now();
  });
  { std::unique_lock<std::mutex> lock(fixture_mu);
    if (!fixture_changed.wait_for(lock, std::chrono::seconds(2), [] { return entered; })) std::abort(); }
  const auto cancel_begin = std::chrono::steady_clock::now();
  if (!timeout) pl_close();
  caller.join();
  // Delay only the observer after the API caller has returned.
  if (delayed_observer) std::this_thread::sleep_for(std::chrono::milliseconds(1100));
  const auto observed = std::chrono::steady_clock::now();
  const auto elapsed = call_end - (timeout ? call_begin : cancel_begin);
  const bool correct = result == (timeout ? -3 : -2) &&
      (!timeout || elapsed >= std::chrono::seconds(5)) &&
      elapsed < (timeout ? std::chrono::milliseconds(5800) : std::chrono::milliseconds(600));
  // A second timed-out call must not spawn a second unresolved OS operation.
  const auto retry_begin = std::chrono::steady_clock::now();
  const int retry = timeout ? pl_connect("tcp://resolver.invalid:1", "resolver", nullptr) : -3;
  const auto retry_elapsed = std::chrono::steady_clock::now() - retry_begin;
  const bool bounded = attempts.load() == 1 && retry == -3 &&
      (!timeout || (retry_elapsed >= std::chrono::seconds(5) &&
                    retry_elapsed < std::chrono::milliseconds(5800)));
  const bool observation_valid = !delayed_observer || observed - call_end >= std::chrono::seconds(1);
  const bool numeric_ok = argc != 3 || pl_connect(argv[2], "numeric-recovery", nullptr) == 0;
  { std::lock_guard<std::mutex> lock(fixture_mu); released = true; }
  fixture_changed.notify_all();
  pl_close();
  // Retire the fixture worker before its gate objects leave static lifetime.
  pl::resolver.worker.join();
  std::fprintf(stderr, "resolver observation: mode=%s elapsed_us=%lld retry_us=%lld observer_us=%lld correct=%d bounded=%d numeric_ok=%d observation_valid=%d\n",
      argv[1], static_cast<long long>(std::chrono::duration_cast<std::chrono::microseconds>(elapsed).count()),
      static_cast<long long>(std::chrono::duration_cast<std::chrono::microseconds>(retry_elapsed).count()),
      static_cast<long long>(std::chrono::duration_cast<std::chrono::microseconds>(observed - call_end).count()),
      int(correct), int(bounded), int(numeric_ok), int(observation_valid));
  if (!correct || !bounded || !numeric_ok || !observation_valid) {
    std::fprintf(stderr, "resolver contract failed: result=%d retry=%d workers=%u\n", result, retry, attempts.load());
    return 1;
  }
  return 0;
}
