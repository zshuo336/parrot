#ifndef PARROT_TEST_SHARED_VECTORS_H
#define PARROT_TEST_SHARED_VECTORS_H

#include "test_vendor/nlohmann/json.hpp"
#include <filesystem>
#include <fstream>
#include <optional>
#include <set>
#include <stdexcept>
#include <string>
#include <vector>

namespace parrot_test {
using Json = nlohmann::json;
namespace fs = std::filesystem;

inline void require(bool condition, const std::string &message) {
  if (!condition) throw std::runtime_error(message);
}

inline std::vector<uint8_t> unhex(const std::string &value) {
  require(value.size() % 2 == 0 && value.size() <= 2u * (16u * 1024u * 1024u + 4u), "invalid fixture hex length");
  auto digit = [](char c) -> unsigned {
    if (c >= '0' && c <= '9') return unsigned(c - '0');
    if (c >= 'a' && c <= 'f') return unsigned(c - 'a') + 10;
    if (c >= 'A' && c <= 'F') return unsigned(c - 'A') + 10;
    throw std::runtime_error("invalid fixture hex digit");
  };
  std::vector<uint8_t> result;
  result.reserve(value.size() / 2);
  for (size_t i = 0; i < value.size(); i += 2)
    result.push_back(uint8_t((digit(value[i]) << 4) | digit(value[i + 1])));
  return result;
}

inline std::string text(const Json &node, const char *key, bool nonempty = false) {
  const auto &item = node.at(key);
  require(item.is_string(), std::string("invalid fixture string: ") + key);
  const auto value = item.get<std::string>();
  require((!nonempty || !value.empty()) && value.find('\0') == std::string::npos,
          std::string("invalid fixture identity: ") + key);
  return value;
}

inline uint64_t uint(const Json &node, const char *key, uint64_t maximum) {
  const auto &value = node.at(key);
  require(value.is_number_integer() && (value.is_number_unsigned() || value.get<int64_t>() >= 0),
          std::string("invalid fixture integer: ") + key);
  const auto result = value.get<uint64_t>();
  require(result <= maximum, std::string("fixture integer out of range: ") + key);
  return result;
}

inline uint64_t optional_uint(const Json &row, const char *key, uint64_t maximum, uint64_t fallback) {
  return row.contains(key) ? uint(row, key, maximum) : fallback;
}

struct Vector {
  std::string name, path, key;
  std::vector<uint8_t> bytes, payload;
  uint8_t ft, hop_count, hop_limit;
  uint16_t flags;
  uint64_t cid;
  uint32_t sequence;
};

inline std::vector<Vector> parse(const std::string &input) {
  require(input.size() <= 64u * 1024u * 1024u, "fixture exceeds input budget");
  std::vector<std::set<std::string>> keys;
  auto callback = [&keys](int, Json::parse_event_t event, Json &value) {
    if (event == Json::parse_event_t::object_start) keys.emplace_back();
    else if (event == Json::parse_event_t::key) {
      require(!keys.empty() && keys.back().insert(value.get<std::string>()).second, "duplicate fixture JSON key");
    } else if (event == Json::parse_event_t::object_end) keys.pop_back();
    return true;
  };
  const auto document = Json::parse(input, callback);
  require(document.is_object() && text(document, "format") == "parrot-wire-1.0", "wrong fixture format");
  const auto &frozen = document.at("frozen");
  require(frozen.is_boolean() && frozen.get<bool>(), "fixture must be frozen");
  const auto &rows = document.at("vectors");
  require(rows.is_array() && !rows.empty() && rows.size() <= 4096, "invalid fixture vector list");
  std::set<std::string> names;
  std::vector<Vector> result;
  for (const auto &row : rows) {
    require(row.is_object(), "fixture row must be an object");
    Vector vector;
    vector.name = text(row, "name", true);
    require(names.insert(vector.name).second, "duplicate fixture vector name");
    const auto frame_type = text(row, "frame_type");
    require(frame_type.size() == 4 && frame_type.substr(0, 2) == "0x", "invalid fixture frame type");
    vector.ft = unhex(frame_type.substr(2)).at(0);
    vector.cid = uint(row, "correlation_id", UINT64_MAX);
    vector.flags = uint16_t(optional_uint(row, "flags", UINT16_MAX, 0));
    vector.hop_count = uint8_t(optional_uint(row, "hop_count", UINT8_MAX, 0));
    vector.hop_limit = uint8_t(optional_uint(row, "hop_limit", UINT8_MAX, 8));
    vector.sequence = uint32_t(optional_uint(row, "sequence", UINT32_MAX, 0));
    vector.path = text(row, "path");
    vector.key = text(row, "type_key");
    vector.bytes = unhex(text(row, "bytes_hex"));
    vector.payload = unhex(text(row, "payload_hex"));
    result.push_back(std::move(vector));
  }
  return result;
}

inline fs::path resolve(const std::optional<std::string> &root, const fs::path &cwd) {
  const fs::path relative("docs/vectors/wire1.json");
  if (root) {
    require(!root->empty(), "empty configured fixture repository");
    const auto path = fs::absolute(cwd / *root / relative).lexically_normal();
    require(fs::is_regular_file(path), "configured fixture is missing: " + path.string());
    return path;
  }
  for (const auto &base : {cwd, cwd / "../..", cwd / "../../.."}) {
    const auto path = fs::absolute(base / relative).lexically_normal();
    if (fs::is_regular_file(path)) return path;
  }
  throw std::runtime_error("shared fixture not found from " + cwd.string());
}

inline std::vector<Vector> load(const fs::path &path) {
  require(fs::file_size(path) <= 64u * 1024u * 1024u, "fixture exceeds input budget");
  std::ifstream stream(path, std::ios::binary);
  require(bool(stream), "cannot open shared fixture");
  std::string input((std::istreambuf_iterator<char>(stream)), std::istreambuf_iterator<char>());
  require(!stream.bad(), "cannot read shared fixture");
  return parse(input);
}
} // namespace parrot_test
#endif
