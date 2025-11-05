// Independent ABI fixture for actual dlclose boundaries, without Rust runtime TLS.
#define _POSIX_C_SOURCE 200809L
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include <time.h>
typedef struct { const uint8_t *ptr; uint32_t len; } AbiStr;
typedef struct { const uint8_t *key; uint32_t key_len; const uint8_t *payload; uint32_t payload_len; } AbiMsg;
typedef struct { uint8_t *buf; uint32_t cap; uint32_t written; } AbiReplyBuf;
typedef struct { uint32_t (*handle_msg)(void*, AbiMsg, AbiReplyBuf*); uint32_t (*on_drain)(void*); } AbiVt;
typedef struct { const AbiVt *vt; void *self; } AbiComponent;
typedef struct { uint32_t (*construct)(AbiStr, AbiComponent**); void (*destroy)(AbiComponent*); } AbiHooks;
typedef struct { uint32_t abi_version; uint32_t parrot_min; const uint8_t *name; uint32_t name_len; AbiHooks hooks; } AbiMeta;
_Static_assert(sizeof(_Atomic uint64_t) == sizeof(uint64_t), "host atomic layout");
static _Atomic uint64_t *host;
// Slots: construct-enter, construct-gate, construct, drain, destroy,
// unload-enter, unload-gate, unload-exit, expired. Host owns their storage.
static int gate(size_t slot) {
    struct timespec start, now;
    clock_gettime(CLOCK_MONOTONIC, &start);
    while (!atomic_load(&host[slot])) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec - start.tv_sec >= 20) {
            atomic_fetch_add(&host[8], 1);
            return 0;
        }
        const struct timespec pause = {0, 1000000};
        nanosleep(&pause, NULL);
    }
    return 1;
}
static uint32_t handle(void *self, AbiMsg msg, AbiReplyBuf *out) {
    (void)self;
    if (msg.key_len == 5 && memcmp(msg.key, "error", 5) == 0) return 2;
    if (msg.payload_len > out->cap) return 5;
    if (msg.payload_len) memcpy(out->buf, msg.payload, msg.payload_len);
    out->written = msg.payload_len;
    return 0;
}
static uint32_t drain(void *self) {
    (void)self;
    atomic_fetch_add(&host[3], 1);
    return 0;
}
static const AbiVt vt = { handle, drain };
static uint32_t construct(AbiStr cfg, AbiComponent **out) {
    if (!cfg.len || cfg.len > 65536 || host) return 2;
    char *text = malloc((size_t)cfg.len + 1);
    if (!text) return 2;
    memcpy(text, cfg.ptr, cfg.len); text[cfg.len] = 0;
    // The test creates this exact integer field in the TOML envelope. This is
    // a fixture protocol, not a general TOML parser or an untrusted host API.
    const char *field = "probe_address = ";
    char *value = strstr(text, field);
    if (!value) { free(text); return 2; }
    char *end;
    uintptr_t address = (uintptr_t)strtoull(value + strlen(field), &end, 10);
    if (!address || (*end != '\n' && *end != '\r' && *end != 0)) {
        free(text); return 2;
    }
    free(text);
    host = (_Atomic uint64_t *)address;
    atomic_fetch_add(&host[0], 1);
    if (!gate(1)) return 2;
    AbiComponent *comp = malloc(sizeof(*comp));
    if (!comp) return 2;
    comp->vt = &vt; comp->self = comp; *out = comp;
    atomic_fetch_add(&host[2], 1);
    return 0;
}
static void destroy(AbiComponent *comp) {
    free(comp);
    atomic_fetch_add(&host[4], 1);
}
__attribute__((destructor)) static void closed(void) {
    if (!host) return;
    atomic_fetch_add(&host[5], 1);
    (void)gate(6);
    atomic_fetch_add(&host[7], 1);
}
const AbiMeta PARROT_ABI_META = {1, 1, (const uint8_t *)"unload", 6, {construct, destroy}};
