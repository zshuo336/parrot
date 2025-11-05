// crawler-lab 用户终端（TS Lite）：五运行时场景的"用户 Web 访问"边缘端。
//
// 场景角色：浏览器/终端用户查询客户端——TS Lite（Node tcp:// 直连，
// 浏览器回落 WS 兜底——同一 bundle 两栖）。
//
// 数据流：TS → JVM(akka search) → Rust hub → (ray/erl 索引链) ——
// 本脚本执行其中"用户"一跳：连 JVM 网关查 top-k，验证索引服务在线。
//
// 通过 npm run test:crawler 显式执行；网关/业务不可用时必须失败。
import test from "node:test";
import assert from "node:assert/strict";
import { ParrotLite } from "../../dist/lite.js";

const JVM = process.env.LAB_JVM ?? "127.0.0.1:19862";

function encQuery(terms) {
  const parts = [Buffer.alloc(4)];
  parts[0].writeUInt32LE(terms.length);
  for (const t of terms) {
    const tb = Buffer.from(t, "utf8");
    const len = Buffer.alloc(4);
    len.writeUInt32LE(tb.length);
    parts.push(len, tb);
  }
  return Buffer.concat(parts);
}

test("crawler-lab: user search via akka gateway", { timeout: 20000 }, async () => {
  const c = await ParrotLite.connect({ url: `tcp://${JVM}`, nodeId: "ts-user-1", codec: "bin" });
  try {
    const reply = await c.ask(process.env.LAB_SEARCH_PATH ?? "/jvm/user/search", "bin:crawl/Search", encQuery(["parrot", "actor"]), 8000);
    const text = Buffer.from(reply).toString("utf8");
    const results = JSON.parse(text);
    assert.ok(Array.isArray(results), `expect json array, got: ${text}`);
    assert.ok(process.env.LAB_EMPTY === "1" ? results.length === 0 : results.length > 0, "search result cardinality");
    if (process.env.LAB_EXPECTED_HITS !== undefined) {
      assert.deepEqual(results, JSON.parse(process.env.LAB_EXPECTED_HITS), "results match the host's independent term-frequency oracle");
    }
    for (const result of results) {
      assert.ok(result && Number.isFinite(result.doc) && Number.isFinite(result.score),
                `invalid search result: ${JSON.stringify(result)}`);
    }
    console.log("[ts-user] search parrot+actor →", text);
  } finally {
    c.close();
  }
});
