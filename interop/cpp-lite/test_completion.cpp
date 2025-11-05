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
static size_t first_read_size = 0, first_read_received = 0;
static bool first_read_done = false;
static ssize_t completion_recv(int fd, void *buffer, size_t length, int flags);
#define recv completion_recv
#include "parrot_lite.cpp"
#undef recv

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
  if (argc != 3) return 2;
  const std::string mode = argv[1];
  const bool split = mode.find("-split-") != std::string::npos;
  const bool violation_first = mode.find("-violation-first-") != std::string::npos;
  const bool error = mode.find("-error") != std::string::npos;
  const bool empty = mode.find("-empty") != std::string::npos;
  const int connected = pl_connect(argv[2], "completion-order", nullptr);
  if (connected) { std::fprintf(stderr, "FAIL completion handshake %d\n", connected); return 1; }
  // Wire fixed frame size is32 bytes including its length prefix.
  first_read_size = 32 + (error ? 4 : (empty ? 0 : 1)) + (split ? 0 : 32);
  uint8_t *out = reinterpret_cast<uint8_t *>(1); size_t length = 99;
  const int status = pl_ask("/completion", "pb:k", nullptr, 0, &out, &length, 1000);
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
  if (after != -2) { std::fprintf(stderr, "FAIL completion protocol violation stayed connected %d\n", after); return 1; }
  if (status != expected || !outputs) {
    std::fprintf(stderr, "FAIL completion result status=%d expected=%d outputs=%d\n", status, expected, outputs); return 1;
  }
  return 0;
}
