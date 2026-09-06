#!/usr/bin/env node
'use strict';
// fluent_crawler(fable 版)HTTP サービス。127.0.0.1 だけで待ち受け、ジョブは 1 本ずつ順に走らせる(人が 1 枚のタブで見るのと同じ)
const http = require('http');
const path = require('path');
const fs = require('fs');
const { Browser } = require('./lib/browser');
const { Job } = require('./lib/job');
const pkg = require('./package.json');

const IMPL = 'fable';
const args = process.argv.slice(2);
const argOf = (k, d) => { const i = args.indexOf(k); return i >= 0 && args[i + 1] ? args[i + 1] : d; };
const PORT = +(argOf('--port', process.env.PORT || 8796));
const JOBS_DIR = argOf('--jobs', path.join(__dirname, 'jobs'));
const HEADED = args.includes('--headed');
fs.mkdirSync(JOBS_DIR, { recursive: true });

const jobs = new Map(); // id → Job(新しい順は配列で管理)
const orderIds = [];
const queue = [];
let running = null;
let browser = null;

async function pump() {
  if (running || !queue.length) return;
  const job = queue.shift();
  running = job;
  try {
    if (!browser) browser = new Browser({ headless: !HEADED });
    job.browser = browser;
    await job.run();
  } catch (e) {
    job.state = 'error'; job.stopReason = 'error:' + String(e.message || e).slice(0, 120); job.writeStatus();
  } finally {
    running = null;
    setImmediate(pump);
  }
}

function send(res, code, body, type = 'application/json') {
  const b = type === 'application/json' ? JSON.stringify(body) : body;
  res.writeHead(code, { 'Content-Type': type + '; charset=utf-8', 'Content-Length': Buffer.byteLength(b) });
  res.end(b);
}
function readJson(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on('data', (c) => { chunks.push(c); if (chunks.reduce((n, x) => n + x.length, 0) > 1e6) { reject(new Error('body too large')); req.destroy(); } });
    req.on('end', () => { try { resolve(chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : {}); } catch (e) { reject(e); } });
    req.on('error', reject);
  });
}

const server = http.createServer(async (req, res) => {
  const u = new URL(req.url, 'http://127.0.0.1');
  const p = u.pathname.replace(/\/$/, '') || '/';
  try {
    if (req.method === 'GET' && p === '/health') {
      return send(res, 200, { ok: true, impl: IMPL, version: pkg.version, browser: `chromium ${browser ? browser.version : '(未起動)'}`, running: running ? running.id : null, queued: queue.length });
    }
    if (req.method === 'POST' && p === '/jobs') {
      const body = await readJson(req);
      if (!body.url || !/^https?:\/\//i.test(String(body.url))) return send(res, 400, { ok: false, detail: 'url (http/https) が必要' });
      if (!body.album) return send(res, 400, { ok: false, detail: 'album が必要' });
      let job;
      try { job = new Job(body, { browser, jobsDir: JOBS_DIR, impl: IMPL, version: pkg.version }); }
      catch (e) { return send(res, 400, { ok: false, detail: String(e.message || e) }); }
      jobs.set(job.id, job); orderIds.unshift(job.id);
      job.writeStatus();
      queue.push(job);
      setImmediate(pump);
      return send(res, 200, { ok: true, id: job.id, queued: queue.length - (running ? 0 : 1) });
    }
    if (req.method === 'GET' && p === '/jobs') {
      return send(res, 200, { jobs: orderIds.slice(0, 50).map((id) => jobs.get(id).status()) });
    }
    let m = p.match(/^\/jobs\/([A-Za-z0-9_]+)$/);
    if (m && req.method === 'GET') {
      const j = jobs.get(m[1]);
      if (!j) return send(res, 404, { ok: false, detail: 'no such job' });
      return send(res, 200, j.status());
    }
    m = p.match(/^\/jobs\/([A-Za-z0-9_]+)\/stop$/);
    if (m && req.method === 'POST') {
      const j = jobs.get(m[1]);
      if (!j) return send(res, 404, { ok: false, detail: 'no such job' });
      if (j.state === 'queued') { const i = queue.indexOf(j); if (i >= 0) queue.splice(i, 1); j.state = 'stopped'; j.stopReason = 'stopped'; j.writeStatus(); }
      else j.stop();
      return send(res, 200, { ok: true });
    }
    m = p.match(/^\/jobs\/([A-Za-z0-9_]+)\/log$/);
    if (m && req.method === 'GET') {
      const j = jobs.get(m[1]);
      if (!j) return send(res, 404, { ok: false, detail: 'no such job' });
      let t = ''; try { t = fs.readFileSync(j.logPath, 'utf8'); } catch {}
      return send(res, 200, t, 'text/plain');
    }
    return send(res, 404, { ok: false, detail: 'not found' });
  } catch (e) {
    return send(res, 500, { ok: false, detail: String(e.message || e) });
  }
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`fluent_crawler(${IMPL}) v${pkg.version} listening on http://127.0.0.1:${PORT}  jobs=${JOBS_DIR}`);
});
async function shutdown() {
  console.log('shutting down');
  if (running) running.stop();
  server.close();
  if (browser) await browser.close();
  process.exit(0);
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
