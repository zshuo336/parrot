// A test-only receive seam controls TCP chunk boundaries without changing frames
// or completion state. All bytes still come from the real peer socket.
#include <sys/socket.h>
#include <unistd.h>
#include <poll.h>
#include <chrono>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <string>
#include <atomic>
static size_t first_read_size = 0, first_read_received = 0;
static bool first_read_done = false;
static ssize_t completion_recv(int fd, void *buffer, size_t length, int flags);
#define recv completion_recv
#include "parrot_lite.cpp"
#undef recv

static std::atomic<unsigned> callbacks{0};
static void on_message(const char *, const char *, const uint8_t *, size_t, uint64_t) {
  ++callbacks;
}

static ssize_t completion_recv(int fd, void *buffer, size_t length, int flags) {
  if (!first_read_size || first_read_done) return ::recv(fd, buffer, length, flags);
  first_read_done = true;
  if (first_read_size > length) { errno = EMSGSIZE; return -1; }
  const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
  while (first_read_received < first_read_size && std::chrono::steady_clock::now() < deadline) {
    const auto n = ::recv(fd, static_cast<char *>(buffer) + first_read_received,
                          first_read_size - first_read_received, flags);
    if (n > 0) { first_read_received += static_cast<size_t>(n); continue; }
    if (n == 0) break;
    if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) return -1;
    pollfd wait{fd, POLLIN, 0};
    ::poll(&wait, 1, 10);
  }
  if (first_read_received != first_read_size) { errno = ETIMEDOUT; return -1; }
  return static_cast<ssize_t>(first_read_received);
}

int main(int argc, char **argv) {
  if (argc != 4) return 2;
  const std::string mode = argv[1];
  const bool split = mode.find("-split-") != std::string::npos;
  const bool violation_first = mode.find("-first-") != std::string::npos;
  const bool error = mode.find("-remote-error") != std::string::npos;
  const bool empty = mode.find("-empty") != std::string::npos;
  const int connected = pl_connect(argv[2], "completion-order", nullptr);
  if (connected) { std::fprintf(stderr, "FAIL terminal handshake %d\n", connected); return 1; }
  pl_set_on_ask(on_message, nullptr);
  first_read_size = 0;
  for (const char *digit = argv[3]; *digit; ++digit) {
    if (*digit < '0' || *digit > '9' || first_read_size > 6553) { pl_close(); return 2; }
    first_read_size = first_read_size * 10 + static_cast<unsigned>(*digit - '0');
  }
  if (!first_read_size || first_read_size > 65536) { pl_close(); return 2; }
  uint8_t *out = reinterpret_cast<uint8_t *>(1); size_t length = 99;
  const int status = pl_ask("/terminal", "pb:k", nullptr, 0, &out, &length, 1500);
  const int expected = violation_first ? -(100 + PL_ERR_PROTOCOL_VIOLATION) : (error ? -103 : 0);
  const bool outputs = expected == 0 ? (out && length == (empty ? 0u : 1u) && (empty || out[0] == 42)) : (!out && length == 0);
  pl_free(out);
  // Split delivery returns the completed reply before the later bad envelope.
  if (split) pl_poll(100);
  const int after = pl_tell("/after", "pb:k", nullptr, 0);
  pl_close();
  if (!first_read_done || first_read_received != first_read_size) {
    std::fprintf(stderr, "FAIL completion read boundary %zu/%zu\n", first_read_received, first_read_size); return 1;
  }
  if (after != -2 || status != expected || !outputs || callbacks.load()) {
    std::fprintf(stderr, "FAIL terminal result status=%d expected=%d outputs=%d after=%d callbacks=%u\n",
        status, expected, int(outputs), after, callbacks.load()); return 1;
  }
  std::puts("TERMINAL ERROR PASS");
  return 0;
}
