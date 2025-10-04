// golden vectors 断言（DEV_04 §5.3）——最小 assert 集（无 GoogleTest 依赖）。
//
// 断言 docs/vectors/wire1.json 四向量逐字节一致（与 Rust/TS/JVM/py/erl 同表）。
// 跑法：clang++ -std=c++17 -I. test_vectors.cpp parrot_lite.cpp -o t && ./t

#include "parrot_lite.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "assert_t.h"

using parrot::lite::Frame;
using parrot::lite::FrameDecoder;

static std::vector<uint8_t> unhex(const char *s) {
  std::vector<uint8_t> out;
  const size_t n = strlen(s);
  out.reserve(n / 2);
  for (size_t i = 0; i + 1 < n; i += 2) {
    const char hex[3] = {s[i], s[i + 1], 0};
    out.push_back(static_cast<uint8_t>(strtol(hex, nullptr, 16)));
  }
  return out;
}

#define CHECK_PL(cond, msg)                                                    \
  do {                                                                         \
    if (!(cond)) {                                                             \
      printf("FAIL %s:%d %s\n", __FILE__, __LINE__, msg);                      \
      g_fail++;                                                                \
    }                                                                          \
  } while (0)

struct Vec {
  const char *name;
  const char *bytes_hex;
  uint8_t ft;
  uint64_t cid;
  const char *path;
  const char *key;
  const char *payload_hex;
};

// docs/vectors/wire1.json（frozen——只增不改）
static const Vec kVectors[] = {
    {"ask-basic",
     "2b0000000110000001000000000000000008000000000000020000002f780800000062696e3a743a3a4d00000000ab",
     PL_FT_ASK, 1, "/x", "bin:t::M", "00000000ab"},
    {"tell-basic",
     "280000000113000000000000000000000008000000000000020000002f790800000062696e3a743a3a540102",
     PL_FT_TELL, 0, "/y", "bin:t::T", "0102"},
    {"reply-err-stopped",
     "2d00000001120000070000000000000000080000000000000000000000000000030000006163746f722073746f70706564",
     PL_FT_REPLY_ERR, 7, "", "", "030000006163746f722073746f70706564"},
    {"heartbeat",
     "1c00000001030000000000000000000000080000000000000000000000000000",
     PL_FT_HEARTBEAT, 0, "", "", ""},
};

int main() {
  for (const auto &v : kVectors) {
    const auto bytes = unhex(v.bytes_hex);

    // 1) 解码：字节 → Frame 字段全等
    FrameDecoder dec;
    dec.feed(bytes.data(), bytes.size());
    Frame f;
    int err = 0;
    const bool got = dec.next(&f, &err);
    CHECK_PL(got && err == 0, v.name);
    if (!got) continue;
    CHECK_PL(f.ft == v.ft, v.name);
    CHECK_PL(f.cid == v.cid, v.name);
    CHECK_PL(f.path == v.path, v.name);
    CHECK_PL(f.type_key == v.key, v.name);
    CHECK_PL(f.payload == unhex(v.payload_hex), v.name);
    CHECK_PL(f.hop_limit == PL_DEFAULT_HOP_LIMIT, v.name);
    CHECK_PL(f.flags == 0, v.name);

    // 2) 再编码：逐字节回环
    const auto re = f.encode();
    CHECK_PL(re == bytes, v.name);
  }

  // 半包：切断任意点，不消费、可续
  const auto full = unhex(kVectors[0].bytes_hex);
  for (size_t cut = 1; cut < full.size(); cut++) {
    FrameDecoder d;
    d.feed(full.data(), cut);
    Frame f;
    int err = 0;
    CHECK_PL(!d.next(&f, &err) && err == 0, "partial must not consume");
    d.feed(full.data() + cut, full.size() - cut);
    CHECK_PL(d.next(&f, &err) && err == 0, "resume after partial");
  }

  // u64 cid 原生（uint64_t LE——§7 实现注意 5）
  {
    Frame f;
    f.ft = PL_FT_ASK;
    f.cid = 0x123456789ABCDEF0ull;
    f.path = "/p";
    f.type_key = "k";
    const auto b = f.encode();
    CHECK_PL(b.size() == 4 + 28 + 2 + 1, "len");
    uint64_t cid = 0;
    for (int i = 0; i < 8; i++) cid |= static_cast<uint64_t>(b[8 + i]) << (8 * i);
    CHECK_PL(cid == 0x123456789ABCDEF0ull, "u64 cid LE");
  }

  if (g_fail == 0) printf("CPP-LITE VECTORS PASS (%zu vectors)\n", sizeof(kVectors) / sizeof(kVectors[0]));
  return g_fail == 0 ? 0 : 1;
}
