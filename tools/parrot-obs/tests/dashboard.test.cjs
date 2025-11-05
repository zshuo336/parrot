const {test} = require('node:test');
const assert = require('node:assert/strict');
const {render, counter, uptime} = require('../src/dashboard.js');
const base = {runtime:'fixture', asks_rx:'10', uptime_start_ms:'100000', ts_ms:'200000', component_states:[],available:'131071',epoch:'owner-a',process_kind:'erlang_processes',sample_clock:'clock-a',sampled_at_ns:'0'};

const measured={...base,available:'131071',epoch:'owner-a',process_kind:'erlang_processes',sample_clock:'clock-a',sampled_at_ns:'0'};
test('measured zero RSS remains known zero',()=>{
  const view=render({node:{...measured,memory_rss:'0'}},1,new Map());
  assert.ok(view.cards.includes('<b>0 B</b>'));
});
test('unavailable nonzero fields and component data remain hidden',()=>{
  const view=render({node:{...measured,available:'0',asks_rx:'31337',memory_rss:'31337',component_states:[{path:'hidden-component'}]}},1,new Map());
  assert.ok(!view.rows.includes('31337'));
  assert.ok(!view.cards.includes('hidden-component'));
});
test('epoch change resets history even when wall-clock startup and runtime match',()=>{
  const state=new Map();render({node:measured},1,state);
  const view=render({node:{...measured,epoch:'owner-b',asks_rx:'20',sampled_at_ns:'5000000000'}},5001,state);
  assert.equal(state.get('node').history.length,1);
  assert.ok(view.rows.includes('<td class="num">n/a</td></tr>'));
});
test('rates use each server receipt clock rather than browser response timing',()=>{
  const state=new Map();render({node:measured},1000,state);
  const view=render({node:{...measured,asks_rx:'20',sampled_at_ns:'5000000000'}},2000,state);
  assert.ok(view.rows.includes('<td class="num">2.00</td></tr>'));
});

test('remote metadata and errors cannot introduce elements or attributes', () => {
  const hostile='<img src=x onerror="document.body.dataset.injected=1">&\'';
  const m={...base,runtime:hostile,connections:hostile,asks_rx:hostile,component_states:[{path:hostile,state:hostile,version:hostile}]};
  const data=Object.fromEntries([[hostile,m],['__proto__',{error:hostile}]]);
  const view=render(data,1,new Map());
  for (const html of [view.rows,view.cards]) {assert.ok(!html.includes('<img'));assert.ok(html.includes('&lt;img'));assert.ok(html.includes('&quot;'));assert.ok(html.includes('&amp;'));}
});
test('unsigned64 counters are preserved and malformed numeric values unavailable', () => {
  const max='18446744073709551615';const view=render({node:{...base,asks_rx:max}},1,new Map());
  assert.ok(view.rows.includes(max));assert.equal(counter(max),18446744073709551615n);
  for (const value of ['18446744073709551616','-1','1e5',' 1',Number.MAX_SAFE_INTEGER+1,null]) assert.equal(counter(value),null);
});
test('uptime uses wire timestamps, including unavailable and reversed clocks', () => {
  assert.equal(uptime(base),'0h01m40s');assert.equal(uptime({...base,uptime_start_ms:'0'}),'0h03m20s');assert.equal(uptime({...base,ts_ms:'1'}),'n/a');
  assert.equal(uptime({...base,available:'0'}),'n/a');
});
test('rates require a continuous process and positive monotonic sample interval', () => {
  const state=new Map();render({node:base},1000,state);
  assert.ok(render({node:{...base,asks_rx:'20',sampled_at_ns:'5000000000'}},6000,state).rows.includes('2.00'));
  assert.ok(render({node:{...base,asks_rx:'1',sampled_at_ns:'6000000000'}},7000,state).rows.includes('<td class="num">n/a</td></tr>'));
  assert.ok(render({node:{...base,asks_rx:'100',epoch:'owner-b',sampled_at_ns:'7000000000'}},8000,state).rows.includes('<td class="num">n/a</td></tr>'));
  render({},9000,state);assert.equal(state.size,0);
  render({node:base},10000,state);render({node:{error:'offline'}},11000,state);assert.equal(state.size,0);
});

test('sparkline history is bounded and resets with the process', () => {
  const state=new Map();
  for(let i=0;i<1000;i++) render({node:{...base,asks_rx:String(i),sampled_at_ns:String(i*100000000)}},i*100,state);
  assert.equal(state.get('node').history.length,61);
  render({node:{...base,epoch:'owner-b',sampled_at_ns:'100001000000'}},100001,state);
  assert.equal(state.get('node').history.length,1);
});

test('collector restart and field disappearance clear rate continuity',()=>{
  const state=new Map();render({node:measured},0,state);
  for (const next of [
    {...measured,sample_clock:'clock-b',asks_rx:'20',sampled_at_ns:'5000000000'},
    {...measured,available:'0',asks_rx:'30',sampled_at_ns:'10000000000'},
    {...measured,asks_rx:'40',sampled_at_ns:'15000000000'},
    {...measured,epoch:'',asks_rx:'50',sampled_at_ns:'20000000000'}
  ]) assert.ok(render({node:next},1,state).rows.includes('<td class="num">n/a</td></tr>'));
});
test('legacy or invalid masks cannot masquerade as measured snapshots',()=>{
  for(const available of [undefined,'131072','-1','wat']) {
    const view=render({node:{...measured,available}},1,new Map());
    assert.ok(view.rows.includes('invalid measured snapshot'));
    assert.equal(view.cards,'');
  }
});
test('process units and independent component counts are visible',()=>{
  const view=render({node:{...measured,processes:'2',components:'1',component_states:[{path:'/a'},{path:'/b'}]}},1,new Map());
  assert.ok(view.cards.includes('2 erlang_processes'));
  assert.ok(view.cards.includes('<span>components</span><b>1</b>'));
});
