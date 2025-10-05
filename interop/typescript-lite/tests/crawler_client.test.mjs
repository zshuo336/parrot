// crawler-lab 用户终端（TS Lite）：五运行时场景的"用户 Web 访问"边缘端。
//
// 场景角色：浏览器/终端用户查询客户端——TS Lite（Node tcp:// 直连，
// 浏览器回落 WS 兜底——同一 bundle 两栖）。
//
// 数据流：TS → JVM(akka search) → Rust hub → (ray/erl 索引链) ——
// 本脚本执行其中"用户"一跳：连 JVM 网关查 top-k，验证索引服务在线。
//
// 注意：需活网关（run-lab.sh 已起）。独立跑（无 LAB_JVM 环境）自动 skip。
import test from "node:test";
import assert from "node:assert/strict";
import { ParrotLite } from "../dist/lite.js";

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

test("crawler-lab: user search via akka gateway", { timeout: 20000 }, async (t) => {
  // 网关探活：连不上（独立跑）→ skip（lab 场景由 run-lab.sh 保证网关在线）
  try {
    const probe = await ParrotLite.connect({ url: `tcp://${JVM}`, nodeId: "ts-probe" });
    probe.close();
  } catch {
    t.skip(`lab gateway not running at ${JVM}（独立跑跳过——由 run-lab.sh 驱动）`);
    return;
  }
  const c = await ParrotLite.connect({ url: `tcp://${JVM}`, nodeId: "ts-user-1" });
  const reply = await c.ask("/jvm/user/search", "bin:crawl/Search", encQuery(["parrot", "actor"]), 8000);
  const text = Buffer.from(reply).toString("utf8");
  assert.ok(text.startsWith("["), `expect json array, got: ${text}`);
  const arr = JSON.parse(text);
  assert.ok(arr.length > 0, "search results non-empty");
  assert.ok(typeof arr[0].doc === "number" && typeof arr[0].score === "number");
  console.log("[ts-user] search parrot+actor →", text);
  c.close();
});
