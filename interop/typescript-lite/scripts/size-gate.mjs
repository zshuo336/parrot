#!/usr/bin/env node
/** E1 bundle 门禁：dist <50KB min+gz、零原生依赖（DEV_03 §2.3）。 */
import { gzipSync } from "node:zlib";
import { readFileSync, statSync } from "node:fs";

const BUDGET = 50 * 1024;
try {
  const buf = readFileSync("dist/lite.js");
  const gz = gzipSync(buf);
  const size = gz.length;
  console.log(`bundle min+gz: ${(size / 1024).toFixed(1)}KB (budget ${BUDGET / 1024}KB)`);
  if (size > BUDGET) {
    console.error("SIZE GATE FAILED");
    process.exit(1);
  }
  console.log("size gate PASS");
} catch (e) {
  console.error("dist/lite.js missing — run `npm run bundle` first:", e.message);
  process.exit(2);
}
