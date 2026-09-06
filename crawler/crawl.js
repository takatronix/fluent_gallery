#!/usr/bin/env node
'use strict';
// CLI: node crawl.js --url <URL> --goal "<目標>" --album <名前> [--no-deliver] [--gallery http://127.0.0.1:8793] [--max-pages 20 --max-images 50 --max-minutes 5 --max-depth 2 --min-side 300 --headed]
const path = require('path');
const { Browser } = require('./lib/browser');
const { Job } = require('./lib/job');
const pkg = require('./package.json');

const args = process.argv.slice(2);
const opt = {};
for (let i = 0; i < args.length; i++) {
  const a = args[i];
  if (!a.startsWith('--')) continue;
  const k = a.slice(2);
  if (['no-deliver', 'no-judge', 'headed', 'quiet', 'offsite'].includes(k)) opt[k] = true;
  else { opt[k] = args[i + 1]; i++; }
}
if (!opt.url || !opt.album) {
  console.error('usage: node crawl.js --url <URL> --album <name> [--goal "..."] [--no-deliver] [--gallery URL] [--max-pages N] [--max-images N] [--max-minutes N] [--max-depth N] [--min-side N] [--headed] [--offsite]');
  process.exit(2);
}
const limits = {};
for (const [k, key] of [['max-pages', 'max_pages'], ['max-images', 'max_images'], ['max-minutes', 'max_minutes'], ['max-depth', 'max_depth'], ['min-side', 'min_side'], ['bored', 'bored_pages'], ['max-bytes', 'max_bytes_mb']]) if (opt[k] != null) limits[key] = +opt[k];
if (opt.offsite) limits.same_site = false;
if (opt.delay) { const [a, b] = String(opt.delay).split(',').map(Number); limits.delay_ms = [a, b || a]; }

(async () => {
  const browser = new Browser({ headless: !opt.headed });
  const job = new Job({ url: opt.url, goal: opt.goal || '', album: opt.album, gallery: opt.gallery, deliver: !opt['no-deliver'], judge: !opt['no-judge'], limits },
    { browser, jobsDir: opt.jobs || path.join(__dirname, 'jobs'), impl: 'fable', version: pkg.version });
  console.log(`job ${job.id} → ${opt.url}\n goal=${job.input.goal || '(なし)'} album=${job.input.album} deliver=${job.input.deliver} gallery=${job.input.gallery}\n limits=${JSON.stringify(job.input.limits)}`);
  let lastLine = '';
  const tick = setInterval(() => {
    if (opt.quiet) return;
    const s = job.status();
    const line = `[${s.elapsed_s}s] pages ${s.pages_visited} frontier ${s.frontier} seen ${s.images_seen} picked ${s.images_picked} ${s.deliver ? `delivered ${s.delivered} accepted ${s.accepted} rejected ${s.rejected} dup ${s.dup}` : `saved ${s.saved}`} | ${s.current ? s.current.url.slice(0, 90) : ''}`;
    if (line !== lastLine) { console.log(line); lastLine = line; }
  }, 2000);
  process.on('SIGINT', () => { console.log('stopping…'); job.stop(); });
  const s = await job.run();
  clearInterval(tick);
  await browser.close();
  console.log(JSON.stringify(s, null, 1));
  console.log(`\n終了: ${s.stop_reason}  pages=${s.pages_visited} picked=${s.images_picked} ${s.deliver ? `delivered=${s.delivered} accepted=${s.accepted} rejected=${s.rejected} dup=${s.dup} failed=${s.failed}` : `saved=${s.saved}`} bytes=${s.bytes} log=${s.dir}/log.jsonl`);
  process.exit(s.state === 'error' ? 1 : 0);
})();
