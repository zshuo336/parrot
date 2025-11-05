// Real sockets and the public C ABI drive every scenario. A resolver seam keeps
// malformed-authority controls from querying public DNS and records admission.
#include <netdb.h>
#include <atomic>
#include <cstdio>
#include <cstring>
static std::atomic<unsigned> resolver_calls{0};
static int address_resolve(const char *, const char *, const addrinfo *, addrinfo **);
#define getaddrinfo address_resolve
#include "parrot_lite.cpp"
#undef getaddrinfo

static int address_resolve(const char *host, const char *service,
                           const addrinfo *hints, addrinfo **out) {
  ++resolver_calls;
  in6_addr numeric{};
  if (std::strcmp(host, "localhost") == 0 || ::inet_pton(AF_INET, host, &numeric) == 1 ||
      ::inet_pton(AF_INET6, host, &numeric) == 1)
    return ::getaddrinfo(host, service, hints, out);
  *out = nullptr;
  return EAI_NONAME;
}
static int failure(const char *what, int rc = 0) {
  std::fprintf(stderr, "FAIL address %s rc=%d resolver_calls=%u\n", what, rc, resolver_calls.load());
  return 1;
}
int main(int argc, char **argv) {
  if (argc != 3 && argc != 4) return 2;
  const bool preserve = std::strcmp(argv[1], "preserve") == 0;
  if (preserve != (argc == 4)) return 2;
  int connected = pl_connect(argv[2], "address-contract", nullptr);
  if (connected) return failure("valid connection", connected);
  if (preserve) {
    const unsigned before = resolver_calls.load();
    const int invalid = pl_connect(argv[3], "rejected-address", nullptr);
    const unsigned after = resolver_calls.load();
    uint8_t *out = reinterpret_cast<uint8_t *>(1); size_t length = 99;
    const int status = pl_ask("/still-connected", "pb:k", nullptr, 0, &out, &length, 1000);
    const bool recovered = status == 0 && out && length == 1 && out[0] == 99;
    pl_free(out); pl_close();
    if (invalid != -1 || before != after)
      return failure("invalid authority must fail before resolver/connection mutation", invalid);
    if (!recovered) return failure("invalid authority retired existing connection", status);
    return 0;
  }
  for (uint8_t round = 0; round < 2; ++round) {
    if (round) {
      pl_close();
      connected = pl_connect(argv[2], "address-contract-reconnected", nullptr);
      if (connected) return failure("explicit reconnect", connected);
    }
    const int registration = pl_register("address-service", "/user/address", "{}");
    if (registration) { pl_close(); return failure("registration", registration); }
    uint8_t *out = nullptr; size_t length = 0;
    const int status = pl_ask("/address", "pb:k", &round, 1, &out, &length, 1000);
    const bool correct = status == 0 && out && length == 1 && out[0] == static_cast<uint8_t>(42 + round);
    pl_free(out);
    if (!correct) { pl_close(); return failure("business reply", status); }
  }
  pl_close();
  return 0;
}
