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
typedef struct AbiComponent AbiComponent;
typedef struct {
    uint32_t (*handle_msg)(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *);
} AbiTraceV1;
typedef struct { uint64_t handle; AbiTraceParentV1 parent; } AbiSpanV1;
typedef struct {
    void *context;
    uint32_t (*start)(void *, uint64_t, AbiStr, AbiSpanV1 *);
    uint32_t (*finish)(void *, uint64_t, uint32_t);
} AbiRecordingHostV1;
typedef struct {
    uint32_t (*handle_msg)(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
} AbiRecordingV1;
typedef struct {
    uint32_t (*construct)(AbiStr, AbiComponent **, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
    uint32_t (*on_drain)(void *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
    uint32_t (*destroy)(AbiComponent *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
} AbiLifecycleV1;
typedef struct {
    void *context;
    uint32_t (*ask)(void *, AbiStr, AbiMsg, AbiStr, AbiReplyBuf *, uint32_t);
    uint32_t (*tell)(void *, AbiStr, AbiMsg, uint32_t);
} AbiMessagingHostV1;
typedef struct {
    uint32_t (*handle_msg)(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *, const AbiMessagingHostV1 *);
} AbiMessagingV1;
typedef struct {
    void *context;
    uint32_t (*reg)(void *, AbiStr, uint32_t);
    uint32_t (*dereg)(void *, AbiStr, uint32_t);
    uint32_t (*subscribe)(void *, AbiStr, uint32_t, uint64_t *);
    uint32_t (*next)(void *, uint64_t, uint32_t, AbiReplyBuf *, uint32_t *);
    uint32_t (*close)(void *, uint64_t);
} AbiServicesHostV1;
typedef struct {
    uint32_t (*handle_msg)(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *, const AbiMessagingHostV1 *, const AbiServicesHostV1 *);
} AbiServicesV1;
_Static_assert(sizeof(AbiSpanV1) == 32, "independent span layout");
_Static_assert(sizeof(AbiLifecycleV1) == 3 * sizeof(void *), "independent lifecycle layout");
_Static_assert(sizeof(AbiMessagingHostV1) == 3 * sizeof(void *), "independent messaging host layout");
_Static_assert(sizeof(AbiServicesHostV1) == 6 * sizeof(void *), "independent services host layout");

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

uint32_t recording_dependency_message(void *self, AbiMsg msg, AbiReplyBuf *out,
                                     const AbiTraceParentV1 *parent, const AbiRecordingHostV1 *host) {
    static const uint8_t marker[] = "foreign recording";
    (void)self;
    (void)msg;
    (void)parent;
    if (host != NULL) {
        AbiSpanV1 span;
        AbiStr label = { marker, sizeof(marker) - 1 };
        if (host->start(host->context, 0, label, &span) != 0) return 2;
        if (span.handle != 123 || span.parent.span_id[0] != 11) return 2;
        if (host->finish(host->context, span.handle, 0) != 0) return 2;
    }
    if (out->cap < sizeof(marker) - 1) return 3;
    memcpy(out->buf, marker, sizeof(marker) - 1);
    out->written = sizeof(marker) - 1;
    return 0;
}
const AbiRecordingV1 PARROT_ABI_RECORDING_V1 = { recording_dependency_message };

static uint32_t lifecycle_record(const AbiTraceParentV1 *parent, const AbiRecordingHostV1 *host) {
    uint8_t bytes[64];
    AbiReplyBuf out = { bytes, sizeof(bytes), 0 };
    AbiMsg msg = { NULL, 0, NULL, 0 };
    if (parent == NULL) return 2;
    for (unsigned i = 0; i < 16; ++i) if (parent->trace_id[i] != 13) return 2;
    for (unsigned i = 0; i < 8; ++i) if (parent->span_id[i] != 29) return 2;
    return recording_dependency_message(NULL, msg, &out, parent, host);
}

uint32_t lifecycle_dependency_construct(AbiStr cfg, AbiComponent **out,
                                      const AbiTraceParentV1 *parent, const AbiRecordingHostV1 *host) {
    (void)cfg;
    (void)out;
    return lifecycle_record(parent, host) == 0 ? 77 : 2;
}
uint32_t lifecycle_dependency_drain(void *self, const AbiTraceParentV1 *parent, const AbiRecordingHostV1 *host) {
    (void)self;
    return lifecycle_record(parent, host) == 0 ? 76 : 2;
}
uint32_t lifecycle_dependency_destroy(AbiComponent *component, const AbiTraceParentV1 *parent, const AbiRecordingHostV1 *host) {
    uint32_t code = lifecycle_record(parent, host);
    free(component);
    return code == 0 ? 78 : code;
}
const AbiLifecycleV1 PARROT_ABI_LIFECYCLE_V1 = {
    lifecycle_dependency_construct, lifecycle_dependency_drain, lifecycle_dependency_destroy
};

uint32_t messaging_dependency_message(void *self, AbiMsg msg, AbiReplyBuf *out,
                                     const AbiTraceParentV1 *parent, const AbiRecordingHostV1 *recording,
                                     const AbiMessagingHostV1 *messaging) {
    static const uint8_t marker[] = "foreign messaging";
    static const uint8_t destination[] = "/user/target";
    if (recording_dependency_message(self, msg, out, parent, recording) != 0) return 2;
    if (messaging != NULL) {
        AbiStr path = { destination, sizeof(destination) - 1 };
        if (messaging->tell(messaging->context, path, msg, 17) != 0) return 2;
    }
    if (out->cap < sizeof(marker) - 1) return 3;
    memcpy(out->buf, marker, sizeof(marker) - 1);
    out->written = sizeof(marker) - 1;
    return 0;
}
const AbiMessagingV1 PARROT_ABI_MESSAGING_V1 = { messaging_dependency_message };
uint32_t services_dependency_message(void *self, AbiMsg msg, AbiReplyBuf *out,
                                    const AbiTraceParentV1 *parent, const AbiRecordingHostV1 *recording,
                                    const AbiMessagingHostV1 *messaging, const AbiServicesHostV1 *services) {
    static const uint8_t marker[] = "foreign services";
    static const uint8_t key[] = "native/business";
    if (messaging_dependency_message(self, msg, out, parent, recording, messaging) != 0) return 2;
    if (services != NULL) {
        AbiStr service_key = { key, sizeof(key) - 1 };
        uint64_t handle = 0;
        uint32_t kind = 0;
        if (services->reg(services->context, service_key, 17) != 0) return 2;
        if (services->subscribe(services->context, service_key, 17, &handle) != 0 || handle != 321) return 2;
        if (services->next(services->context, handle, 17, out, &kind) != 0 || kind != 1) return 2;
        if (services->close(services->context, handle) != 0) return 2;
        if (services->dereg(services->context, service_key, 17) != 0) return 2;
    }
    if (out->cap < sizeof(marker) - 1) return 3;
    memcpy(out->buf, marker, sizeof(marker) - 1);
    out->written = sizeof(marker) - 1;
    return 0;
}
const AbiServicesV1 PARROT_ABI_SERVICES_V1 = { services_dependency_message };
#else
typedef struct {
    uint32_t (*handle_msg)(void *, AbiMsg, AbiReplyBuf *);
    uint32_t (*on_drain)(void *);
} AbiVt;
struct AbiComponent { const AbiVt *vt; void *self; };
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
extern uint32_t recording_dependency_message(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
const AbiRecordingV1 PARROT_ABI_RECORDING_V1 = { recording_dependency_message };
extern uint32_t messaging_dependency_message(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *, const AbiMessagingHostV1 *);
const AbiMessagingV1 PARROT_ABI_MESSAGING_V1 = { messaging_dependency_message };
extern uint32_t services_dependency_message(void *, AbiMsg, AbiReplyBuf *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *, const AbiMessagingHostV1 *, const AbiServicesHostV1 *);
const AbiServicesV1 PARROT_ABI_SERVICES_V1 = { services_dependency_message };
extern uint32_t lifecycle_dependency_construct(AbiStr, AbiComponent **, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
extern uint32_t lifecycle_dependency_drain(void *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
extern uint32_t lifecycle_dependency_destroy(AbiComponent *, const AbiTraceParentV1 *, const AbiRecordingHostV1 *);
const AbiLifecycleV1 PARROT_ABI_LIFECYCLE_V1 = {
    lifecycle_dependency_construct, lifecycle_dependency_drain, lifecycle_dependency_destroy
};
#endif
#endif
