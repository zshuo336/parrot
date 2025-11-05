import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { delimiter } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ParrotLite } from '../../dist/index.js';

// Run the real gateway, not an ACK imitation. Missing Java/artifacts are errors.
async function gateway() {
  const target = new URL('../../../jvm/target/', import.meta.url);
  const cp = process.env.PARROT_JVM_CP ?? [
    fileURLToPath(new URL('parrot-protocol-jvm-0.1.0.jar', target)),
    readFileSync(new URL('cp.txt', target), 'utf8').trim(),
  ].join(delimiter);
  const child = spawn('java', ['-cp', cp, 'parrot.protocol.jvm.ParrotGatewayMain', '0', '60']);
  let output = '', count = 0, timer;
  const ended = new Promise(resolve => {
    child.once('error', resolve);
    child.once('close', (code, signal) => resolve({code, signal}));
  });
  const stop = async () => {
    clearTimeout(timer);
    if (child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
    const force = setTimeout(() => child.kill('SIGKILL'), 5000);
    try { await ended; } finally { clearTimeout(force); }
  };
  try {
    const port = await new Promise((resolve, reject) => {
      timer = setTimeout(() => reject(new Error(`JVM startup timeout: ${output}`)), 15000);
      ended.then(() => reject(new Error(`JVM exited before readiness: ${output}`)));
      const collect = bytes => {
        count += bytes.length;
        if (count > 65536) { reject(new Error('JVM startup output limit')); return; }
        output += bytes.toString();
        const match = output.match(/(?:^|\n)PARROT_JVM_PORT=(\d+)\r?\n/);
        if (match && Number(match[1]) > 0 && Number(match[1]) <= 65535) resolve(Number(match[1]));
      };
      child.stdout.on('data', collect);
      child.stderr.on('data', collect);
    });
    clearTimeout(timer);
    return {url: `tcp://127.0.0.1:${port}`, stop};
  } catch (error) { await stop(); throw error; }
}

test('explicit bin stack exchanges exact payloads with the real JVM gateway', {timeout: 45000}, async () => {
  const peer = await gateway();
  let client;
  try {
    await assert.rejects(ParrotLite.connect({url:peer.url, nodeId:'ts-pb', connectTimeoutMs:3000}));
    client = await ParrotLite.connect({url:peer.url, nodeId:'ts-bin', codec:'bin', connectTimeoutMs:5000});
    for (const payload of [Buffer.alloc(0), Buffer.from([0,255,7]), Buffer.alloc(128*1024, 97)]) {
      assert.deepEqual(Buffer.from(await client.ask('/jvm/user/echo','bin:parrot.interop.Echo#v1',payload)),payload);
    }
    const replies = await Promise.all(Array.from({length:32}, async (_, i) => {
      const payload = Buffer.from(`request-${i}`);
      assert.deepEqual(Buffer.from(await client.ask('/jvm/user/echo','bin:parrot.interop.Echo#v1',payload)),payload);
      return i;
    }));
    assert.equal(replies.length,32);
    await assert.rejects(client.ask('/jvm/user/missing','bin:parrot.interop.Echo#v1',Buffer.alloc(0)),/^Error: remote err 1: missing$/);
  } finally { client?.close(); await peer.stop(); }
});


test('production JVM CPU matches Rust bincode boundaries and rejects malformed payloads', {timeout:45000}, async()=>{
  const peer=await gateway();let client;
  try {
    client=await ParrotLite.connect({url:peer.url,nodeId:'ts-cpu-contract',codec:'bin'});
    const rows=readFileSync(new URL('../../../../docs/vectors/jvm-codecs.tsv',import.meta.url),'utf8')
      .trimEnd().split('\n').filter(line=>line.startsWith('cpu\t')).map(line=>line.split('\t'));
    assert.equal(rows.length,32);
    await assert.rejects(client.ask('/jvm/user/cpu','pb:wrong',Buffer.from([41])),/^Error: remote err 6:/);
    for(const [,name,input,expected] of [...rows,['cpu','after-errors','29','2a']]) {
      const reply=client.ask('/jvm/user/cpu','bin:parrot.interop.Cpu#v1',Buffer.from(input,'hex'));
      if(expected==='ERR')await assert.rejects(reply,/^Error: remote err 5:/,name);
      else assert.deepEqual(Buffer.from(await reply),Buffer.from(expected,'hex'),name);
    }
  }finally{client?.close();await peer.stop();}
});
