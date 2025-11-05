// golden vectors 断言（DEV_04 §5.3）——最小 assert 集（无 GoogleTest 依赖）。
//
// 断言 docs/vectors/wire1.json 四向量逐字节一致（与 Rust/TS/JVM/py/erl 同表）。
// 跑法：clang++ -std=c++17 -I. test_vectors.cpp parrot_lite.cpp -o t && ./t

#include "parrot_lite.h"
#include "test_vector_contracts.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>
#include <fstream>
#include <sstream>
#include <stdexcept>
#include <limits>

#include "assert_t.h"

using parrot::lite::Frame;
using parrot::lite::FrameDecoder;

using parrot_test::unhex;

#define CHECK_PL(cond, msg)                                                    \
  do {                                                                         \
    if (!(cond)) {                                                             \
      printf("FAIL %s:%d %s\n", __FILE__, __LINE__, msg);                      \
      g_fail++;                                                                \
    }                                                                          \
  } while (0)

static int run() {
  parrot_test::contracts();
  const char *configured = std::getenv("PARROT_REPO");
  const auto fixture = parrot_test::resolve(configured ? std::optional<std::string>(configured) : std::nullopt,
                                           std::filesystem::current_path());
  const auto vectors = parrot_test::load(fixture);
  auto named = [&vectors](const char *name) -> const parrot_test::Vector & {
    for (const auto &vector : vectors) if (vector.name == name) return vector;
    throw std::runtime_error(std::string("missing shared vector: ") + name);
  };
  for (const auto *name : {"ask-basic", "tell-basic", "reply-err-stopped", "heartbeat"}) (void)named(name);
  // Identical bytes/expected classification are exercised by all language stacks.
  std::ifstream corpus(fixture.parent_path() / "wire1-boundaries.tsv");
  CHECK_PL(corpus.is_open(), "shared wire boundary corpus must exist");
  std::string line;
  size_t corpus_count = 0;
  while (std::getline(corpus, line)) {
    if (line.empty() || line[0] == '#') continue;
    std::istringstream row(line);
    std::string name, expected, encoded;
    std::getline(row, name, '\t');
    std::getline(row, expected, '\t');
    std::getline(row, encoded);
    const auto bytes = unhex(encoded.c_str());
    FrameDecoder decoder;
    decoder.feed(bytes.data(), bytes.size());
    Frame frame;
    int error = 0;
    const bool complete = decoder.next(&frame, &error);
    CHECK_PL(complete == (expected == "frame"), name.c_str());
    CHECK_PL((error != 0) == (expected == "reject"), name.c_str());
    if (complete) CHECK_PL(frame.cid == 0x123456789abcdef0ull, name.c_str());
    if (expected == "frame") {
      for (size_t split = 0; split < bytes.size(); ++split) {
        FrameDecoder partial;
        partial.feed(bytes.data(), split);
        CHECK_PL(!partial.next(&frame, &error) && error == 0, "valid split is incomplete");
        partial.feed(bytes.data() + split, bytes.size() - split);
        partial.feed(bytes.data(), bytes.size());
        CHECK_PL(partial.next(&frame, &error) && error == 0, "split resumes");
        CHECK_PL(partial.next(&frame, &error) && error == 0, "tail stays intact");
        CHECK_PL(!partial.next(&frame, &error) && error == 0, "no extra frame");
      }
    }
    ++corpus_count;
  }
  CHECK_PL(corpus_count >= 47, "corpus must not be silently empty or truncated");
  printf("CPP shared wire boundaries: %zu cases\n", corpus_count);
  for (const auto &v : vectors) {
    const auto &bytes = v.bytes;
    const char *name = v.name.c_str();
    Frame expected;
    expected.ft = v.ft; expected.cid = v.cid; expected.flags = v.flags;
    expected.hop_count = v.hop_count; expected.hop_limit = v.hop_limit; expected.sequence = v.sequence;
    expected.path = v.path; expected.type_key = v.key; expected.payload = v.payload;
    CHECK_PL(expected.encode() == bytes, (v.name + ": independent encoding differs").c_str());

    // 1) 解码：字节 → Frame 字段全等
    FrameDecoder dec;
    dec.feed(bytes.data(), bytes.size());
    Frame f;
    int err = 0;
    const bool got = dec.next(&f, &err);
    CHECK_PL(got && err == 0, name);
    if (!got) continue;
    CHECK_PL(f.ft == v.ft, name);
    CHECK_PL(f.cid == v.cid, name);
    CHECK_PL(f.path == v.path, name);
    CHECK_PL(f.type_key == v.key, name);
    CHECK_PL(f.payload == v.payload, name);
    CHECK_PL(f.hop_limit == v.hop_limit && f.hop_count == v.hop_count, name);
    CHECK_PL(f.flags == v.flags && f.sequence == v.sequence, name);

    // 2) 再编码：逐字节回环
    const auto re = f.encode();
    CHECK_PL(re == bytes, name);
    CHECK_PL(!dec.next(&f, &err) && err == 0, (v.name + ": trailing bytes").c_str());
    for (size_t cut = 0; cut < bytes.size(); ++cut) {
      FrameDecoder split;
      split.feed(bytes.data(), cut);
      CHECK_PL(!split.next(&f, &err) && err == 0, (v.name + ": incomplete split").c_str());
      split.feed(bytes.data() + cut, bytes.size() - cut);
      split.feed(bytes.data(), bytes.size());
      CHECK_PL(split.next(&f, &err) && err == 0 && f.encode() == bytes,
               (v.name + ": split changes fields").c_str());
      CHECK_PL(split.next(&f, &err) && err == 0 && f.encode() == bytes,
               (v.name + ": split loses next frame").c_str());
      CHECK_PL(!split.next(&f, &err) && err == 0, (v.name + ": split trailing frame").c_str());
    }
  }

  FrameDecoder pipeline;
  for (const auto &v : vectors) pipeline.feed(v.bytes.data(), v.bytes.size());
  for (const auto &v : vectors) {
    Frame actual; int error = 0;
    CHECK_PL(pipeline.next(&actual, &error) && error == 0 && actual.encode() == v.bytes,
             (v.name + ": pipeline order or fields").c_str());
  }
  { Frame extra; int error = 0;
    CHECK_PL(!pipeline.next(&extra, &error) && error == 0, "pipeline has trailing frame"); }

  // Changing the negotiated bound preserves partial input and rejects prefixes
  // immediately. Invalid API values must not silently replace the valid bound.
  {
    const auto &heartbeat = named("heartbeat").bytes;
    FrameDecoder d; Frame f; int error = 0;
    d.feed(heartbeat.data(), 7);
    CHECK_PL(d.set_max_frame_len(28), "minimum negotiated limit");
    for (uint32_t invalid : {0u, 27u, PL_MAX_FRAME_LEN + 1u, UINT32_MAX})
      CHECK_PL(!d.set_max_frame_len(invalid), "invalid limit rejected");
    CHECK_PL(!d.next(&f, &error) && error == 0, "partial bytes retained");
    d.feed(heartbeat.data() + 7, heartbeat.size() - 7);
    CHECK_PL(d.next(&f, &error) && error == 0 && f.ft == PL_FT_HEARTBEAT, "exact minimum resumes");
    const uint8_t oversized[] = {29, 0, 0, 0};
    d.feed(oversized, sizeof(oversized));
    CHECK_PL(!d.next(&f, &error) && error == -4, "invalid updates preserve active bound");
  }

  // 半包：切断任意点，不消费、可续
  const auto &full = named("ask-basic").bytes;
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

  // Header extensions must survive forwarding; defaults cannot replace them.
  {
    Frame original;
    original.ft = PL_FT_TELL; original.cid = UINT64_MAX;
    original.hop_count = 128; original.hop_limit = 255;
    original.sequence = UINT32_MAX;
    original.path = "/unicode/目录"; original.type_key = "pb:key";
    auto bytes = original.encode(); Frame decoded; FrameDecoder d; int error = 0;
    d.feed(bytes.data(), bytes.size());
    CHECK_PL(d.next(&decoded, &error) && error == 0, "extended header decodes");
    CHECK_PL(decoded.sequence == UINT32_MAX && decoded.hop_count == 128 && decoded.hop_limit == 255,
             "sequence and unsigned hop are preserved");
    CHECK_PL(decoded.encode() == bytes, "extended header exact roundtrip");
    for (int invalid = 0; invalid < 4; ++invalid) {
      Frame bad = original;
      if (invalid == 0) bad.hop_count = bad.hop_limit;
      if (invalid == 1) bad.hop_limit = 0;
      if (invalid == 2) bad.path = std::string(1, char(0xff));
      if (invalid == 3) bad.payload.resize(PL_MAX_FRAME_LEN);
      bool rejected = false;
      try { (void)bad.encode(); } catch (const std::exception &) { rejected = true; }
      CHECK_PL(rejected, "encoder rejects invalid input before copy");
    }
    FrameDecoder empty;
    empty.feed(nullptr, 0);
    CHECK_PL(!empty.next(&decoded, &error) && error == 0, "empty nullptr input is safe");
  }

  if (g_fail == 0) printf("CPP-LITE VECTORS PASS (%zu vectors)\n", vectors.size());
  return g_fail == 0 ? 0 : 1;
}

int main() {
  try { return run(); }
  catch (const std::exception &error) {
    std::fprintf(stderr, "CPP_FIXTURE_ERROR %s\n", error.what());
    return 1;
  }
}
