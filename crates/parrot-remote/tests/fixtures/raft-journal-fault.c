#include <dlfcn.h>
#include <errno.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static _Atomic unsigned state_syncs;

int fsync(int fd) {
    char descriptor[64];
    char path[4096];
    int count = snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", fd);
    if (count > 0 && (size_t)count < sizeof(descriptor)) {
        ssize_t length = readlink(descriptor, path, sizeof(path) - 1);
        if (length >= 0) {
            path[length] = '\0';
            const char *suffix = "/raft.state";
            size_t suffix_length = strlen(suffix);
            if ((size_t)length >= suffix_length &&
                strcmp(path + length - suffix_length, suffix) == 0 &&
                atomic_fetch_add(&state_syncs, 1) == 1) {
                errno = EIO;
                return -1;
            }
        }
    }
    int (*real_sync)(int) = dlsym(RTLD_NEXT, "fsync");
    if (!real_sync) {
        errno = EIO;
        return -1;
    }
    return real_sync(fd);
}
