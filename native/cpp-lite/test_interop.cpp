// C++ 侧互操作入口（Rust test_cpp_interop.rs 调起）。
//
// 用法：
//   ./test_interop <port>   —— 连 Rust echo 节点：握手 + pl_ask 往返 + pl_register
//   ./test_interop 0        —— ABI 冒烟（未连接态错误码语义 + vectors）

#include "parrot_lite.h"

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "assert_t.h" // CHECK 宏（vectors 同款）

int main(int argc, char **argv) {
  const int port = argc > 1 ? atoi(argv[1]) : 0;

  if (port == 0) {
    // ── ABI 冒烟：未连接态语义 ──────────────────────────
    uint8_t *out = nullptr;
    size_t out_len = 0;
    CHECK(pl_ask("/user/x", "k", nullptr, 0, &out, &out_len, 100) == -2,
          "unconnected ask -> -2");
    CHECK(pl_tell("/user/x", "k", nullptr, 0) == -2, "unconnected tell -> -2");
    CHECK(pl_register("k", "/p", nullptr) == -2, "unconnected register -> -2");
    pl_poll(0);  // 幂等无操作
    pl_close();  // 幂等
    pl_close();
    pl_free(nullptr); // 幂等
    CHECK(pl_connect("bogus://x", "n", nullptr) == -1, "bad url -> -1");
    if (g_fail == 0) printf("CABI-SMOKE PASS\n");
    return g_fail == 0 ? 0 : 1;
  }

  // ── 真实互操作：Rust echo 节点 ────────────────────────
  char url[64];
  snprintf(url, sizeof(url), "parrot://127.0.0.1:%d", port);
  CHECK(pl_connect(url, "cpp-lite-1", nullptr) == 0, "connect+handshake");

  // receptionist 注册（TELL $receptionist.register:——不期待回包）
  CHECK(pl_register("edge/sensor", "/user/cpp1", nullptr) == 0, "register");

  // pl_ask 往返（Rust echo 原样回——净载荷）
  const uint8_t req[] = {0xDE, 0xAD, 0xBE, 0xEF};
  uint8_t *out = nullptr;
  size_t out_len = 0;
  const int rc = pl_ask("/user/echo", "bin:u:Echo", req, sizeof(req), &out, &out_len, 5000);
  CHECK(rc == 0, "pl_ask rc");
  if (rc == 0) {
    CHECK(out_len == sizeof(req), "echo len");
    CHECK(out && memcmp(out, req, out_len) == 0, "echo bytes");
    pl_free(out); // 调用方释放（防双 free——§7.4）
  }

  pl_close();

  if (g_fail == 0) printf("CABI-INTEROP PASS\n");
  return g_fail == 0 ? 0 : 1;
}
