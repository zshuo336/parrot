import test from 'node:test';
import assert from 'node:assert/strict';
import { setTimeout as delay } from 'node:timers/promises';
import { ParrotLite, FrameDecoder, FT, buildFrame, handshakeBody, handshakeAckBody, parseTlv, withReplyToPrefix } from '../dist/index.js';
const empty = new Uint8Array();
const turn = () => new Promise(resolve => setImmediate(resolve));
function tlv(tag, bytes) { return Buffer.concat([Buffer.from([tag, bytes.length & 255, bytes.length >> 8]), bytes]); }
function u32(n) { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; }
// Independent protocol fixture: tag 8 is DIRECT_ADDR; tag 9 is CHOSEN_CODEC.
function ackBody(limit = 1<<20) { return Buffer.concat([tlv(1,Buffer.from('peer')),tlv(4,u32(2)),tlv(5,u32(limit)),tlv(6,Buffer.from([0])),tlv(7,Buffer.from([8])),tlv(9,Buffer.from('pb'))]); }
const ack = body => buildFrame(2,1n,'','',body ?? ackBody());
function socket() {
  return { readyState:0, bufferedAmount:0, sent:[], onopen:null,onclose:null,onerror:null,onmessage:null,
    send(bytes) { this.sent.push(bytes.slice()); this.onSend?.(decode(bytes)); },
    close() { this.readyState=3; this.onclose?.(); },
    open() { this.readyState=1; this.onopen?.(); },
    deliver(bytes) { this.onmessage?.({data:Uint8Array.from(bytes).buffer}); },
  };
}
function decode(bytes) { const d=new FrameDecoder();d.feed(bytes);return d.nextFrame(); }
async function until(predicate, ms=2000) { const end=Date.now()+ms; while(!predicate()) { if(Date.now()>end) throw new Error('fixture event missing'); await delay(2); } }
function fixture(options={}) {
  const sockets=[];
  const connecting=ParrotLite.connect({url:'ws://fixture',nodeId:'client',connectTimeoutMs:100,reconnectDelayMs:10,
    ...options,wsFactory:()=>{ const ws=socket();sockets.push(ws);queueMicrotask(()=>ws.open());return ws; }});
  // Rejections are observed immediately even before the test awaits the result.
  connecting.catch(()=>{});
  return {sockets,connecting};
}
async function ready(options={}) { const f=fixture(options);await until(()=>f.sockets[0]?.sent.length);f.sockets[0].deliver(ack());return {...f,node:await f.connecting}; }

test('connect remains pending until a complete valid Wire ACK', async () => {
  const f=fixture();let node,settled=false;f.connecting.then(n=>{node=n;settled=true;},()=>{settled=true;});
  try { await until(()=>f.sockets[0]?.sent.length);await turn();assert.equal(settled,false);f.sockets[0].deliver(ack());node=await f.connecting; }
  finally { node?.close();f.sockets[0]?.close(); }
});
test('empty, truncated and duplicate codec ACKs reject establishment', async () => {
  for(const body of [empty,Buffer.concat([ackBody(),Buffer.from([1])]),Buffer.concat([ackBody(),tlv(9,Buffer.from('pb'))])]) {
    const f=fixture();let node;f.connecting.then(n=>{node=n;},()=>{});
    try { await until(()=>f.sockets[0]?.sent.length);f.sockets[0].deliver(ack(body));await assert.rejects(f.connecting); }
    finally {node?.close();f.sockets[0]?.close();}
  }
});
test('handshake codec uses protocol tag 9, never direct-address tag 8',()=>{
  const fields=new Map(parseTlv(handshakeAckBody('client')));
  assert.equal(new TextDecoder().decode(fields.get(9)),'pb');assert.equal(fields.has(8),false);
});
test('truncated TLV and oversized node identity cannot be silently encoded',()=>{
  assert.throws(()=>parseTlv(Buffer.from([1,3,0,65])));
  assert.throws(()=>handshakeBody('x'.repeat(65536)));
});
test('synchronous transport reply cannot beat pending registration',async()=>{
  const {node,sockets}=await ready();try {
    sockets[0].onSend=f=>{if(f.ft===FT.ASK)sockets[0].deliver(buildFrame(FT.REPLY,f.cid,'','',Buffer.from([99])));};
    assert.deepEqual(Array.from(await node.ask('/x','pb:k',empty,60)),[99]);
  } finally {node.close();}
});
test('connection loss promptly rejects pending without waiting for ASK timer',async()=>{
  const {node,sockets}=await ready();try {
    const result=node.ask('/x','pb:k',empty,5000).then(()=> 'success',()=> 'rejected');
    sockets[0].close();
    assert.equal(await Promise.race([result,delay(50,'still pending')]),'rejected');
  } finally {node.close();}
});
test('malformed ASK prefix never executes the actor',async()=>{
  const {node,sockets}=await ready();let calls=0;
  try {node.spawn('x',{onAsk:async()=>{++calls;return empty;}});
    sockets[0].deliver(buildFrame(FT.ASK,9n,'/user/x','pb:k',Buffer.from([255,255,255,255])));
    await turn();assert.equal(calls,0);assert.equal(sockets[0].readyState,3);
  } finally {node.close();}
});
test('unsupported payload flags never execute the actor',async()=>{
  const {node,sockets}=await ready();let calls=0;
  try {node.spawn('x',{onTell:()=>{++calls;}});
    sockets[0].deliver(buildFrame(FT.TELL,9n,'/user/x','pb:k',empty,1));
    await turn();assert.equal(calls,0);assert.equal(sockets[0].readyState,3);
  } finally {node.close();}
});
test('async reply from retired generation never enters replacement socket',async()=>{
  const {node,sockets}=await ready();let release;
  try {node.spawn('x',{onAsk:()=>new Promise(resolve=>{release=resolve;})});
    sockets[0].deliver(buildFrame(FT.ASK,9n,'/user/x','pb:k',withReplyToPrefix('/return',empty)));
    await until(()=>release);sockets[0].close();await until(()=>sockets.length===2&&sockets[1].sent.length);
    sockets[1].deliver(ack());await turn();release(Buffer.from([7]));await turn();
    assert.equal(sockets[1].sent.map(decode).filter(f=>f.ft===FT.REPLY||f.ft===FT.REPLY_ERR).length,0);
  } finally {release?.(empty);node.close();}
});
test('reconnect discards old partial decoder state',async()=>{
  const {node,sockets}=await ready();try {
    sockets[0].deliver(Buffer.from([60,0,0]));sockets[0].close();await until(()=>sockets.length===2&&sockets[1].sent.length);
    sockets[1].deliver(ack());await turn();
    sockets[1].onSend=f=>{if(f.ft===FT.ASK)sockets[1].deliver(buildFrame(FT.REPLY,f.cid,'','',Buffer.from([99])));};
    assert.deepEqual(Array.from(await node.ask('/x','pb:k',empty,60)),[99]);
  } finally {node.close();}
});

test('handshake timeout retires initial socket and leaves no retry loop',async()=>{
  const f=fixture({connectTimeoutMs:25});await assert.rejects(f.connecting,/timeout/);
  assert.equal(f.sockets[0].readyState,3);await delay(25);assert.equal(f.sockets.length,1);
});
test('TLS credential requests and unsupported transports never dial plaintext',async()=>{
  for(const options of [{tls:{}},{url:'quic://fixture:80'},{nodeId:'bad\0id'},{connectTimeoutMs:0}]) {
    let dials=0;await assert.rejects(ParrotLite.connect({url:'ws://fixture',nodeId:'client',...options,wsFactory:()=>{++dials;return socket();}}));
    assert.equal(dials,0);
  }
});
test('negotiated outbound and advertised inbound limits are enforced',async()=>{
  const f=fixture();await until(()=>f.sockets[0]?.sent.length);f.sockets[0].deliver(ack(ackBody(64)));const node=await f.connecting;
  try {const before=f.sockets[0].sent.length;assert.throws(()=>node.tell('/x','pb:k',new Uint8Array(100)),/large|limit/);
    assert.equal(f.sockets[0].sent.length,before);node.tell('/x','pb:k',empty);
    f.sockets[0].deliver(u32((1<<20)+1));assert.equal(f.sockets[0].readyState,3);
  } finally {node.close();}
});
test('pending count and byte admission return capacity after replies and timeout',async()=>{
  for(const options of [{maxPending:1},{maxPendingBytes:80}]) {
    const {node,sockets}=await ready(options);try {
      const first=node.ask('/x','pb:k',empty,1000);first.catch(()=>{});
      await assert.rejects(node.ask('/x','pb:k',empty,1000),/limit/);
      const sent=sockets[0].sent.map(decode).filter(f=>f.ft===FT.ASK);assert.equal(sent.length,1);
      sockets[0].deliver(buildFrame(FT.REPLY,sent[0].cid,'','',Buffer.from([99])));assert.deepEqual(Array.from(await first),[99]);
      await assert.rejects(node.ask('/x','pb:k',empty,5),/timeout/);
      sockets[0].onSend=f=>{if(f.ft===FT.ASK)sockets[0].deliver(buildFrame(FT.REPLY,f.cid,'','',empty));};
      assert.equal((await node.ask('/x','pb:k',empty)).length,0);
    } finally {node.close();}
  }
});
test('buffer, actor and registration limits reject before transport publication',async()=>{
  const {node,sockets}=await ready({maxBufferedBytes:2000,maxActors:1,maxRegistrations:1});try {
    node.spawn('x',{});assert.throws(()=>node.spawn('y',{}),/limit/);node.spawn('x',{});
    await node.register('one','/x');await assert.rejects(node.register('two','/y'),/limit/);
    sockets[0].bufferedAmount=1990;const before=sockets[0].sent.length;
    await assert.rejects(node.ask('/x','pb:k',empty),/buffer/);assert.equal(sockets[0].sent.length,before);
    sockets[0].bufferedAmount=0;node.tell('/x','pb:k',empty);
  } finally {node.close();}
});
test('retired handlers retain admission until actual settlement and receive abort signal',async()=>{
  const {node,sockets}=await ready({maxConcurrentHandlers:1});let release,signal,calls=0;
  try {node.spawn('x',{onAsk:(_key,_payload,ctx)=>{++calls;signal=ctx.signal;return new Promise(r=>{release=r;});}});
    sockets[0].deliver(buildFrame(FT.ASK,90n,'/user/x','pb:k',withReplyToPrefix('/return',empty)));
    await until(()=>release);sockets[0].close();assert.equal(signal.aborted,true);
    await until(()=>sockets.length===2&&sockets[1].sent.length);sockets[1].deliver(ack());
    sockets[1].deliver(buildFrame(FT.ASK,91n,'/user/x','pb:k',withReplyToPrefix('/return',empty)));
    let response=decode(sockets[1].sent.at(-1));assert.equal(response.ft,FT.REPLY_ERR);assert.equal(new DataView(response.payload.buffer).getUint16(0,true),10);assert.equal(calls,1);
    release(empty);await turn();node.spawn('x',{onAsk:async()=>Buffer.from([7])});
    sockets[1].deliver(buildFrame(FT.ASK,92n,'/user/x','pb:k',withReplyToPrefix('/return',empty)));await turn();
    response=decode(sockets[1].sent.at(-1));assert.equal(response.ft,FT.REPLY);assert.equal(response.cid,92n);
  } finally {release?.(empty);node.close();}
});
test('close during reconnect admission ignores late open and old callbacks',async()=>{
  const {node,sockets}=await ready();const old=sockets[0];const staleMessage=old.onmessage,staleClose=old.onclose;
  old.close();await until(()=>sockets.length===2&&sockets[1].sent.length);const replacement=sockets[1];
  const lateOpen=replacement.onopen;const before=replacement.sent.length;node.close();
  staleMessage({data:ack().buffer});staleClose();lateOpen();await delay(25);
  assert.equal(sockets.length,2);assert.equal(replacement.sent.length,before);assert.equal(replacement.readyState,3);
});
test('async TELL rejection is owned and closes only its original connection',async()=>{
  const {node,sockets}=await ready();let reject;
  try {node.spawn('x',{onTell:()=>new Promise((_,r)=>{reject=r;})});
    sockets[0].deliver(buildFrame(FT.TELL,1n,'/user/x','pb:k',empty));await until(()=>reject);
    sockets[0].close();await until(()=>sockets.length===2&&sockets[1].sent.length);sockets[1].deliver(ack());
    reject(new Error('late tell failure'));await turn();assert.equal(sockets[1].readyState,1);node.tell('/x','pb:k',empty);
  } finally {node.close();}
});
test('routing aliases cannot redirect another node or unrelated path to local actor',async()=>{
  const {node,sockets}=await ready();let calls=0;try {
    node.spawn('x',{onAsk:async()=>{++calls;return empty;}});
    for(const [path,code] of [['parrot://other/user/x',7],['/unrelated/user/x',1]]) {
      sockets[0].deliver(buildFrame(FT.ASK,1n,path,'pb:k',withReplyToPrefix('/r',empty)));
      const reply=decode(sockets[0].sent.at(-1));assert.equal(reply.ft,FT.REPLY_ERR);assert.equal(new DataView(reply.payload.buffer).getUint16(0,true),code);
    }
    assert.equal(calls,0);
    sockets[0].deliver(buildFrame(FT.ASK,2n,'parrot://client/user/x','pb:k',withReplyToPrefix('/r',empty)));await turn();assert.equal(calls,1);
  } finally {node.close();}
});
test('encoding validates numbers, UTF-16 identities and retains sequence',()=>{
  for(const call of [()=>buildFrame(FT.TELL,-1n,'','',empty),()=>buildFrame(FT.TELL,1n,'\ud800','',empty),()=>buildFrame(FT.TELL,1n,'','',empty,65536),()=>buildFrame(FT.TELL,1n,'','',empty,0,8,8)]) assert.throws(call);
  assert.equal(decode(buildFrame(FT.TELL,1n,'','',empty,0,2,8,0x12345678)).sequence,0x12345678);
});

import net from 'node:net';
async function tcpPeer(run, testBody) {
  const sockets=new Set(),errors=[];
  const server=net.createServer(sock=>{sockets.add(sock);sock.on('close',()=>sockets.delete(sock));sock.on('error',()=>{});const decoder=new FrameDecoder();
    sock.on('data',data=>{try {decoder.feed(data);for(let f=decoder.nextFrame();f;f=decoder.nextFrame())run(sock,f);} catch(error){errors.push(error);sock.destroy();}});
  });
  await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
  try {await testBody(`tcp://127.0.0.1:${server.address().port}`);assert.deepEqual(errors,[]);}
  finally {for(const sock of sockets)sock.destroy();await new Promise(resolve=>server.close(resolve));}
}
test('real Node TCP performs Wire handshake and bidirectional ASK and heartbeat', {timeout:5000},async()=>{
  let inboundReply=false,heartbeat=false;
  await tcpPeer((sock,f)=>{
    if(f.ft===FT.HANDSHAKE)sock.write(ack());
    else if(f.ft===FT.ASK){sock.write(buildFrame(FT.REPLY,f.cid,'','',Buffer.from([1,2,3])));sock.write(buildFrame(FT.HEARTBEAT,77n,'','',empty));sock.write(buildFrame(FT.ASK,78n,'/user/local','pb:k',withReplyToPrefix('/r',Buffer.from([7]))));}
    else if(f.ft===FT.REPLY){assert.equal(f.cid,78n);assert.deepEqual(Array.from(f.payload),[7]);inboundReply=true;}
    else if(f.ft===FT.HEARTBEAT_ACK){assert.equal(f.cid,77n);heartbeat=true;}
  },async url=>{const node=await ParrotLite.connect({url,nodeId:'client'});try {node.spawn('local',{onAsk:async(_key,payload)=>payload});assert.deepEqual(Array.from(await node.ask('/x','pb:k',empty)),[1,2,3]);await until(()=>inboundReply&&heartbeat);}finally{node.close();}});
});
test('real TCP silent or malformed handshakes reject and retire socket', {timeout:5000},async()=>{
  for(const malformed of [false,true])await tcpPeer((sock,f)=>{if(f.ft===FT.HANDSHAKE&&malformed)sock.write(ack(empty));},async url=>{await assert.rejects(ParrotLite.connect({url,nodeId:'client',connectTimeoutMs:30}),malformed?/handshake/:/timeout/);});
});
test('real TCP reconnect restores registration and fails old ASK without replay', {timeout:5000},async()=>{
  let generations=0,registered=0,asks=0;const ids=new Map();
  await tcpPeer((sock,f)=>{
    if(f.ft===FT.HANDSHAKE){ids.set(sock,++generations);sock.write(ack());}
    else if(f.ft===FT.TELL){assert.equal(f.typeKey,'$receptionist.register:actor');++registered;}
    else if(f.ft===FT.ASK){++asks;if(ids.get(sock)===1)sock.destroy();else sock.write(buildFrame(FT.REPLY,f.cid,'','',Buffer.from([99])));}
  },async url=>{const node=await ParrotLite.connect({url,nodeId:'client',reconnectDelayMs:5});try {await node.register('actor','/x');await assert.rejects(node.ask('/x','pb:k',empty),/lost|transport/);await until(()=>registered===2);assert.deepEqual(Array.from(await node.ask('/x','pb:k',empty)),[99]);assert.equal(generations,2);assert.equal(asks,2);}finally{node.close();}});
});

test('strict ACK verifies all required fields, identities, lengths and codec intersection',async()=>{
  const standard=parseTlv(ackBody());
  const changed=(tag,value)=>Buffer.concat(standard.map(([t,b])=>tlv(t,t===tag?value:b)));
  const cases=[changed(1,Buffer.from([255])),changed(1,Buffer.from('a\0b')),changed(4,u32(1)),changed(4,Buffer.from([2])),changed(5,u32(27)),changed(6,Buffer.from([4])),changed(7,Buffer.from([0])),changed(9,Buffer.from('bin')),
    Buffer.concat(standard.filter(([t])=>t!==9).map(([t,b])=>tlv(t,b))),Buffer.concat([ackBody(),tlv(8,Buffer.from([255]))]),Buffer.concat([ackBody(),tlv(1,Buffer.from('duplicate'))])];
  for(const body of cases){const f=fixture();await until(()=>f.sockets[0]?.sent.length);f.sockets[0].deliver(ack(body));await assert.rejects(f.connecting);assert.equal(f.sockets[0].readyState,3);}
});
test('fragmented ACK remains pending until complete and unknown TLVs stay compatible',async()=>{
  const f=fixture();let settled=false,node;f.connecting.then(n=>{node=n;settled=true;},()=>{settled=true;});
  try {await until(()=>f.sockets[0]?.sent.length);const bytes=ack(Buffer.concat([ackBody(),tlv(100,Buffer.from([1,2,3]))]));
    for(let i=0;i<bytes.length-1;i++)f.sockets[0].deliver(bytes.subarray(i,i+1));await turn();assert.equal(settled,false);
    f.sockets[0].deliver(bytes.subarray(bytes.length-1));node=await f.connecting;
  }finally{node?.close();f.sockets[0]?.close();}
});
test('retired onmessage cannot resolve replacement pending with the same wire cid',async()=>{
  const {node,sockets}=await ready();try {const stale=sockets[0].onmessage;sockets[0].close();await until(()=>sockets.length===2&&sockets[1].sent.length);sockets[1].deliver(ack());
    let settled=false;const pending=node.ask('/x','pb:k',empty);pending.then(()=>{settled=true;},()=>{});const cid=decode(sockets[1].sent.at(-1)).cid;
    stale({data:buildFrame(FT.REPLY,cid,'','',Buffer.from([8])).buffer});await turn();assert.equal(settled,false);
    sockets[1].deliver(buildFrame(FT.REPLY,cid,'','',Buffer.from([9])));assert.deepEqual(Array.from(await pending),[9]);
  }finally{node.close();}
});
test('synchronous callback close prevents remaining coalesced events from dispatching',async()=>{
  const {node,sockets}=await ready();let calls=0;node.spawn('x',{onTell:()=>{++calls;node.close();}});
  const bytes=buildFrame(FT.TELL,1n,'/user/x','pb:k',empty);sockets[0].deliver(Buffer.concat([bytes,bytes]));await turn();assert.equal(calls,1);assert.equal(sockets[0].readyState,3);
});
test('send exception rejects and releases pending exactly once before reconnect',async()=>{
  const {node,sockets}=await ready({maxPending:1});try {
    sockets[0].send=()=>{throw new Error('forced write failure');};await assert.rejects(node.ask('/x','pb:k',empty),/forced write/);
    await until(()=>sockets.length===2&&sockets[1].sent.length);sockets[1].deliver(ack());
    sockets[1].onSend=f=>{if(f.ft===FT.ASK)sockets[1].deliver(buildFrame(FT.REPLY,f.cid,'','',empty));};assert.equal((await node.ask('/x','pb:k',empty)).length,0);
  }finally{node.close();}
});
test('malformed error envelopes retire connection and wake every pending request',async()=>{
  const {node,sockets}=await ready();try {
    const p1=node.ask('/x','pb:k',empty).catch(e=>e),p2=node.ask('/y','pb:k',empty).catch(e=>e);const cid=decode(sockets[0].sent.at(-1)).cid;
    sockets[0].deliver(buildFrame(FT.REPLY_ERR,cid,'','',Buffer.from([1,0,1,0])));
    assert.match((await p1).message,/invalid error/);assert.match((await p2).message,/invalid error/);assert.equal(sockets[0].readyState,3);
  }finally{node.close();}
});

import http from 'node:http';
import {createHash} from 'node:crypto';
test('native WebSocket receives fragmented Wire ACK and binary echo (Node 18 requires adapter)', {timeout:5000},async()=>{
  if(typeof WebSocket==='undefined') {
    await assert.rejects(ParrotLite.connect({url:'ws://127.0.0.1:1',nodeId:'client'}),/WebSocket unavailable/);return;
  }
  const sockets=new Set(),errors=[];
  const server=http.createServer((_req,res)=>{res.writeHead(404);res.end();});
  function packet(opcode,bytes) {
    const header=bytes.length<126?Buffer.from([0x80|opcode,bytes.length]):Buffer.from([0x80|opcode,126,bytes.length>>8,bytes.length&255]);
    return Buffer.concat([header,bytes]);
  }
  server.on('upgrade',(req,sock,head)=>{
    sockets.add(sock);sock.on('close',()=>sockets.delete(sock));sock.on('error',()=>{});
    const accept=createHash('sha1').update(req.headers['sec-websocket-key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
    sock.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
    let buffered=Buffer.from(head);const wire=new FrameDecoder();
    const feed=bytes=>{try {
      buffered=Buffer.concat([buffered,bytes]);
      while(buffered.length>=2) {
        const opcode=buffered[0]&15;assert.ok(buffered[0]&128);assert.ok(buffered[1]&128);
        let length=buffered[1]&127,offset=2;
        if(length===126){if(buffered.length<4)return;length=buffered.readUInt16BE(2);offset=4;}
        else if(length===127){if(buffered.length<10)return;const n=buffered.readBigUInt64BE(2);assert.ok(n<=1048576n);length=Number(n);offset=10;}
        if(buffered.length<offset+4+length)return;
        const mask=buffered.subarray(offset,offset+4),payload=Buffer.from(buffered.subarray(offset+4,offset+4+length));
        for(let i=0;i<payload.length;i++)payload[i]^=mask[i%4];buffered=buffered.subarray(offset+4+length);
        if(opcode===8){sock.end(packet(8,payload));return;}
        if(opcode===9){sock.write(packet(10,payload));continue;}
        assert.equal(opcode,2);wire.feed(payload);
        for(let f=wire.nextFrame();f;f=wire.nextFrame()) {
          if(f.ft===FT.HANDSHAKE){const bytes=ack();sock.write(packet(2,bytes.subarray(0,7)));sock.write(packet(2,bytes.subarray(7)));}
          else {assert.equal(f.ft,FT.ASK);sock.write(packet(2,buildFrame(FT.REPLY,f.cid,'','',Buffer.from([99]))));}
        }
      }
    }catch(error){errors.push(error);sock.destroy();}};
    sock.on('data',feed);if(head.length)feed(Buffer.alloc(0));
  });
  await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
  try {const node=await ParrotLite.connect({url:`ws://127.0.0.1:${server.address().port}`,nodeId:'client'});
    try {assert.deepEqual(Array.from(await node.ask('/x','pb:k',empty)),[99]);}finally{node.close();}
    await until(()=>sockets.size===0);assert.deepEqual(errors,[]);
  }finally{for(const sock of sockets)sock.destroy();await new Promise(resolve=>server.close(resolve));}
});

test('late reply cannot beat deadline when timer dispatch is delayed by adapter code',async()=>{
  const {node,sockets}=await ready();try {
    sockets[0].onSend=f=>{if(f.ft===FT.ASK){const start=performance.now();while(performance.now()-start<12){}sockets[0].deliver(buildFrame(FT.REPLY,f.cid,'','',empty));}};
    await assert.rejects(node.ask('/x','pb:k',empty,5),/timeout/);
  }finally{node.close();}
});


test('opaque adapter and handler exceptions cannot escape owned promise boundaries',async()=>{
  await assert.rejects(ParrotLite.connect({url:'ws://fixture',nodeId:'client',wsFactory:()=>{throw Object.create(null);}}),/unprintable failure/);
  const {node,sockets}=await ready();try {
    node.spawn('x',{onAsk:async()=>{throw Object.create(null);}});
    sockets[0].deliver(buildFrame(FT.ASK,1n,'/user/x','pb:k',withReplyToPrefix('/r',empty)));await turn();
    assert.equal(decode(sockets[0].sent.at(-1)).ft,FT.REPLY_ERR);assert.equal(sockets[0].readyState,1);
    sockets[0].send=()=>{throw Object.create(null);};await assert.rejects(node.ask('/x','pb:k',empty),/unprintable failure/);assert.equal(sockets[0].readyState,3);
  }finally{node.close();}
});

function codecAck(caps, chosen) {
  const fields = [tlv(1,Buffer.from('peer')),tlv(4,u32(caps)),tlv(5,u32(1<<20)),tlv(6,Buffer.from([0]))];
  if (chosen !== undefined) fields.push(tlv(9,Buffer.from(chosen)));
  return Buffer.concat(fields);
}
test('explicit bin stack advertises only bin and survives actual socket replacement',async()=>{
  const f=fixture({codec:'bin'});let node;
  try {
    for(let generation=0;generation<2;generation++) {
      await until(()=>f.sockets[generation]?.sent.length);
      const ws=f.sockets[generation];
      const fields=new Map(parseTlv(decode(ws.sent[0]).payload));
      assert.equal(Buffer.from(fields.get(4)).readUInt32LE(),1);
      ws.deliver(ack(codecAck(3,'bin')));
      if(generation===0)node=await f.connecting;
      ws.onSend=frame=>{if(frame.ft===FT.ASK)ws.deliver(buildFrame(FT.REPLY,frame.cid,'','',Buffer.from([255])));};
      assert.deepEqual(Array.from(await node.ask('/echo','bin:custom',empty)),[255]);
      if(generation===0)ws.close();
    }
  }finally{node?.close();for(const ws of f.sockets)ws.close();}
});
test('bin and pb ACKs cannot choose an unadvertised or unshared stack',async()=>{
  for(const [codec,caps,chosen] of [['bin',2,'pb'],['bin',3,'pb'],['bin',2,'bin'],['bin',3,'json'],['pb',3,'bin'],['pb',3,undefined]]) {
    const f=fixture({codec});
    try {await until(()=>f.sockets[0]?.sent.length);f.sockets[0].deliver(ack(codecAck(caps,chosen)));await assert.rejects(f.connecting,/codec/);}
    finally{for(const ws of f.sockets)ws.close();}
  }
});
test('legacy omitted ACK choice is bin and requires explicit bin admission',async()=>{
  const f=fixture({codec:'bin'});let node;
  try {await until(()=>f.sockets[0]?.sent.length);f.sockets[0].deliver(ack(codecAck(1,undefined)));node=await f.connecting;}
  finally{node?.close();for(const ws of f.sockets)ws.close();}
});
test('invalid runtime codec options are rejected before any dial',async()=>{
  for(const codec of ['json','',null,0,{},Object.create(null)]) {
    let dials=0;
    await assert.rejects(ParrotLite.connect({url:'ws://fixture',nodeId:'client',codec,wsFactory:()=>{++dials;return socket();}}),/codec/);
    assert.equal(dials,0);
  }
});
