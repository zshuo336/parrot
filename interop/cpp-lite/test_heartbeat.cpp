#include "parrot_lite.h"
#include <atomic>
#include <cstdio>
#include <cstring>

static std::atomic<unsigned> callbacks{0};
static void on_message(const char *, const char *, const uint8_t *, size_t, uint64_t) {
  ++callbacks;
}

int main(int argc, char **argv) {
  if (argc != 3) return 2;
  const bool valid = std::strcmp(argv[1], "valid") == 0;
  if (!valid && std::strcmp(argv[1], "invalid") != 0) return 2;
  const int connected = pl_connect(argv[2], "heartbeat-envelope", nullptr);
  if (connected) { std::fprintf(stderr, "heartbeat fixture handshake failed: %d\n", connected); return 1; }
  pl_set_on_ask(on_message, nullptr);
  uint8_t *out = reinterpret_cast<uint8_t *>(1);
  size_t size = 99;
  const int status = pl_ask("/heartbeat-first", "pb:k", nullptr, 0, &out, &size, 1500);
  const int expected = valid ? 0 : -(100 + PL_ERR_PROTOCOL_VIOLATION);
  bool correct = status == expected;
  correct = correct && (valid ? out && size == 3 && out[0] == 17 && out[1] == 34 && out[2] == 51
                             : !out && size == 0);
  pl_free(out);
  if (valid && correct) {
    out = reinterpret_cast<uint8_t *>(1); size = 99;
    const int second = pl_ask("/heartbeat-next", "pb:k", nullptr, 0, &out, &size, 1500);
    correct = second == 0 && out && size == 2 && out[0] == 68 && out[1] == 85;
    pl_free(out);
  }
  const int after = pl_tell("/heartbeat-finished", "pb:k", nullptr, 0);
  const unsigned called = callbacks.load();
  pl_close();
  if (!correct || called || after != (valid ? 0 : -2)) {
    std::fprintf(stderr, "heartbeat envelope mismatch: status=%d expected=%d outputs=%d callbacks=%u after=%d\n",
        status, expected, int(correct), called, after);
    return 1;
  }
  std::printf("HEARTBEAT ENVELOPE PASS %s\n", argv[1]);
  return 0;
}
