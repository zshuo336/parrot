/** Building the browser bundle must never overwrite the package's ESM module. */
import assert from 'node:assert/strict';
import net from 'node:net';
import * as esm from '../dist/index.js';
import * as bundle from '../dist/parrot-lite.bundle.js';
assert.deepEqual(Object.keys(bundle).sort(),Object.keys(esm).sort());
const sockets=new Set(),errors=[];
const server=net.createServer(sock=>{
  sockets.add(sock);sock.on('close',()=>sockets.delete(sock));sock.on('error',()=>{});
  const decoder=new esm.FrameDecoder();
  sock.on('data',bytes=>{try {
    decoder.feed(bytes);for(let frame=decoder.nextFrame();frame;frame=decoder.nextFrame()) {
      if(frame.ft===esm.FT.HANDSHAKE) {
        const caps = new Map(esm.parseTlv(frame.payload)).get(4);
        const value = new DataView(caps.buffer,caps.byteOffset,caps.byteLength).getUint32(0,true);
        assert.ok(value===1 || value===2);
        sock.write(esm.buildFrame(esm.FT.HANDSHAKE_ACK,frame.cid,'','',esm.handshakeAckBody('peer',value===1?'bin':'pb')));
      }
      else if(frame.ft===esm.FT.ASK)sock.write(esm.buildFrame(esm.FT.REPLY,frame.cid,'','',esm.splitReplyTo(frame.payload).payload));
    }
  }catch(error){errors.push(error);sock.destroy();}});
});
await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(0,'127.0.0.1',resolve);});
try {
  for(const api of [esm,bundle]) for(const codec of ['pb','bin']) {
    const node=await api.ParrotLite.connect({url:`tcp://127.0.0.1:${server.address().port}`,nodeId:'package-gate',codec});
    try {const data=new Uint8Array([1,2,3,255]);assert.deepEqual(await node.ask('/echo',`${codec}:k`,data),data);}finally{node.close();}
  }
  assert.deepEqual(errors,[]);
}finally{for(const sock of sockets)sock.destroy();await new Promise(resolve=>server.close(resolve));}
console.log('ESM and browser bundle: exports and real TCP echo PASS');
