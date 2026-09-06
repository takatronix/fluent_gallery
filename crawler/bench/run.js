#!/usr/bin/env node
'use strict';
// 比較ベンチ: 同じサイト・goal・limits で HTTP サービス(fable=8796 / codex=8797)を叩き、結果表を出す
// node bench/run.js --impl fable [--site commons_shiba|all] [--base http://127.0.0.1:8796] [--deliver] [--gallery http://127.0.0.1:8798]
const fs = require('fs');
const path = require('path');
const args = process.argv.slice(2);
const argOf = (k, d) => { const i = args.indexOf(k); return i >= 0 && args[i + 1] ? args[i + 1] : d; };
const impl = argOf('--impl', 'fable');
const base = argOf('--base', impl === 'codex' ? 'http://127.0.0.1:8797' : 'http://127.0.0.1:8796');
const site = argOf('--site', 'all');
const deliver = args.includes('--deliver');
const gallery = argOf('--gallery', 'http://127.0.0.1:8798');
const sites = JSON.parse(fs.readFileSync(path.join(__dirname, 'sites.json'), 'utf8')).filter((s) => site === 'all' || s.name === site);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  const h = await fetch(base + '/health').then((r) => r.json()).catch((e) => ({ ok: false, err: String(e) }));
  if (!h.ok) { console.error(`${base} が応答しません: ${JSON.stringify(h)}`); process.exit(1); }
  console.log(`impl=${h.impl} v${h.version} ${h.browser}  deliver=${deliver}${deliver ? ' → ' + gallery : ''}`);
  const rows = [];
  for (const s of sites) {
    const body = { url: s.url, goal: s.goal, album: s.album, gallery, deliver, judge: true, limits: s.limits };
    const t0 = Date.now();
    const j = await fetch(base + '/jobs', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).then((r) => r.json());
    if (!j.id) { console.error(`${s.name}: ジョブ作成失敗 ${JSON.stringify(j)}`); continue; }
    process.stdout.write(`${s.name} (${j.id}) `);
    let st;
    while (true) {
      await sleep(3000);
      st = await fetch(`${base}/jobs/${j.id}`).then((r) => r.json()).catch(() => null);
      if (!st) continue;
      process.stdout.write('.');
      if (['done', 'stopped', 'error'].includes(st.state)) break;
      if (Date.now() - t0 > (s.limits.max_minutes + 3) * 60 * 1000) { await fetch(`${base}/jobs/${j.id}/stop`, { method: 'POST' }); }
    }
    console.log(` ${st.state}/${st.stop_reason} ${st.elapsed_s}s`);
    let log = '';
    try { log = await fetch(`${base}/jobs/${j.id}/log`).then((r) => r.text()); } catch {}
    const taken = deliver ? st.delivered - st.dup - st.failed : st.saved;
    rows.push({ site: s.name, id: j.id, state: st.state, stop: st.stop_reason, secs: st.elapsed_s, pages: st.pages_visited, seen: st.images_seen, picked: st.images_picked,
      delivered: st.delivered, accepted: st.accepted, rejected: st.rejected, dup: st.dup, failed: st.failed, saved: st.saved, taken,
      pass: st.delivered ? Math.round(100 * st.accepted / Math.max(1, st.delivered - st.dup)) : null,
      mb: Math.round(st.bytes / 1e5) / 10, errors: st.errors, log });
  }
  const out = path.join(__dirname, `results-${impl}-${new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19)}.json`);
  fs.writeFileSync(out, JSON.stringify(rows.map((r) => ({ ...r, log: undefined })), null, 1));
  for (const r of rows) { if (r.log) fs.writeFileSync(out.replace(/\.json$/, `-${r.site}.log.jsonl`), r.log); }
  console.log(`\n| site | 状態 | 秒 | ページ | 見た | 選んだ | 取れた | 渡した | 通過 | 却下 | 重複 | 失敗 | 通過率 | MB | err |`);
  console.log('|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|');
  for (const r of rows) console.log(`| ${r.site} | ${r.state}/${r.stop} | ${r.secs} | ${r.pages} | ${r.seen} | ${r.picked} | ${r.taken} | ${r.delivered} | ${r.accepted} | ${r.rejected} | ${r.dup} | ${r.failed} | ${r.pass == null ? '-' : r.pass + '%'} | ${r.mb} | ${r.errors} |`);
  console.log(`\n結果: ${out}`);
})();
