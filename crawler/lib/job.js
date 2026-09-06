'use strict';
// 1 ジョブ = 開始 URL から人のように見て回り、内容の画像を出典つきで渡す。リミットのどれかに達したら止まる
const fs = require('fs');
const path = require('path');
const { sleep, now, jitter, sha1, hamming, normalizeUrl, sameSite, isPrivateHost, isMediaHost, IMAGE_EXT, terms, tagsFrom, fmtBytes } = require('./util');
const { Robots } = require('./robots');
const { Browser } = require('./browser');
const { dismissConsent, humanScroll, readPage } = require('./page');
const { junkImage, scoreImage, imageCandidates, junkLink, scoreLink } = require('./score');
const { deliver, saveLocal } = require('./deliver');
const llm = require('./llm');

const DEFAULT_LIMITS = { max_pages: 30, max_images: 100, max_minutes: 10, max_depth: 3, max_bytes_mb: 300, same_site: true, min_side: 300, bored_pages: 6, delay_ms: [800, 2500] };
const IMG_THRESHOLD = 0.3;
const LINK_THRESHOLD = 0.12;
const MAX_PICK_PER_PAGE = 40;
const FRONTIER_CAP = 600;
let seq = 0;

function makeId() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, '0');
  return `j_${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}${(++seq % 100).toString().padStart(2, '0')}`;
}

function normalizeInput(inp) {
  const limits = Object.assign({}, DEFAULT_LIMITS, inp.limits || {});
  limits.max_pages = Math.max(1, Math.min(2000, +limits.max_pages || DEFAULT_LIMITS.max_pages));
  limits.max_images = Math.max(1, Math.min(5000, +limits.max_images || DEFAULT_LIMITS.max_images));
  limits.max_minutes = Math.max(0.1, Math.min(600, +limits.max_minutes || DEFAULT_LIMITS.max_minutes));
  limits.max_depth = Math.max(0, Math.min(10, +limits.max_depth ?? DEFAULT_LIMITS.max_depth));
  limits.max_bytes_mb = Math.max(1, +limits.max_bytes_mb || DEFAULT_LIMITS.max_bytes_mb);
  limits.min_side = Math.max(64, +limits.min_side || DEFAULT_LIMITS.min_side);
  limits.bored_pages = Math.max(1, +limits.bored_pages || DEFAULT_LIMITS.bored_pages);
  limits.same_site = limits.same_site !== false;
  let d = Array.isArray(limits.delay_ms) ? limits.delay_ms.map(Number) : [DEFAULT_LIMITS.delay_ms[0], DEFAULT_LIMITS.delay_ms[1]];
  d = [Math.max(500, d[0] || 800), Math.max(500, d[1] || 2500)];
  if (d[1] < d[0]) d[1] = d[0];
  limits.delay_ms = d;
  return {
    url: String(inp.url || '').trim(), goal: String(inp.goal || '').trim(), album: String(inp.album || '').trim(),
    gallery: String(inp.gallery || 'http://127.0.0.1:8793').replace(/\/$/, ''), deliver: inp.deliver !== false, judge: inp.judge !== false,
    headless: inp.headless !== false, limits,
  };
}

class Job {
  constructor(input, { browser, jobsDir, impl = 'fable', version = '0.1.0' }) {
    this.input = normalizeInput(input);
    const u = new URL(this.input.url);
    if (isPrivateHost(u.hostname)) throw new Error('内部ネットワーク宛ての URL は対象外');
    if (isMediaHost(u.hostname)) throw new Error('動画/SNS 媒体は対象外(別パイプライン)');
    if (!this.input.album) throw new Error('album は必須');
    this.id = makeId();
    this.impl = impl; this.version = version; this.browser = browser;
    this.dir = path.join(jobsDir, this.id);
    fs.mkdirSync(this.dir, { recursive: true });
    this.logPath = path.join(this.dir, 'log.jsonl');
    this.state = 'queued'; this.created = now(); this.started = 0; this.ended = 0;
    this.pagesVisited = 0; this.imagesSeen = 0; this.imagesPicked = 0;
    this.delivered = 0; this.accepted = 0; this.rejected = 0; this.dup = 0; this.failed = 0; this.saved = 0; this.bytes = 0; this.errors = 0;
    this.current = null; this.recent = []; this.stopReason = null; this._stop = false; this.bored = 0; this.skippedDepth = 0;
    this.frontier = []; this.enqueued = new Set(); this.visited = new Set();
    this.seenPages = new Map(); // 画像 URL → 何ページで見たか(部品検出)
    this.sha1s = new Set(); this.hashes = [];
    this.hostLast = new Map();
    this.goalTerms = terms(this.input.goal);
    this.order = 0;
  }

  status() {
    const L = this.input.limits;
    return {
      id: this.id, impl: this.impl, state: this.state, album: this.input.album, goal: this.input.goal, start_url: this.input.url,
      deliver: this.input.deliver, gallery: this.input.gallery, limits: L,
      started: this.started || null, elapsed_s: this.started ? Math.round(((this.ended || now()) - this.started) * 10) / 10 : 0,
      pages_visited: this.pagesVisited, frontier: this.frontier.length, images_seen: this.imagesSeen, images_picked: this.imagesPicked,
      delivered: this.delivered, accepted: this.accepted, rejected: this.rejected, dup: this.dup, failed: this.failed, saved: this.saved,
      bytes: this.bytes, current: this.current, recent: this.recent.slice(0, 10), stop_reason: this.stopReason, errors: this.errors,
      dir: this.dir,
    };
  }
  stop() { this._stop = true; }
  writeStatus() { try { fs.writeFileSync(path.join(this.dir, 'status.json'), JSON.stringify(this.status(), null, 1)); } catch {} }
  log(obj) { try { fs.appendFileSync(this.logPath, JSON.stringify(obj) + '\n'); } catch {} }

  gained() { return this.input.deliver ? this.accepted + this.pendingCount() : this.saved; }
  pendingCount() { return this._pending || 0; }
  imagesTaken() { return this.input.deliver ? this.delivered - this.dup - this.failed : this.saved; }

  push(url, depth, score, why, offsite) {
    const n = normalizeUrl(url);
    if (!n || this.visited.has(n) || this.enqueued.has(n)) return false;
    this.enqueued.add(n);
    this.frontier.push({ url: n, depth, score, why, offsite: !!offsite, order: this.order++ });
    if (this.frontier.length > FRONTIER_CAP) {
      this.frontier.sort((a, b) => b.score - a.score || a.depth - b.depth || a.order - b.order);
      for (const x of this.frontier.splice(FRONTIER_CAP)) this.enqueued.delete(x.url);
    }
    return true;
  }
  pop() {
    if (!this.frontier.length) return null;
    this.frontier.sort((a, b) => b.score - a.score || a.depth - b.depth || a.order - b.order);
    return this.frontier.shift();
  }

  limitHit({ ignorePages = false } = {}) {
    const L = this.input.limits;
    if (this._stop) return 'stopped';
    if (now() - this.started > L.max_minutes * 60) return 'max_minutes';
    if (this.imagesTaken() >= L.max_images) return 'max_images';
    if (!ignorePages && this.pagesVisited >= L.max_pages) return 'max_pages';
    if (this.bytes > L.max_bytes_mb * 1024 * 1024) return 'max_bytes';
    if (this.bored >= L.bored_pages) return 'bored';
    return null;
  }

  async run() {
    this.state = 'running'; this.started = now(); this.writeStatus();
    let ctx = null;
    const watchdog = setTimeout(() => { this._stop = true; this._watchdog = true; }, (this.input.limits.max_minutes * 60 + 90) * 1000);
    try {
      ctx = await this.browser.newContext();
      this.ctx = ctx;
      this.robots = new Robots(async (u) => {
        const r = await ctx.context.request.get(u, { timeout: 8000, maxRedirects: 3 });
        return r.ok() ? await r.text() : (r.status() >= 500 ? null : '');
      });
      if (llm.enabled && this.goalTerms.length) {
        const extra = await llm.expandGoal(this.input.goal);
        for (const w of extra) if (!this.goalTerms.includes(w)) this.goalTerms.push(w);
        this.log({ t: now(), llm_terms: extra });
      }
      this.push(this.input.url, 0, 1, '開始 URL', false);
      while (true) {
        const hit = this.limitHit();
        if (hit) { this.stopReason = this._watchdog ? 'error:watchdog' : hit; break; }
        const next = this.pop();
        if (!next) { this.stopReason = this.skippedDepth > 0 && this.pagesVisited > 0 ? 'max_depth_exhausted' : 'frontier_empty'; break; }
        this.visited.add(next.url);
        try { await this.visit(next); }
        catch (e) {
          this.errors++;
          this.log({ t: now(), url: next.url, error: String(e.message || e).slice(0, 200) });
          this.recent.unshift({ url: next.url, title: '', picked: 0, delivered: 0, why: 'エラー: ' + String(e.message || e).slice(0, 80) });
          this.recent = this.recent.slice(0, 10);
          this.bored++;
        }
        this.writeStatus();
      }
      this.state = this.stopReason === 'stopped' ? 'stopped' : 'done';
    } catch (e) {
      this.state = 'error'; this.stopReason = 'error:' + String(e.message || e).slice(0, 120); this.errors++;
    } finally {
      clearTimeout(watchdog);
      this.ended = now(); this.current = null;
      if (ctx) await ctx.close();
      this.writeStatus();
    }
    return this.status();
  }

  async politeWait(url) {
    const host = new URL(url).hostname;
    const cd = await this.robots.crawlDelay(url);
    let wait = jitter(this.input.limits.delay_ms);
    if (cd) wait = Math.max(wait, Math.min(cd, 30) * 1000);
    const last = this.hostLast.get(host) || 0;
    const due = last + wait / 1000 - now();
    if (due > 0) await sleep(due * 1000);
    this.hostLast.set(host, now());
  }

  async visit(item) {
    const { page, util, context, cache } = this.ctx;
    const L = this.input.limits;
    const t0 = now();
    this.current = { url: item.url, title: '' };
    if (!(await this.robots.allowed(item.url))) {
      this.log({ t: now(), url: item.url, depth: item.depth, skipped: 'robots' });
      return; // 訪問数にも飽きにも数えない
    }
    await this.politeWait(item.url);
    let resp;
    try { resp = await page.goto(item.url, { waitUntil: 'domcontentloaded', timeout: 30000 }); }
    catch (e) { throw new Error('goto: ' + String(e.message || e).split('\n')[0].slice(0, 100)); }
    this.pagesVisited++;
    const ct = resp ? (resp.headers()['content-type'] || '') : '';
    if (resp && !resp.ok() && resp.status() !== 304) throw new Error(`HTTP ${resp.status()}`);
    if (ct && !/text\/html|application\/xhtml/.test(ct)) {
      // 画像そのものなら 1 枚の候補として扱う。それ以外(PDF 等)は見ない
      if (ct.startsWith('image/')) {
        const got = await this.take({ src: item.url, currentSrc: item.url, srcset: [], lazy: [], link: '', alt: '', caption: '', context: '', head: '', w: 0, h: 0, nw: 0, nh: 0, inMain: true, inFigure: false, cls: '', kind: 'img' },
          { url: item.url, title: '' }, item, 1, 'URL 直接');
        this.log({ t: now(), url: item.url, depth: item.depth, direct_image: true, taken: got.ok });
        this.bored = got.ok ? 0 : this.bored + 1;
      } else {
        this.log({ t: now(), url: item.url, depth: item.depth, skipped: 'not_html:' + ct.slice(0, 40) });
      }
      return;
    }
    await Promise.race([page.waitForLoadState('networkidle').catch(() => {}), sleep(4000)]);
    const consent = await dismissConsent(page);
    const screens = await humanScroll(page, { maxScreens: 12 });
    await Promise.race([page.waitForLoadState('networkidle').catch(() => {}), sleep(2500)]);
    const inv = await readPage(page);
    const pageUrl = normalizeUrl(inv.url) || item.url;
    this.visited.add(pageUrl);
    this.current = { url: pageUrl, title: inv.title };
    if (item.depth === 0 && !this.topicTerms) {
      // 開始ページの題と h1 = いま居る節の主題(「Saturn - NASA Science」→ saturn)。goal が別言語でも話題を外さない手がかり
      const t = terms(`${inv.title.replace(/^\s*(Category|File|Portal|Wikipedia|Commons|Tag|Topic|カテゴリ)\s*[:：]\s*/i, '').split(/[|\-–—:：]/)[0]} ${inv.headings[0] || ''}`).filter((w) => w.length >= 3 && !/^(wikimedia|commons|category|wiki|nasa|science|home|page)$/i.test(w));
      this.topicTerms = t.slice(0, 6);
      this.log({ t: now(), topic_terms: this.topicTerms });
    }
    this.imagesSeen += inv.images.length;
    // 部品検出: このページで見た画像 URL を数える(1 ページ 1 回)
    const seenHere = new Set();
    for (const im of inv.images) { const k = im.currentSrc || im.src; if (k && !seenHere.has(k)) { seenHere.add(k); this.seenPages.set(k, (this.seenPages.get(k) || 0) + 1); } }

    // ---- 画像を選ぶ(人が「このページの内容の画像」と思う物だけ)
    const skipped = {};
    const picks = [];
    for (const im of inv.images) {
      const j = junkImage(im, { minSide: L.min_side, seenPages: this.seenPages });
      if (j) { skipped[j] = (skipped[j] || 0) + 1; continue; }
      const sc = scoreImage(im, this.goalTerms, { minSide: L.min_side });
      if (sc.score < IMG_THRESHOLD) { skipped.low = (skipped.low || 0) + 1; continue; }
      picks.push({ im, ...sc });
    }
    if (inv.ogImage && !picks.length && item.depth > 0 && !/\.svg/i.test(inv.ogImage)) {
      picks.push({ im: { src: inv.ogImage, currentSrc: inv.ogImage, srcset: [], lazy: [], link: '', alt: inv.title, caption: '', context: inv.desc, head: '', w: 0, h: 0, nw: 0, nh: 0, inMain: true, inFigure: false, cls: '', kind: 'og' }, score: 0.45, why: 'og:image(主画像)', rel: 0 });
    }
    picks.sort((a, b) => b.score - a.score);
    const chosen = picks.slice(0, MAX_PICK_PER_PAGE);
    this.imagesPicked += chosen.length;
    const pickedLog = [];
    let pageDelivered = 0, pageAccepted = 0, pageRejected = 0, pageNew = 0;
    for (let i = 0; i < chosen.length; i++) {
      if (this._stop || this.limitHit({ ignorePages: true })) break; // ページ上限に達していても、見ているページの画像は取り切る
      const c = chosen[i];
      const r = await this.take(c.im, { url: pageUrl, title: inv.title }, item, c.score, c.why, i);
      pickedLog.push({ url: r.url || c.im.currentSrc || c.im.src, w: r.w || 0, h: r.h || 0, score: Math.round(c.score * 100) / 100, why: c.why, result: r.result });
      if (r.result === 'accepted' || r.result === 'pending' || r.result === 'saved') pageNew++;
      if (r.result === 'accepted') pageAccepted++;
      if (r.result === 'rejected') pageRejected++;
      if (r.delivered) pageDelivered++;
    }

    // ---- 次に見に行くページを選ぶ
    const nextLog = [];
    let added = 0;
    for (const lk of inv.links) {
      const j = junkLink(lk.href, pageUrl, { sameSite: L.same_site, startUrl: this.input.url, pageOffsite: item.offsite });
      if (j) continue;
      const depth = item.depth + 1;
      if (depth > L.max_depth) { this.skippedDepth++; continue; }
      const sc = scoreLink(lk, this.goalTerms, depth, { startUrl: this.input.url, topicTerms: this.topicTerms || [] });
      if (sc.score < LINK_THRESHOLD) continue;
      const offsite = !sameSite(lk.href, this.input.url);
      if (this.push(lk.href, depth, sc.score, sc.why, offsite)) { added++; if (nextLog.length < 8) nextLog.push({ url: lk.href, score: Math.round(sc.score * 100) / 100, why: sc.why, text: lk.text.slice(0, 40) }); }
    }
    nextLog.sort((a, b) => b.score - a.score);
    const why = `${inv.title ? '' : ''}${chosen.length ? `内容画像 ${chosen.length} 枚` : '内容画像なし'}(見た ${inv.images.length})、候補追加 ${added}` + (nextLog[0] ? `。次: ${nextLog[0].text || nextLog[0].url.replace(/^https?:\/\/[^/]+/, '')} (${nextLog[0].why})` : '');
    this.recent.unshift({ url: pageUrl, title: inv.title, picked: chosen.length, delivered: pageDelivered, accepted: pageAccepted, why });
    this.recent = this.recent.slice(0, 10);
    this.bored = pageNew > 0 ? 0 : this.bored + 1;
    this.log({
      t: now(), url: pageUrl, title: inv.title, depth: item.depth, score: Math.round(item.score * 100) / 100, why_visited: item.why,
      consent: consent || null, screens, images_seen: inv.images.length, picked: pickedLog, skipped,
      next: nextLog, frontier: this.frontier.length, delivered: pageDelivered, accepted: pageAccepted, rejected: pageRejected, new: pageNew,
      ms: Math.round((now() - t0) * 1000),
    });
    cache.clear();
  }

  /** 1 枚を取る: 最高解像度の候補から順に取得 → 復号/寸法 → 重複 → 渡す(or 保存)。戻り {result, url, w, h, delivered} */
  async take(im, pageInfo, item, score, why, idx = 0) {
    const { context, util, cache } = this.ctx;
    const L = this.input.limits;
    const cands = imageCandidates(im);
    let got = null, usedUrl = '', lastErr = 'no_candidate';
    for (const u of cands) {
      let host; try { host = new URL(u).hostname; } catch { continue; }
      if (isPrivateHost(host)) continue;
      if (this.bytes > L.max_bytes_mb * 1024 * 1024) { lastErr = 'max_bytes'; break; }
      let r = cache.get(u);
      if (r) { r = { buf: r.buf, type: r.type, cached: true }; this.bytes += r.buf.length; } // ブラウザが読み込んだ分も転送量として数える(再 DL はしない)
      else {
        r = await Browser.fetchImage(context, u, pageInfo.url, Math.min(40 * 1024 * 1024, L.max_bytes_mb * 1024 * 1024));
        if (r.err) { lastErr = r.err; continue; }
        this.bytes += r.buf.length;
      }
      const d = await Browser.decode(util, r.buf, r.type);
      if (d.err) { lastErr = d.err; continue; }
      if (Math.min(d.w, d.h) < L.min_side) { lastErr = `small ${d.w}x${d.h}`; continue; }
      got = { ...r, ...d }; usedUrl = u; break;
    }
    if (!got) return { result: 'skip:' + lastErr, url: cands[0] || '', delivered: false };
    const h = sha1(got.buf);
    if (this.sha1s.has(h)) return { result: 'dup_local', url: usedUrl, w: got.w, h: got.h, delivered: false };
    if (this.hashes.some((x) => hamming(x, got.hash) <= 6)) return { result: 'near_dup', url: usedUrl, w: got.w, h: got.h, delivered: false };
    this.sha1s.add(h); this.hashes.push(got.hash);
    const host = (() => { try { return new URL(pageInfo.url).hostname; } catch { return ''; } })();
    const tags = tagsFrom(this.input.goal, '', 5);
    if (host && !tags.includes(host)) tags.push(host);
    const meta = {
      album: this.input.album, judge: this.input.judge, rights: 'unknown', min_side: L.min_side,
      crawl: {
        engine: `browser:${this.impl}`, url: usedUrl, landing: pageInfo.url, title: pageInfo.title, query: this.input.goal, album: this.input.album,
        tags, alt: im.alt || '', caption: im.caption || '', context: (im.context || '').slice(0, 300), head: im.head || '',
        score: Math.round(score * 100) / 100, why, depth: item.depth, page_index: this.pagesVisited, w: got.w, h: got.h, dhash: got.hash, cached: !!got.cached,
      },
    };
    if (!this.input.deliver) {
      saveLocal(path.join(this.dir, 'images'), h, got.type, got.buf, meta);
      this.saved++;
      return { result: 'saved', url: usedUrl, w: got.w, h: got.h, delivered: false };
    }
    let res = null;
    for (let attempt = 0; attempt < 2; attempt++) {
      try { res = await deliver(this.input.gallery, meta, got.buf, got.type); break; }
      catch (e) { res = { ok: false, reason: 'net', why: String(e.message || e).slice(0, 120) }; if (e.status && e.status < 500) break; await sleep(1500); }
    }
    this.delivered++;
    if (res.ok) {
      if (res.verdict === 'accepted') this.accepted++; else this._pending = (this._pending || 0) + 1;
      return { result: res.verdict || 'accepted', url: usedUrl, w: got.w, h: got.h, delivered: true, sha1: res.sha1 };
    }
    if (res.reason === 'rejected') this.rejected++;
    else if (res.reason === 'dup') this.dup++;
    else { this.failed++; if (res.reason === 'net') this.errors++; }
    return { result: res.reason === 'rejected' ? 'rejected' : res.reason === 'dup' ? 'dup' : 'failed:' + (res.reason || '?') + (res.why ? ' ' + res.why : ''), url: usedUrl, w: got.w, h: got.h, delivered: true };
  }
}

module.exports = { Job, normalizeInput, DEFAULT_LIMITS };
