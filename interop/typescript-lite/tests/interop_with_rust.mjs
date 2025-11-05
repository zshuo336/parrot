/** Actual native TCP against the Rust test server, using a freshly built client. */
import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';
import path from 'node:path';
const moduleUrl = process.argv[3] ? pathToFileURL(path.join(process.argv[3],'lite.js')).href : new URL('../dist/lite.js',import.meta.url).href;
const {ParrotLite} = await import(moduleUrl);
const port = process.argv[2] ?? '9871';
const node = await ParrotLite.connect({url:`tcp://127.0.0.1:${port}`,nodeId:'ts-lite-1'});
try {
  console.log('Wire ACK negotiated protobuf');
  for(const bytes of [new Uint8Array(),new Uint8Array([1,2,3]),new Uint8Array(128*1024).fill(0x5a)])
    assert.deepEqual(await node.ask('/user/echo','bin:u:Echo',bytes),bytes);
  await Promise.all(Array.from({length:32},async(_,i)=>{
    const bytes=new Uint8Array(8192).fill(i);
    assert.deepEqual(await node.ask('/user/echo','bin:u:Echo',bytes),bytes);
  }));
  await assert.rejects(node.ask('/user/missing','bin:u:Echo',new Uint8Array()),/remote err 1:/);
} finally {node.close();}
console.log('LITE-INTEROP PASS');
