#include <dlfcn.h>
#include <errno.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static _Atomic unsigned matching_syncs;

int fsync(int fd) {
    char descriptor[64];
    char path[4096];
    const char *mode = getenv("PARROT_SNAPSHOT_FAIL_AT");
    const char *directory = getenv("PARROT_SNAPSHOT_FAULT_CHILD");
    int count = snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", fd);
    if (mode && directory && count > 0 && (size_t)count < sizeof(descriptor)) {
        ssize_t length = readlink(descriptor, path, sizeof(path) - 1);
        if (length >= 0) {
            path[length] = '\0';
            size_t suffix = strlen("/raft.next");
            int matches = strcmp(mode, "directory") == 0
                ? strcmp(path, directory) == 0
                : ((size_t)length >= suffix && strcmp(path + length - suffix, "/raft.next") == 0);
            if (matches && atomic_fetch_add(&matching_syncs, 1) == 1) {
                errno = EIO;
                return -1;
            }
        }
    }
    int (*real_sync)(int) = dlsym(RTLD_NEXT, "fsync");
    if (!real_sync) { errno = EIO; return -1; }
    return real_sync(fd);
}
