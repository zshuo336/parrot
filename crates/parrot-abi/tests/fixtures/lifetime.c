// Independent C ABI fixture: observations live in host memory. No observer
// Library reference may keep this image alive after the loader releases it.
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
typedef struct { const uint8_t *ptr; uint32_t len; } AbiStr;
typedef struct { const uint8_t *key; uint32_t key_len; const uint8_t *payload; uint32_t payload_len; } AbiMsg;
typedef struct { uint8_t *buf; uint32_t cap; uint32_t written; } AbiReplyBuf;
typedef struct { uint32_t (*handle_msg)(void*, AbiMsg, AbiReplyBuf*); uint32_t (*on_drain)(void*); } AbiVt;
typedef struct { const AbiVt *vt; void *self; } AbiComponent;
typedef struct { uint32_t (*construct)(AbiStr, AbiComponent**); void (*destroy)(AbiComponent*); } AbiHooks;
typedef struct { uint32_t abi_version; uint32_t parrot_min; const uint8_t *name; uint32_t name_len; AbiHooks hooks; } AbiMeta;
_Static_assert(sizeof(_Atomic uint64_t) == sizeof(uint64_t), "host atomic layout");
static _Atomic uint64_t *host;
static uint32_t handle(void *self, AbiMsg msg, AbiReplyBuf *out) {
    (void)self; (void)msg; out->written = 0; return 0;
}
static uint32_t drain(void *self) {
    (void)self;
    atomic_fetch_add(&host[0], 1);
    while (!atomic_load(&host[1])) { }
    return 0;
}
static const AbiVt vt = { handle, drain };
static uint32_t construct(AbiStr cfg, AbiComponent **out) {
    char address[32];
    if (!cfg.len || cfg.len >= sizeof(address)) return 2;
    memcpy(address, cfg.ptr, cfg.len); address[cfg.len] = 0;
    host = (_Atomic uint64_t *)(uintptr_t)strtoull(address, NULL, 10);
    if (!host) return 2;
    AbiComponent *comp = malloc(sizeof(*comp));
    if (!comp) return 2;
    comp->vt = &vt; comp->self = comp; *out = comp;
    return 0;
}
static void destroy(AbiComponent *comp) {
    free(comp);
    atomic_fetch_add(&host[2], 1);
}
__attribute__((destructor)) static void closed(void) {
    if (host) atomic_fetch_add(&host[3], 1);
}
const AbiMeta PARROT_ABI_META = { 1, 1, (const uint8_t *)"lifetime", 8, { construct, destroy } };
