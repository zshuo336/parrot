/* SPDX-License-Identifier: AGPL-3.0-only */
#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <string.h>

DIR *opendir(const char *path) {
    if (strcmp(path, "/proc") == 0) {
        errno = EACCES;
        return NULL;
    }
    DIR *(*next)(const char *) = dlsym(RTLD_NEXT, "opendir");
    return next(path);
}
