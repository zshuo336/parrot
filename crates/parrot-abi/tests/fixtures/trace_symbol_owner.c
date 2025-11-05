#include <stdint.h>
#include <stdlib.h>
#include <string.h>

/* Independent C declarations exercise the frozen ABI without Rust bindings. */
typedef struct { const uint8_t *ptr; uint32_t len; } AbiStr;
typedef struct {
    const uint8_t *type_key;
    uint32_t key_len;
    const uint8_t *payload;
    uint32_t payload_len;
} AbiMsg;
typedef struct { uint8_t *buf; uint32_t cap; uint32_t written; } AbiReplyBuf;
typedef struct { uint8_t trace_id[16]; uint8_t span_id[8]; } AbiTraceParentV1;
typedef struct {
    uint32_t (*handle_msg)(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *);
} AbiTraceV1;

#ifdef TRACE_DEPENDENCY
uint32_t trace_dependency_anchor(void) { return 0; }

uint32_t trace_dependency_message(void *self, AbiMsg msg, AbiReplyBuf *out,
                                  const AbiTraceParentV1 *parent) {
    static const uint8_t marker[] = "foreign trace";
    (void)self;
    (void)msg;
    (void)parent;
    if (out->cap < sizeof(marker) - 1) return 3;
    memcpy(out->buf, marker, sizeof(marker) - 1);
    out->written = sizeof(marker) - 1;
    return 0;
}

const AbiTraceV1 PARROT_ABI_TRACE_V1 = { trace_dependency_message };
#else
typedef struct {
    uint32_t (*handle_msg)(void *, AbiMsg, AbiReplyBuf *);
    uint32_t (*on_drain)(void *);
} AbiVt;
typedef struct { const AbiVt *vt; void *self; } AbiComponent;
typedef struct {
    uint32_t (*construct)(AbiStr, AbiComponent **);
    void (*destroy)(AbiComponent *);
} AbiHooks;
typedef struct {
    uint32_t abi_version;
    uint32_t parrot_min;
    const uint8_t *name;
    uint32_t name_len;
    AbiHooks hooks;
} AbiMeta;
typedef struct { AbiComponent component; int drained; } Instance;

extern uint32_t trace_dependency_anchor(void);

static uint32_t handle_message(void *self, AbiMsg msg, AbiReplyBuf *out) {
    if (((Instance *)self)->drained) return 2;
    if (msg.payload_len > out->cap) return 3;
    memcpy(out->buf, msg.payload, msg.payload_len);
    out->written = msg.payload_len;
    return 0;
}

static uint32_t drain(void *self) {
    ((Instance *)self)->drained = 1;
    return 0;
}

static const AbiVt VTABLE = { handle_message, drain };

static uint32_t construct(AbiStr cfg, AbiComponent **out) {
    (void)cfg;
    if (trace_dependency_anchor() != 0) return 2;
    Instance *instance = calloc(1, sizeof(*instance));
    if (instance == NULL) return 2;
    instance->component.vt = &VTABLE;
    instance->component.self = instance;
    *out = &instance->component;
    return 0;
}

static void destroy(AbiComponent *component) { free(component); }
static const uint8_t NAME[] = "trace-symbol-owner";
const AbiMeta PARROT_ABI_META = { 1, 1, NAME, sizeof(NAME) - 1, { construct, destroy } };

#ifdef TRACE_FORWARDING
/* A component-owned descriptor may intentionally delegate its hook's code. */
extern uint32_t trace_dependency_message(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *);
const AbiTraceV1 PARROT_ABI_TRACE_V1 = { trace_dependency_message };
#endif
#endif
