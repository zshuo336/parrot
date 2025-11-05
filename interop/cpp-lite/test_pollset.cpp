// Exercise real pipes, descriptor failure and signal interruption. The poll
// fallback's hard-error control uses a syscall seam; native errors use real EBADF.
#include <poll.h>
#include <signal.h>
#include <sys/time.h>
#include <unistd.h>
#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <memory>
#include <string>
static bool fail_poll = false;
static int fixture_poll(pollfd *fds, nfds_t count, int timeout);
#define poll fixture_poll
#include "parrot_poll.cpp"
#undef poll

static int fixture_poll(pollfd *fds, nfds_t count, int timeout) {
  if (fail_poll) { errno = EBADF; return -1; }
  return ::poll(fds, count, timeout);
}
static volatile sig_atomic_t interrupted = 0;
static void alarm_handler(int) { interrupted = 1; }
struct Pipe {
  int read_fd = -1, write_fd = -1;
  Pipe() {
    int descriptors[2];
    if (::pipe(descriptors) == 0) { read_fd = descriptors[0]; write_fd = descriptors[1]; }
  }
  ~Pipe() {
    if (read_fd >= 0) ::close(read_fd);
    if (write_fd >= 0) ::close(write_fd);
  }
};
static int failure(const char *what, int result = 0) {
  std::fprintf(stderr, "FAIL pollset %s result=%d errno=%d\n", what, result, errno);
  return 1;
}
int main(int argc, char **argv) {
  if (argc != 3) return 2;
  const std::string mode = argv[1];
  std::unique_ptr<pl_pollset, decltype(&pl_pollset_destroy)> owner(pl_pollset_create(), pl_pollset_destroy);
  auto *ps = owner.get();
  if (!ps) return failure("create");
  const std::string backend = pl_pollset_backend(ps);
  if (backend != argv[2]) return failure("required backend unavailable");
  if (mode == "invalid") {
    if (pl_pollset_ctl(nullptr, 0, true) != -1 || pl_pollset_wait(nullptr, 0) != -1 ||
        pl_pollset_ready_at(nullptr, 0) != -1 || std::strcmp(pl_pollset_backend(nullptr), "none") ||
        pl_pollset_ctl(ps, -1, true) != -1 || pl_pollset_ready_at(ps, -1) != -1 ||
        pl_pollset_ready_at(ps, 0) != -1) return failure("invalid handles/bounds");
    pl_pollset_destroy(nullptr);
  } else {
    Pipe first, second;
    if (first.read_fd < 0 || second.read_fd < 0) return failure("pipe setup");
    if (pl_pollset_ctl(ps, first.read_fd, true) || pl_pollset_ctl(ps, first.read_fd, true))
      return failure("idempotent add");
    if (mode == "closed" || mode == "mixed") {
      if (backend != "poll") return failure("closed-fd case requires poll backend");
      const int retired = first.read_fd;
      if (::close(retired)) return failure("close watched descriptor");
      first.read_fd = -1;
      if (mode == "mixed" && (pl_pollset_ctl(ps, second.read_fd, true) || ::write(second.write_fd, "x", 1) != 1))
        return failure("mixed readiness setup");
      const int ready = pl_pollset_wait(ps, 0);
      const int expected = mode == "mixed" ? 2 : 1;
      if (ready != expected) return failure("invalid descriptor readiness lost", ready);
      bool saw_retired = false, saw_readable = false;
      for (int i = 0; i < ready; ++i) {
        const int fd = pl_pollset_ready_at(ps, i);
        if (fd == retired) saw_retired = true;
        else if (fd == second.read_fd) saw_readable = true;
        else return failure("unexpected ready descriptor", fd);
      }
      if (!saw_retired || (mode == "mixed" && !saw_readable)) return failure("ready identities");
      if (pl_pollset_ctl(ps, retired, false)) return failure("remove closed registration");
      if (mode == "mixed" && pl_pollset_ctl(ps, second.read_fd, false)) return failure("remove readable registration");
      if (pl_pollset_wait(ps, 0) != 0 || pl_pollset_ready_at(ps, 0) != -1) return failure("removed readiness persisted");
    } else {
      if (::write(first.write_fd, "x", 1) != 1 || pl_pollset_wait(ps, 100) != 1 ||
          pl_pollset_ready_at(ps, 0) != first.read_fd || pl_pollset_ready_at(ps, 1) != -1)
        return failure("initial readiness");
      if (mode == "hard-error") {
        const int descriptor = ps->backend_fd;
        if (descriptor >= 0) {
          if (::close(descriptor)) return failure("close native poll descriptor");
        } else fail_poll = true;
        errno = 0;
        const int ready = pl_pollset_wait(ps, 100);
        const int error = errno;
        const bool cleared = pl_pollset_ready_at(ps, 0) == -1;
        if (descriptor >= 0) ps->backend_fd = -1;
        fail_poll = false;
        if (ready != -1 || error != EBADF || !cleared) return failure("system error swallowed/stale result", ready);
      } else {
        char byte;
        if (::read(first.read_fd, &byte, 1) != 1) return failure("drain pipe");
        if (mode == "interrupt") {
          struct sigaction action{}, previous{};
          action.sa_handler = alarm_handler;
          sigemptyset(&action.sa_mask);
          if (::sigaction(SIGALRM, &action, &previous)) return failure("signal setup");
          itimerval timer{}; timer.it_value.tv_usec = 100000;
          if (::setitimer(ITIMER_REAL, &timer, nullptr)) return failure("timer setup");
          const auto begin = std::chrono::steady_clock::now();
          const int ready = pl_pollset_wait(ps, 1000);
          const auto elapsed = std::chrono::steady_clock::now() - begin;
          timer = {}; ::setitimer(ITIMER_REAL, &timer, nullptr);
          ::sigaction(SIGALRM, &previous, nullptr);
          if (!interrupted || ready != 0 || elapsed >= std::chrono::milliseconds(750) || pl_pollset_ready_at(ps, 0) != -1)
            return failure("EINTR must clear readiness and return zero", ready);
        } else if (mode == "lifecycle") {
          if (pl_pollset_ctl(ps, first.read_fd, false) || pl_pollset_ctl(ps, first.read_fd, false) ||
              pl_pollset_wait(ps, 0) != 0 || pl_pollset_ready_at(ps, 0) != -1)
            return failure("idempotent removal/reset");
        } else if (mode == "timeout") {
          if (pl_pollset_wait(ps, 0) != 0 || pl_pollset_ready_at(ps, 0) != -1)
            return failure("timeout must clear readiness");
        } else return failure("unknown test mode");
      }
    }
  }
  std::printf("PARROT_POLLSET_CASE {\"mode\":\"%s\",\"backend\":\"%s\",\"passed\":true}\n", mode.c_str(), backend.c_str());
  return 0;
}
