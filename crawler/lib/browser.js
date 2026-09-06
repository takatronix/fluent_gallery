'use strict';
// 内蔵ブラウザ(Playwright Chromium)。1 ジョブ = 1 コンテキスト(人が 1 枚のタブで見る)。
// 画像レスポンスはその場で捕まえて再ダウンロードを避ける(礼儀)。画像の復号と dHash もブラウザにやらせる(依存ゼロ)
const { chromium } = require('playwright');
const { UA_TOKEN } = require('./robots');

const BLOCK_TYPES = new Set(['media', 'font', 'websocket', 'manifest', 'texttrack', 'eventsource']);
const BLOCK_HOST = /(doubleclick|googlesyndication|googleadservices|google-analytics|googletagmanager|facebook\.net|connect\.facebook|adservice|adsystem|scorecardresearch|hotjar|criteo|taboola|outbrain|adnxs|quantserve|newrelic|sentry\.io|mixpanel|segment\.io|amplitude)/i;
const CAPTURE_MAX = 12 * 1024 * 1024;
const CACHE_BYTES = 160 * 1024 * 1024;

class Browser {
  constructor(opts = {}) { this.opts = opts; this.browser = null; this.version = ''; }
  async launch() {
    if (this.browser && this.browser.isConnected()) return this;
    this.browser = await chromium.launch({ headless: this.opts.headless !== false, args: ['--disable-blink-features=AutomationControlled', '--lang=ja-JP'] });
    this.version = this.browser.version();
    this.browser.on('disconnected', () => { this.browser = null; });
    return this;
  }
  userAgent() {
    const major = (this.version || '120').split('.')[0];
    return `Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${major}.0.0.0 Safari/537.36 ${UA_TOKEN}/0.1 (+research)`;
  }
  /** ジョブ用のコンテキスト。戻り: {context, page, util, cache, close} */
  async newContext() {
    await this.launch();
    const context = await this.browser.newContext({
      viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1, locale: 'ja-JP', userAgent: this.userAgent(),
      javaScriptEnabled: true, ignoreHTTPSErrors: false, serviceWorkers: 'block',
    });
    context.setDefaultTimeout(30000);
    await context.route('**/*', (route) => {
      const req = route.request();
      const t = req.resourceType();
      if (BLOCK_TYPES.has(t)) return route.abort();
      try { if (BLOCK_HOST.test(new URL(req.url()).hostname)) return route.abort(); } catch {}
      return route.continue();
    });
    const cache = new Map(); // url → {buf, type}(LRU、上限 CACHE_BYTES)
    let cacheBytes = 0;
    const page = await context.newPage();
    page.on('response', async (res) => {
      try {
        const ct = (res.headers()['content-type'] || '').split(';')[0].trim();
        if (!ct.startsWith('image/') || ct === 'image/svg+xml') return;
        const len = +(res.headers()['content-length'] || 0);
        if (len > CAPTURE_MAX) return;
        const buf = await res.body();
        if (!buf || buf.length < 2048 || buf.length > CAPTURE_MAX) return;
        const url = res.url();
        if (cache.has(url)) return;
        cache.set(url, { buf, type: ct });
        cacheBytes += buf.length;
        while (cacheBytes > CACHE_BYTES && cache.size) { const [k, v] = cache.entries().next().value; cache.delete(k); cacheBytes -= v.buf.length; }
      } catch {}
    });
    const util = await context.newPage(); // 復号/dHash 用の裏ページ(about:blank)
    await util.goto('about:blank');
    return { context, page, util, cache, close: async () => { try { await context.close(); } catch {} } };
  }
  /** 画像バイト列 → {w, h, hash(dHash 64bit hex)}。ブラウザに復号させる(jpg/png/webp/gif/avif) */
  static async decode(util, buf, type) {
    if (buf.length > CAPTURE_MAX) return { err: 'too_big' };
    return util.evaluate(async ({ b64, type }) => {
      const bin = atob(b64);
      const arr = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
      let bmp;
      try { bmp = await createImageBitmap(new Blob([arr], { type: type || 'image/jpeg' })); } catch { return { err: 'decode' }; }
      const w = bmp.width, h = bmp.height;
      const c = new OffscreenCanvas(9, 8);
      const g = c.getContext('2d');
      g.drawImage(bmp, 0, 0, 9, 8);
      const d = g.getImageData(0, 0, 9, 8).data;
      const gray = [];
      for (let i = 0; i < 72; i++) gray.push(0.299 * d[i * 4] + 0.587 * d[i * 4 + 1] + 0.114 * d[i * 4 + 2]);
      let bits = '';
      for (let y = 0; y < 8; y++) for (let x = 0; x < 8; x++) bits += gray[y * 9 + x] < gray[y * 9 + x + 1] ? '1' : '0';
      let hex = '';
      for (let i = 0; i < 64; i += 4) hex += parseInt(bits.slice(i, i + 4), 2).toString(16);
      bmp.close();
      return { w, h, hash: hex };
    }, { b64: buf.toString('base64'), type });
  }
  /** 画像を取りに行く(コンテキストの Cookie/UA を共有、Referer 付き)。戻り {buf, type} か {err} */
  static async fetchImage(context, url, referer, maxBytes) {
    let res;
    try {
      res = await context.request.get(url, {
        headers: { Referer: referer || '', Accept: 'image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8,*/*;q=0.5' },
        timeout: 30000, maxRedirects: 5,
      });
    } catch (e) { return { err: 'net:' + String(e.message || e).slice(0, 80) }; }
    if (!res.ok()) return { err: 'http' + res.status() };
    const ct = (res.headers()['content-type'] || '').split(';')[0].trim();
    const len = +(res.headers()['content-length'] || 0);
    if (len > maxBytes) return { err: 'too_big' };
    let buf;
    try { buf = await res.body(); } catch { return { err: 'body' }; }
    if (buf.length > maxBytes) return { err: 'too_big' };
    const magic = buf.subarray(0, 12);
    const sniff = magic[0] === 0xff && magic[1] === 0xd8 ? 'image/jpeg' : magic.subarray(0, 4).toString('binary') === '\x89PNG' ? 'image/png'
      : magic.subarray(0, 4).toString('binary') === 'RIFF' && magic.subarray(8, 12).toString('binary') === 'WEBP' ? 'image/webp'
      : magic.subarray(0, 4).toString('binary') === 'GIF8' ? 'image/gif' : magic.subarray(4, 8).toString('binary') === 'ftyp' ? 'image/avif' : '';
    if (!sniff && !ct.startsWith('image/')) return { err: 'not_image' };
    return { buf, type: sniff || ct };
  }
  async close() { try { if (this.browser) await this.browser.close(); } catch {} this.browser = null; }
}

module.exports = { Browser };
