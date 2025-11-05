#ifndef PARROT_TEST_VECTOR_CONTRACTS_H
#define PARROT_TEST_VECTOR_CONTRACTS_H

#include "test_shared_vectors.h"
#include <functional>
#include <cstdio>

namespace parrot_test {
inline void rejected(const std::function<void()> &operation) {
  bool failed = false;
  try { operation(); } catch (const std::exception &) { failed = true; }
  require(failed, "invalid fixture was accepted");
}

inline void contracts() {
  const auto base = Json::parse(R"({"format":"parrot-wire-1.0","frozen":true,"vectors":[{"name":"data","frame_type":"0x11","correlation_id":1,"path":"/x","type_key":"k","payload_hex":"00ff","bytes_hex":"00"}]})");
  unsigned passed = 0;
  auto test = [&passed](const char *name, const std::function<void()> &run) {
    run(); ++passed; std::printf("CPP_FIXTURE_OK %s\n", name);
  };
  test("standard whitespace and escaping", [&] {
    auto doc = base;
    doc["vectors"][0]["path"] = std::string("\xef\xbb\xbf\xe9\x9b\xaa\"\\\n{path}");
    require(parse(doc.dump(3))[0].path == doc["vectors"][0]["path"], "JSON string escapes changed");
  });
  test("unsigned header maxima", [&] {
    auto doc = base;
    auto &row = doc["vectors"][0];
    row["correlation_id"] = UINT64_MAX; row["flags"] = UINT16_MAX;
    row["hop_count"] = 128; row["hop_limit"] = 255; row["sequence"] = UINT32_MAX;
    const auto value = parse(doc.dump())[0];
    require(value.cid == UINT64_MAX && value.flags == UINT16_MAX && value.hop_count == 128 &&
            value.hop_limit == 255 && value.sequence == UINT32_MAX, "integer precision changed");
  });
  test("wrong format and frozen type", [&] {
    auto doc = base; doc["format"] = "other"; rejected([&] { parse(doc.dump()); });
    for (const auto &value : {Json(false), Json(1), Json("true"), Json(nullptr)}) {
      doc = base; doc["frozen"] = value; rejected([&] { parse(doc.dump()); });
    }
  });
  test("empty and malformed rows", [&] {
    for (const auto &value : {Json::array(), Json::object(), Json(nullptr), Json("rows"), Json::array({nullptr})}) {
      auto doc = base; doc["vectors"] = value; rejected([&] { parse(doc.dump()); });
    }
  });
  test("duplicate object keys", [&] {
    const auto source = base.dump();
    rejected([&] { parse("{\"format\":\"parrot-wire-1.0\"," + source.substr(1)); });
    rejected([&] { parse("{\"format\":\"parrot-wire-1.0\",\"frozen\":true,\"vectors\":[{\"name\":\"a\",\"name\":\"b\"}]}"); });
  });
  test("duplicate and empty names", [&] {
    auto doc = base; doc["vectors"].push_back(doc["vectors"][0]); rejected([&] { parse(doc.dump()); });
    doc = base; doc["vectors"][0]["name"] = ""; rejected([&] { parse(doc.dump()); });
  });
  test("malformed trailing and nonstandard JSON", [&] {
    for (const auto &value : {std::string("{"), std::string("[]"), std::string("null"),
          base.dump() + "{}", std::string("{\"n\":NaN}"), std::string("{\"n\":Infinity}")})
      rejected([&] { parse(value); });
  });
  test("required fields and string types", [&] {
    for (const auto *key : {"name", "frame_type", "correlation_id", "path", "type_key", "payload_hex", "bytes_hex"}) {
      auto doc = base; doc["vectors"][0].erase(key); rejected([&] { parse(doc.dump()); });
    }
    for (const auto *key : {"name", "path", "type_key", "payload_hex", "bytes_hex"}) {
      auto doc = base; doc["vectors"][0][key] = 7; rejected([&] { parse(doc.dump()); });
    }
  });
  test("numeric types negatives and overflow", [&] {
    for (const auto *key : {"correlation_id", "flags", "hop_count", "hop_limit", "sequence"}) {
      for (const auto &value : {Json(-1), Json(true), Json("1"), Json(1.0), Json(18446744073709551616.0)}) {
        auto doc = base; doc["vectors"][0][key] = value; rejected([&] { parse(doc.dump()); });
      }
    }
    for (const auto &pair : std::vector<std::pair<std::string, uint64_t>>{
         {"flags", 65536}, {"hop_count", 256}, {"hop_limit", 256}, {"sequence", uint64_t(UINT32_MAX) + 1}}) {
      auto doc = base; doc["vectors"][0][pair.first] = pair.second; rejected([&] { parse(doc.dump()); });
    }
    auto source = base.dump(); const auto at = source.find("\"correlation_id\":1");
    require(at != std::string::npos, "numeric fixture anchor missing");
    source.replace(at, std::string("\"correlation_id\":1").size(), "\"correlation_id\":18446744073709551616");
    rejected([&] { parse(source); });
  });
  test("strict hex and exact frame type", [&] {
    for (const auto *key : {"bytes_hex", "payload_hex"}) {
      for (const auto *value : {"0", "gg", "00 01", "00\n01"}) {
        auto doc = base; doc["vectors"][0][key] = value; rejected([&] { parse(doc.dump()); });
      }
    }
    for (const auto *value : {"0x+1", "0x 1", "0x-0", "0X10", "0x1", "0x100", "0xgg"}) {
      auto doc = base; doc["vectors"][0]["frame_type"] = value; rejected([&] { parse(doc.dump()); });
    }
  });
  test("NUL and malformed Unicode identities", [&] {
    for (const auto *key : {"name", "path", "type_key"}) {
      auto doc = base; doc["vectors"][0][key] = std::string("a\0b", 3); rejected([&] { parse(doc.dump()); });
    }
    for (const auto *escape : {"\\ud800", "\\udfff"}) {
      auto source = base.dump(); const auto at = source.find("\"path\":\"/x\"");
      source.replace(at, std::string("\"path\":\"/x\"").size(), std::string("\"path\":\"") + escape + "\"");
      rejected([&] { parse(source); });
    }
    auto source = base.dump(); const auto at = source.find("/x"); source.replace(at, 2, std::string(1, char(0xff)));
    rejected([&] { parse(source); });
  });
  test("additional rows preserve all data", [&] {
    auto doc = base; auto row = doc["vectors"][0]; row["name"] = "second"; row["correlation_id"] = 99;
    doc["vectors"].push_back(row); const auto values = parse(doc.dump());
    require(values.size() == 2 && values.back().name == "second" && values.back().cid == 99, "appended row lost");
  });
  test("explicit invalid root cannot fall back", [&] {
    rejected([&] { resolve(std::string(), fs::current_path()); });
    rejected([&] { resolve(std::string("missing-fixture-repository"), fs::current_path()); });
  });
  require(passed == 13, "fixture contracts incomplete");
  std::printf("CPP_FIXTURE_RESULT tests=%u failures=0\n", passed);
}
} // namespace parrot_test
#endif
