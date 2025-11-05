#include "parrot_lite.h"

#include <cstdio>
#include <limits>
#include <stdexcept>
#include <string>

using parrot::lite::Frame;
using parrot::lite::FrameDecoder;

static void require(bool condition, const char *message) {
  if (!condition) throw std::runtime_error(message);
}

static Frame sample(uint32_t bound) {
  Frame frame;
  frame.ft = PL_FT_TELL;
  frame.cid = 0x123456789abcdef0ULL;
  frame.sequence = 0x76543210;
  frame.payload.resize(bound - 28);
  for (size_t i = 0; i < frame.payload.size(); ++i)
    frame.payload[i] = static_cast<uint8_t>(i * 31 + 17);
  return frame;
}

static void compare(const Frame &actual, const Frame &expected) {
  require(actual.ft == expected.ft && actual.cid == expected.cid &&
          actual.sequence == expected.sequence && actual.flags == expected.flags &&
          actual.hop_count == expected.hop_count && actual.hop_limit == expected.hop_limit &&
          actual.path == expected.path && actual.type_key == expected.type_key &&
          actual.payload == expected.payload, "decoded frame differs");
}

static void invalid_constructor() {
  for (uint32_t bound : {0u, 1u, 27u, PL_MAX_FRAME_LEN + 1,
                         std::numeric_limits<uint32_t>::max() - 3,
                         std::numeric_limits<uint32_t>::max()}) {
    bool rejected = false;
    try { FrameDecoder decoder(bound); }
    catch (const std::invalid_argument &) { rejected = true; }
    require(rejected, "invalid constructor bound was accepted");
  }
}

static void exact_bounds() {
  for (uint32_t bound : {28u, 64u, 1u << 20, PL_MAX_FRAME_LEN}) {
    FrameDecoder decoder(bound);
    auto expected = sample(bound);
    auto encoded = expected.encode();
    require(encoded.size() == size_t(bound) + 4, "fixture size differs");
    Frame actual;
    int error = 123;
    decoder.feed(encoded.data(), 4);
    require(!decoder.next(&actual, &error) && error == 0, "prefix consumed as full frame");
    decoder.feed(encoded.data() + 4, encoded.size() - 5);
    require(!decoder.next(&actual, &error) && error == 0, "short body consumed");
    decoder.feed(encoded.data() + encoded.size() - 1, 1);
    require(decoder.next(&actual, &error) && error == 0, "exact bound rejected");
    compare(actual, expected);
    require(!decoder.next(&actual, &error) && error == 0, "frame decoded twice");
  }
}

static void rejection_and_recovery() {
  for (uint32_t bound : {28u, 64u, PL_MAX_FRAME_LEN}) {
    FrameDecoder decoder(bound);
    const uint8_t hostile[] = {255, 255, 255, 255};
    Frame actual;
    int error = 123;
    decoder.feed(hostile, sizeof(hostile));
    require(!decoder.next(&actual, &error) && error == -4, "hostile prefix not rejected");
    auto expected = sample(28);
    auto encoded = expected.encode();
    decoder.feed(encoded.data(), encoded.size());
    require(decoder.next(&actual, &error) && error == 0, "decoder did not recover");
    compare(actual, expected);

    // Setter must preserve both its limit and buffered bytes on invalid input.
    decoder.feed(encoded.data(), 4);
    for (uint32_t invalid : {0u, 27u, PL_MAX_FRAME_LEN + 1, UINT32_MAX})
      require(!decoder.set_max_frame_len(invalid), "invalid setter bound accepted");
    decoder.feed(encoded.data() + 4, encoded.size() - 4);
    require(decoder.next(&actual, &error) && error == 0, "setter discarded buffered prefix");
    compare(actual, expected);
    if (bound < PL_MAX_FRAME_LEN) {
      auto oversized = sample(bound + 1).encode();
      decoder.feed(oversized.data(), 4);
      require(!decoder.next(&actual, &error) && error == -4, "invalid setter changed ceiling");
    }
  }
}

static void overflow_probe() {
  bool rejected = false;
  try {
    FrameDecoder decoder(UINT32_MAX);
    const uint8_t hostile[] = {255, 255, 255, 255};
    decoder.feed(hostile, sizeof(hostile));
    Frame actual;
    int error = 0;
    (void)decoder.next(&actual, &error);
  } catch (const std::invalid_argument &) { rejected = true; }
  require(rejected, "unsafe constructor survived overflow probe without rejection");
}

int main(int argc, char **argv) {
  try {
    require(argc == 2, "expected scenario name");
    const std::string mode = argv[1];
    if (mode == "invalid") invalid_constructor();
    else if (mode == "bounds") exact_bounds();
    else if (mode == "recovery") rejection_and_recovery();
    else if (mode == "overflow") overflow_probe();
    else throw std::runtime_error("unknown scenario");
    std::printf("DECODER LIMIT PASS %s\n", argv[1]);
    return 0;
  } catch (const std::exception &error) {
    std::fprintf(stderr, "DECODER LIMIT FAILURE: %s\n", error.what());
    return 1;
  }
}
