import test from 'node:test';
import assert from 'node:assert/strict';
import {setTimeout as delay} from 'node:timers/promises';
import {ParrotLite, FrameDecoder, FT, buildFrame, handshakeAckBody} from '../dist/index.js';
const decode=bytes=>{const d=new FrameDecoder();d.feed(bytes);return d.nextFrame();};
async function until(predicate) { const end=Date.now()+2000; while(!predicate()) {if(Date.now()>end)throw new Error('fixture event missing');await delay(2);} }
test('actual reconnect replays each unique registration once on the new socket',async()=>{
  const sockets=[];
  const node=await ParrotLite.connect({url:'ws://fixture',nodeId:'client',reconnectDelayMs:5,connectTimeoutMs:100,wsFactory:()=>{
    const ws={readyState:0,bufferedAmount:0,sent:[],onopen:null,onclose:null,onmessage:null,onerror:null,
      send(bytes){this.sent.push(decode(bytes));if(decode(bytes).ft===FT.HANDSHAKE)queueMicrotask(()=>this.onmessage?.({data:buildFrame(FT.HANDSHAKE_ACK,1n,'','',handshakeAckBody('peer')).buffer}));},
      close(){this.readyState=3;this.onclose?.();}};
    sockets.push(ws);queueMicrotask(()=>{ws.readyState=1;ws.onopen?.();});return ws;
  }});
  try {
    await node.register('edge/rpa','rpa-main');await node.register('edge/cam','cam-main');await node.register('edge/rpa','rpa-main');
    for(let round=0;round<3;round++) {
      const old=sockets.at(-1);const oldCount=old.sent.length;old.close();
      await until(()=>sockets.length===round+2&&sockets.at(-1).sent.length===3);
      const current=sockets.at(-1);const regs=current.sent.filter(f=>f.typeKey.startsWith('$receptionist.register:'));
      assert.deepEqual(regs.map(f=>[f.typeKey,f.path]),[['$receptionist.register:edge/rpa','rpa-main'],['$receptionist.register:edge/cam','cam-main']]);
      assert.equal(old.sent.length,oldCount);assert.equal(current.sent[0].ft,FT.HANDSHAKE);
    }
    const last=sockets.at(-1);last.onmessage?.({data:buildFrame(FT.HANDSHAKE_ACK,2n,'','',handshakeAckBody('peer')).buffer});
    assert.equal(last.readyState,3,'duplicate ACK retires the current generation, it is not a reconnect');
  } finally {node.close();}
  const count=sockets.length;await delay(20);assert.equal(sockets.length,count,'close cancels retry timer');
});
