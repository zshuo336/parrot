'use strict';

const escapeText = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
function counter(value) {
  if (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0) return BigInt(value);
  if (typeof value !== 'string' || !/^(0|[1-9][0-9]{0,19})$/.test(value)) return null;
  const n = BigInt(value);
  return n <= 18446744073709551615n ? n : null;
}
const fields = ['ts_ms','runtime','connections','handshakes_ok','handshakes_failed','asks_rx','tells_rx','replies_tx','reply_errs','bytes_rx','bytes_tx','heartbeats_rx','components','component_states','processes','memory_rss','uptime_start_ms'];
const bits = Object.fromEntries(fields.map((name, index) => [name, 1n << BigInt(index)]));
function mask(m) {
  const value = counter(m?.available);
  return value !== null && value <= 131071n ? value : null;
}
function metric(m, name) {
  const available = mask(m);
  return available !== null && bits[name] !== undefined && (available & bits[name]) !== 0n ? m[name] : undefined;
}
const identity = s => typeof s === 'string' && s.length > 0 && new TextEncoder().encode(s).length <= 128;
const countText = value => counter(value)?.toString() ?? 'n/a';
function bytes(value) {
  const n = counter(value);
  if (n === null) return 'n/a';
  if (n >= 1073741824n) return (Number(n) / 1073741824).toFixed(2) + ' GiB';
  if (n >= 1048576n) return (Number(n) / 1048576).toFixed(1) + ' MiB';
  if (n >= 1024n) return (Number(n) / 1024).toFixed(1) + ' KiB';
  return n.toString() + ' B';
}
function uptime(m) {
  const start = counter(metric(m,'uptime_start_ms')), now = counter(metric(m,'ts_ms'));
  if (start === null || now === null || now < start) return 'n/a';
  const seconds = (now - start) / 1000n;
  return `${seconds / 3600n}h${String(seconds % 3600n / 60n).padStart(2,'0')}m${String(seconds % 60n).padStart(2,'0')}s`;
}
function spark(history) {
  if (history.length < 2) return '';
  const first=history[0], last=history[history.length-1], span=last.at-first.at;
  if (span <= 0n) return '';
  const total=Number(last.asks-first.asks);
  const points=history.map(p => `${(Number(p.at-first.at)/Number(span)*100).toFixed(1)},${(14-Number(p.asks-first.asks)/Math.max(1,total)*14).toFixed(1)}`).join(' ');
  return `<div class="spark"><svg width="100" height="14"><polyline fill="none" stroke="#58a6ff" stroke-width="1.2" points="${points}"/></svg> asks ${(last.asks-first.asks).toString()}/窗</div>`;
}
function render(data, _browserNow, state) {
  let rows = '', cards = '';
  const present = new Set();
  for (const [name, m] of Object.entries(data)) {
    present.add(name);
    if (!m || m.error !== undefined || mask(m) === null) {
      rows += `<tr><td>${escapeText(name)}</td><td colspan="11" class="err">${escapeText(m?.error ?? 'invalid measured snapshot')}</td></tr>`;
      state.delete(name);
      continue;
    }
    const get = key => metric(m, key);
    const asks = counter(get('asks_rx')), at = counter(m.sampled_at_ns), prev = state.get(name);
    const knownIdentity = identity(m.epoch) && identity(m.sample_clock);
    const continuous = knownIdentity && asks !== null && at !== null && prev && prev.asks !== null && prev.at !== null && prev.epoch === m.epoch && prev.clock === m.sample_clock && asks >= prev.asks && at > prev.at;
    const history = continuous ? prev.history.filter(p => at-p.at < 120000000000n).slice(-60) : [];
    if (knownIdentity && asks !== null && at !== null) history.push({at, asks});
    const rate = continuous ? (Number(asks-prev.asks) / (Number(at-prev.at)/1e9)).toFixed(2) : 'n/a';
    const runtime = get('runtime') ?? 'n/a';
    rows += `<tr><td><b>${escapeText(name)}</b></td><td>${escapeText(runtime)}</td><td>${uptime(m)}</td>
      <td class="num">${countText(get('connections'))}</td><td class="num">${countText(get('asks_rx'))}</td><td class="num">${countText(get('tells_rx'))}</td>
      <td class="num">${countText(get('replies_tx'))}</td><td class="num">${countText(get('reply_errs'))}</td>
      <td class="num">${bytes(get('bytes_rx'))}</td><td class="num">${bytes(get('bytes_tx'))}</td><td class="num">${bytes(get('memory_rss'))}</td><td class="num">${rate}</td></tr>`;
    const states = get('component_states');
    const compHtml = Array.isArray(states) ? states.map(c => `<div class="comps"><code>${escapeText(c?.path)}</code>${escapeText(c?.state)} ${escapeText(c?.version)}</div>`).join('') : '<div class="comps">component states: n/a</div>';
    const processCount = counter(get('processes'));
    const processes = processCount !== null && identity(m.process_kind) ? `${processCount} ${escapeText(m.process_kind)}` : 'n/a';
    cards += `<div class="card"><h3>${escapeText(name)} · ${escapeText(runtime)}</h3>
      <div class="kv"><span>uptime</span><b>${uptime(m)}</b></div>
      <div class="kv"><span>handshakes ok/fail</span><b>${countText(get('handshakes_ok'))}/${countText(get('handshakes_failed'))}</b></div>
      <div class="kv"><span>heartbeats_rx</span><b>${countText(get('heartbeats_rx'))}</b></div>
      <div class="kv"><span>processes</span><b>${processes}</b></div>
      <div class="kv"><span>memory_rss</span><b>${bytes(get('memory_rss'))}</b></div>
      <div class="kv"><span>components</span><b>${countText(get('components'))}</b></div>${compHtml}${spark(history)}</div>`;
    state.set(name, {at, asks, epoch:m.epoch, clock:m.sample_clock, history});
  }
  for (const name of state.keys()) if (!present.has(name)) state.delete(name);
  return {rows, cards};
}
if (typeof module !== 'undefined') module.exports = {render, counter, uptime};
if (typeof document !== 'undefined') {
  const state = new Map();
  async function tick() {
    try {
      const response = await fetch('/api/metrics', {cache:'no-store'});
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const data = await response.json();
      const view = render(data, performance.now(), state);
      document.getElementById('rows').innerHTML = view.rows;
      document.getElementById('cards').innerHTML = view.cards;
      document.getElementById('meta').textContent = '每 2s 刷新 · ' + new Date().toLocaleTimeString();
    } catch (error) {
      state.clear();
      document.getElementById('meta').textContent = '断开：' + error;
    }
    setTimeout(tick, 2000);
  }
  tick();
}
