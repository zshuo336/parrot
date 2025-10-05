// Parrot 轮询后端实现（poll / epoll / kqueue 三态）。

#include "parrot_poll.h"

#include <errno.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#if defined(PL_FORCE_POLL)
#define PL_BACKEND_POLL 1
#elif defined(PL_FORCE_EPOLL)
#define PL_BACKEND_EPOLL 1
#elif defined(PL_FORCE_KQUEUE)
#define PL_BACKEND_KQUEUE 1
#else
// auto：按平台选最优；内核态创建失败时运行时降级 poll
#if defined(__linux__)
#define PL_BACKEND_EPOLL 1
#define PL_EPOLL_FALLBACK_POLL 1
#elif defined(__APPLE__) || defined(__FreeBSD__) || defined(__OpenBSD__) || \
    defined(__NetBSD__) || defined(__DragonFly__)
#define PL_BACKEND_KQUEUE 1
#define PL_KQUEUE_FALLBACK_POLL 1
#else
#define PL_BACKEND_POLL 1
#endif
#endif

#if PL_BACKEND_EPOLL
#include <sys/epoll.h>
#elif PL_BACKEND_KQUEUE
#include <sys/event.h>
#endif

struct pl_pollset {
  /// >= 0：epoll/kqueue 内核 fd；-1：poll 兜底态
  int backend_fd;
  char backend[16];
  /// 就绪 fd 列表（三种后端统一出参形态）
  int ready[PL_POLL_MAX_FDS];
  int n_ready;
  /// poll 兜底态登记表
  struct pollfd pfds[PL_POLL_MAX_FDS];
  int n_pfds;
};

static void set_poll_mode(pl_pollset *ps) {
  ps->backend_fd = -1;
  memcpy(ps->backend, "poll", 5);
  ps->n_ready = 0;
  ps->n_pfds = 0;
}

pl_pollset *pl_pollset_create(void) {
  pl_pollset *ps = (pl_pollset *)calloc(1, sizeof(pl_pollset));
  if (!ps) return nullptr;
#if PL_BACKEND_EPOLL
  int fd = epoll_create1(EPOLL_CLOEXEC);
  if (fd >= 0) {
    ps->backend_fd = fd;
    memcpy(ps->backend, "epoll", 6);
    return ps;
  }
#if PL_EPOLL_FALLBACK_POLL
  set_poll_mode(ps); // 内核裁剪/fd 上限——降级 poll
  return ps;
#endif
#elif PL_BACKEND_KQUEUE
  int fd = kqueue();
  if (fd >= 0) {
    ps->backend_fd = fd;
    memcpy(ps->backend, "kqueue", 7);
    return ps;
  }
#if PL_KQUEUE_FALLBACK_POLL
  set_poll_mode(ps);
  return ps;
#endif
#endif
  set_poll_mode(ps);
  return ps;
}

void pl_pollset_destroy(pl_pollset *ps) {
  if (!ps) return;
  if (ps->backend_fd >= 0) close(ps->backend_fd);
  free(ps);
}

int pl_pollset_ctl(pl_pollset *ps, int fd, bool want_read) {
  if (!ps || fd < 0) return -1;
#if PL_BACKEND_EPOLL
  if (ps->backend_fd >= 0) {
    struct epoll_event ev;
    memset(&ev, 0, sizeof(ev));
    ev.data.fd = fd;
    ev.events = EPOLLIN;
    int op = want_read ? EPOLL_CTL_ADD : EPOLL_CTL_DEL;
    if (epoll_ctl(ps->backend_fd, op, fd, &ev) < 0) {
      if (want_read && errno == EEXIST) return 0; // 幂等
      if (!want_read && errno == ENOENT) return 0;
      return -1;
    }
    return 0;
  }
#elif PL_BACKEND_KQUEUE
  if (ps->backend_fd >= 0) {
    struct kevent ev;
    EV_SET(&ev, fd, EVFILT_READ, want_read ? EV_ADD : EV_DELETE, 0, 0, nullptr);
    if (kevent(ps->backend_fd, &ev, 1, nullptr, 0, nullptr) < 0) {
      if (errno == EEXIST || errno == ENOENT) return 0; // 幂等
      return -1;
    }
    return 0;
  }
#endif
  // poll 兜底：登记表 upsert
  int slot = -1;
  for (int i = 0; i < ps->n_pfds; i++) {
    if (ps->pfds[i].fd == fd) {
      slot = i;
      break;
    }
  }
  if (!want_read) { // 删除：尾部交换压缩
    if (slot >= 0) {
      ps->pfds[slot] = ps->pfds[ps->n_pfds - 1];
      ps->n_pfds--;
    }
    return 0;
  }
  if (slot < 0) {
    if (ps->n_pfds >= PL_POLL_MAX_FDS) return -1; // 表满
    slot = ps->n_pfds++;
    ps->pfds[slot].fd = fd;
  }
  ps->pfds[slot].events = POLLIN;
  ps->pfds[slot].revents = 0;
  return 0;
}

int pl_pollset_wait(pl_pollset *ps, int timeout_ms) {
  if (!ps) return -1;
  ps->n_ready = 0;
#if PL_BACKEND_EPOLL
  if (ps->backend_fd >= 0) {
    struct epoll_event evs[PL_POLL_MAX_FDS];
    int n = epoll_wait(ps->backend_fd, evs, PL_POLL_MAX_FDS, timeout_ms);
    for (int i = 0; i < n; i++) ps->ready[i] = evs[i].data.fd;
    ps->n_ready = n > 0 ? n : 0;
    return ps->n_ready;
  }
#elif PL_BACKEND_KQUEUE
  if (ps->backend_fd >= 0) {
    struct kevent evs[PL_POLL_MAX_FDS];
    struct timespec ts, *pts = nullptr;
    if (timeout_ms >= 0) {
      ts.tv_sec = timeout_ms / 1000;
      ts.tv_nsec = (long)(timeout_ms % 1000) * 1000000L;
      pts = &ts;
    }
    int n = kevent(ps->backend_fd, nullptr, 0, evs, PL_POLL_MAX_FDS, pts);
    int w = 0;
    for (int i = 0; i < n; i++) {
      if (evs[i].flags & EV_ERROR) continue;
      ps->ready[w++] = (int)evs[i].ident;
    }
    ps->n_ready = w;
    return w;
  }
#endif
  // poll 兜底
  int n = poll(ps->pfds, (nfds_t)ps->n_pfds, timeout_ms);
  if (n <= 0) return n < 0 && errno == EINTR ? 0 : n;
  int w = 0;
  for (int i = 0; i < ps->n_pfds && w < n; i++) {
    if (ps->pfds[i].revents & (POLLIN | POLLERR | POLLHUP)) ps->ready[w++] = ps->pfds[i].fd;
  }
  ps->n_ready = w;
  return w;
}

int pl_pollset_ready_at(pl_pollset *ps, int i) {
  if (!ps || i < 0 || i >= ps->n_ready) return -1;
  return ps->ready[i];
}

const char *pl_pollset_backend(pl_pollset *ps) { return ps ? ps->backend : "none"; }
