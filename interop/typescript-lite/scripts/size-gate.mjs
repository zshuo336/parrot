#!/usr/bin/env node
/** E1 bundle: gzip output <50,000 bytes (DEV_03 section 9.3). */
import { gzipSync } from "node:zlib";
import { readFileSync } from "node:fs";

const BUDGET = 50_000;
try {
  const buf = readFileSync("dist/parrot-lite.bundle.js");
  const gz = gzipSync(buf);
  const size = gz.length;
  console.log(`bundle min+gz: ${size} bytes (required < ${BUDGET} bytes)`);
  if (size >= BUDGET) {
    console.error("SIZE GATE FAILED");
    process.exit(1);
  }
  console.log("size gate PASS");
} catch (e) {
  console.error("dist/parrot-lite.bundle.js missing or unreadable; run `npm run bundle` first:", e.message);
  process.exit(2);
}
