/* Test-only system resolver boundary, loaded into one owned child process. */
#include <dlfcn.h>
#include <netdb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef int (*resolver_fn)(const char *, const char *, const struct addrinfo *, struct addrinfo **);
#ifdef __APPLE__
static int parrot_getaddrinfo(const char *name, const char *service,
                             const struct addrinfo *hints, struct addrinfo **result)
#else
int getaddrinfo(const char *name, const char *service,
                const struct addrinfo *hints, struct addrinfo **result)
#endif
{
#ifdef __APPLE__
    /* dyld excludes references from the interposing image itself. dlsym can
       return the interposed symbol and recurse, so use the original import. */
    resolver_fn original = getaddrinfo;
#else
    resolver_fn original = (resolver_fn)dlsym(RTLD_NEXT, "getaddrinfo");
#endif
    if (!original) return EAI_SYSTEM;
    if (name && strcmp(name, "changing.parrot.test") == 0) {
        const char *path = getenv("PARROT_TEST_DNS_STATE");
        if (!path) return EAI_FAIL;
        FILE *input = fopen(path, "r");
        if (!input) return EAI_FAIL;
        char address[128];
        char *read = fgets(address, sizeof(address), input);
        fclose(input);
        if (!read) return EAI_FAIL;
        address[strcspn(address, "\r\n")] = '\0';
        return original(address, service, hints, result);
    }
    return original(name, service, hints, result);
}
#ifdef __APPLE__
__attribute__((used)) static const struct {
    const void *replacement;
    const void *original;
} interpose __attribute__((section("__DATA,__interpose"))) = {
    (const void *)parrot_getaddrinfo, (const void *)getaddrinfo
};
#endif
