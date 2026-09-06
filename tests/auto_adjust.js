// 自動色補正: 実ブラウザ、応答/デコード順の競合、版付きキャッシュ、原本保持。
// 実行: FG_URL=http://127.0.0.1:<isolated-test-port> node tests/auto_adjust.js
// 必ず専用データルートのサーバを指定する。画像はこの実行だけのPNGを生成する。
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const puppeteer = require('puppeteer-core');

const BASE = process.env.FG_URL;
assert(BASE, '専用テストサーバの FG_URL を指定してください');
const nonce = `${Date.now().toString(36)}_${crypto.randomBytes(6).toString('hex')}`;
const source = `crawl:_autotest_${nonce}`;
const createdShas = new Set();
const originalBytes = new Map();
const originalTiers = new Map();
const tiers = [['micro', 120], ['thumb', 360], ['preview', 1080]];
const errors = [];
const editRequests = [];
let browser, page, checks = 0;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const passed = message => { checks++; console.log(`✅ ${message}`); };

async function api(path, body, method = body === undefined ? 'GET' : 'PUT') {
  const response = await fetch(BASE + path, {method,
    headers: body === undefined ? {} : {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body)});
  const raw = await response.text();
  assert(response.ok, `${method} ${path}: ${response.status} ${raw}`);
  return raw ? JSON.parse(raw) : null;
}
async function bytes(path) {
  const response = await fetch(BASE + path);
  assert(response.ok, `GET ${path}: ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}
async function until(probe, description, timeout = 30000) {
  const start = Date.now();
  let last;
  while (Date.now() - start < timeout) {
    last = await probe();
    if (last) return last;
    await sleep(50);
  }
  throw new Error(`Timeout: ${description}; last=${JSON.stringify(last)}`);
}
async function imageStats(path) {
  return page.evaluate(async path => {
    const response = await fetch(path);
    if (!response.ok) throw new Error(`image ${path}: ${response.status}`);
    const image = await createImageBitmap(await response.blob());
    const canvas = document.createElement('canvas'); canvas.width = canvas.height = 64;
    const ctx = canvas.getContext('2d'); ctx.drawImage(image, 0, 0, 64, 64);
    const data = ctx.getImageData(0, 0, 64, 64).data;
    const samples = [], channels = [0, 0, 0];
    for (let i = 0; i < data.length; i += 4) {
      samples.push((data[i] + data[i + 1] + data[i + 2]) / 3);
      for (let c = 0; c < 3; c++) channels[c] += data[i + c] / 4096;
    }
    samples.sort((a, b) => a - b);
    const result = {width: image.width, height: image.height, channels,
      mean: channels.reduce((a, b) => a + b) / 3,
      low: samples[Math.floor(samples.length * .05)], high: samples[Math.floor(samples.length * .95)]};
    image.close(); return result;
  }, path);
}
async function currentImage() {
  return page.evaluate(() => {
    const image = $('lbimg'), rect = image.getBoundingClientRect();
    return {sha: items[lbIdx]?.sha1, src: image.src, tier: image.dataset.tier,
      width: rect.width, loaded: image.complete && image.naturalWidth > 0,
      rev: lbMeta?.edits_rev, edits: lbMeta?.edits || [],
      busy: $('editpanel').getAttribute('aria-busy'), status: $('edstatus').textContent,
      autoDisabled: $('edauto').disabled};
  });
}
async function ready(sha, edited = false) {
  return until(async () => {
    const current = await currentImage();
    const url = new URL(current.src || BASE);
    return current.sha === sha && current.loaded && current.width > 100 && !current.autoDisabled &&
      current.busy !== 'true' && (edited ? current.edits.length > 0 &&
        url.pathname === '/render/' + sha && url.searchParams.get('v') === current.rev :
        current.edits.length === 0 && url.pathname === '/img/' + sha) && current;
  }, `visible ${edited ? 'edited' : 'original'} ${sha}`, 60000);
}
async function show(sha, edited = false) {
  await page.evaluate(sha => {
    const index = items.findIndex(value => value.sha1 === sha);
    if (index < 0) throw new Error(`missing fixture ${sha}`);
    if ($('lb').classList.contains('show')) lbShow(index, 0); else openLb(index);
    $('lb').classList.add('editing');
  }, sha);
  return ready(sha, edited);
}
async function clearCurrent(sha) {
  await page.evaluate(() => edClear());
  return ready(sha, false);
}
async function holdEditResponse(sha) {
  await page.evaluate(sha => {
    const realFetch = window.fetch;
    window.__autoResponseGate = {realFetch, received: false, result: null, release: null};
    window.fetch = async (input, init) => {
      const response = await realFetch(input, init);
      const url = new URL(typeof input === 'string' ? input : input.url, location.href);
      const gate = window.__autoResponseGate;
      if (!gate.received && init?.method === 'PUT' && url.pathname === '/api/edits/' + sha) {
        gate.result = await response.clone().json(); gate.received = true;
        await new Promise(resolve => { gate.release = resolve; });
      }
      return response;
    };
  }, sha);
}
async function releaseEditResponse() {
  await page.evaluate(() => {
    const gate = window.__autoResponseGate;
    window.fetch = gate.realFetch; gate.release();
  });
}
async function holdDecode(pathname, marker = null) {
  await page.evaluate(({pathname, marker}) => {
    const original = HTMLImageElement.prototype.decode;
    window.__autoDecodeGate = {original, entered: false, release: null, completed: false};
    HTMLImageElement.prototype.decode = async function() {
      await original.call(this);
      const url = new URL(this.src, location.href), gate = window.__autoDecodeGate;
      if (!gate.entered && url.pathname === pathname && (!marker || url.searchParams.get('test_gate') === marker)) {
        gate.entered = true;
        await new Promise(resolve => { gate.release = resolve; });
        gate.completed = true;
      }
    };
  }, {pathname, marker});
}
async function releaseDecode() {
  await page.evaluate(async () => {
    const gate = window.__autoDecodeGate;
    HTMLImageElement.prototype.decode = gate.original;
    gate.release();
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
  });
}
async function verifyOriginals() {
  for (const sha of createdShas) {
    assert.equal(hash(await bytes('/img/' + sha)), originalBytes.get(sha), 'original PNG bytes changed');
    for (const [tier] of tiers) {
      assert.equal(hash(await bytes(`/${tier}/${sha}`)), originalTiers.get(`${tier}/${sha}`),
        `unversioned original ${tier} bytes changed`);
    }
  }
}
async function verifyUnversionedRenders(sha, expectedMean) {
  // The mask UI uses v=0 before an image has edit history. These URLs mean current
  // state and must remain revalidatable, including the original-file fast path.
  for (const query of ['', '?w=1600', '?v=0', '?w=360&seg=1&v=0']) {
    const path = '/render/' + sha + query;
    const response = await fetch(BASE + path);
    assert(response.ok, `unversioned/mask compatibility ${path}: ${response.status}`);
    assert.match(response.headers.get('cache-control') || '', /\bno-cache\b/, path);
    assert(!/immutable/.test(response.headers.get('cache-control') || ''), path);
    const imageBytes = Buffer.from(await response.arrayBuffer());
    const actual = await imageStats(`data:${response.headers.get('content-type')};base64,${imageBytes.toString('base64')}`);
    assert(Math.abs(actual.mean - expectedMean) < 2,
      `${path} must show current image state: ${JSON.stringify({actual, expectedMean})}`);
  }
}

(async () => {
  browser = await puppeteer.launch({executablePath: process.env.CHROME || '/usr/bin/google-chrome',
    headless: 'new', args: ['--no-sandbox'], defaultViewport: {width: 1600, height: 1000}});
  page = await browser.newPage();
  page.on('pageerror', error => errors.push(error.message));
  page.on('request', request => {
    if (request.method() === 'PUT' && new URL(request.url()).pathname.startsWith('/api/edits/')) {
      editRequests.push({url: request.url(), body: JSON.parse(request.postData())});
    }
  });
  await page.goto(BASE, {waitUntil: 'networkidle2', timeout: 60000});
  await page.waitForFunction(() => typeof go === 'function' && typeof lbView === 'object');
  await page.evaluate(() => localStorage.setItem('fg_lbseg', '0'));
  const uploaded = await page.evaluate(async ({source, nonce}) => {
    const form = new FormData(); form.append('source', source);
    for (let index = 0; index < 2; index++) {
      const canvas = document.createElement('canvas'); canvas.width = 1456; canvas.height = 1092;
      const ctx = canvas.getContext('2d');
      for (let x = 0; x < canvas.width; x++) {
        const tone = 12 + Math.round(80 * x / (canvas.width - 1));
        ctx.fillStyle = `rgb(${tone},${tone},${tone})`; ctx.fillRect(x, 0, 1, canvas.height);
      }
      // A few neutral pixels make this source unique without affecting automatic statistics.
      [...nonce + index].forEach((character, x) => {
        const tone = character.charCodeAt(0); ctx.fillStyle = `rgb(${tone},${tone},${tone})`;
        ctx.fillRect(10 + x * 3, 10, 2, 2);
      });
      form.append('file', await new Promise(resolve => canvas.toBlob(resolve, 'image/png')), `auto-${index}.png`);
    }
    const response = await fetch('/api/upload', {method: 'POST', body: form});
    return {ok: response.ok, data: await response.json()};
  }, {source, nonce});
  assert(uploaded.ok && uploaded.data.added === 2, JSON.stringify(uploaded));
  const listing = await api('/api/images?' + new URLSearchParams({source, limit: '10'}));
  assert.equal(listing.items.length, 2);
  const [sha, other] = listing.items.map(value => value.sha1);
  for (const image of listing.items) {
    createdShas.add(image.sha1);
    originalBytes.set(image.sha1, hash(await bytes('/img/' + image.sha1)));
    for (const [tier] of tiers) originalTiers.set(`${tier}/${image.sha1}`, hash(await bytes(`/${tier}/${image.sha1}`)));
  }
  await page.evaluate(async source => { await go({type: 'lib', key: 'all', criteria: {source}}); }, source);
  await page.waitForFunction(shas => items.length === 2 && items.every(value => shas.includes(value.sha1)), {}, [sha, other]);
  await show(sha);
  const before = await imageStats('/img/' + sha);
  assert(before.mean < 65, JSON.stringify(before));
  passed('一意な暗いグレースケールPNGを2枚作成・原本と各段サムネを記録');
  await verifyUnversionedRenders(sha, before.mean);
  passed('履歴なしのv=0マスクURLを受理・無版renderの原本/縮小経路をno-cacheで返す');

  // Make the delayed reply deterministic and verify the actual button cannot stack operations.
  await holdEditResponse(sha);
  const requestsBefore = editRequests.length;
  await page.click('#edauto');
  await page.waitForFunction(() => window.__autoResponseGate.received);
  const firstSaved = await page.evaluate(() => window.__autoResponseGate.result);
  const immediateImages = new Map(await Promise.all(tiers.map(async ([tier]) => {
    const path = `/${tier}/${sha}?v=${encodeURIComponent(firstSaved.rev)}`;
    const responseBytes = await bytes(path);
    const dataURL = 'data:image/jpeg;base64,' + responseBytes.toString('base64');
    const actual = await imageStats(dataURL);
    assert(actual.mean > before.mean + 20,
      `${tier} served an old original immediately after editing: ${JSON.stringify(actual)}`);
    return [path, hash(responseBytes)];
  })));
  const busy = await currentImage();
  assert.equal(busy.busy, 'true'); assert(busy.autoDisabled && busy.status.trim(), JSON.stringify(busy));
  await page.click('#edauto');
  assert.equal(editRequests.length - requestsBefore, 1, 'rapid second click must not enqueue a duplicate auto');
  await holdDecode('/render/' + sha);
  await releaseEditResponse();
  await page.waitForFunction(() => window.__autoDecodeGate.entered);
  const decoding = await currentImage();
  assert(decoding.busy === 'true' && decoding.autoDisabled && decoding.loaded && decoding.width > 100,
    'render decoding must keep the previous image visible and show progress until ready');
  await releaseDecode();
  const first = await ready(sha, true);
  assert.equal(first.edits.length, 1); assert.equal(first.edits[0].op, 'auto');
  const after = await imageStats(first.src);
  assert(after.mean > before.mean + 20, `auto must visibly brighten dark neutral image: ${JSON.stringify({before, after})}`);
  assert(Math.max(...after.channels) - Math.min(...after.channels) < 3, 'gray image must remain neutral');
  await verifyUnversionedRenders(sha, after.mean);
  await page.screenshot({path: '/tmp/fg-auto-adjust-desktop.png'});
  passed(`自動ボタンに即座に処理中表示・連打は1履歴・明度 ${before.mean.toFixed(1)} → ${after.mean.toFixed(1)}`);

  // Fetch each revision immediately, including cold sizes, before asynchronous work could replace originals.
  const oldRevision = first.rev, oldImages = immediateImages;
  for (const [tier, width] of tiers) {
    const path = `/${tier}/${sha}?v=${encodeURIComponent(first.rev)}`;
    assert.equal(hash(await bytes(path)), oldImages.get(path), `${tier} revision changed after first response`);
    const [actual, expected] = await Promise.all([imageStats(path), imageStats(`/render/${sha}?w=${width}&v=${first.rev}`)]);
    assert(actual.mean > before.mean + 20, `${tier} served the old original under the new revision`);
    assert(Math.abs(actual.mean - expected.mean) < 3, `${tier} doesn't match edited render: ${JSON.stringify({actual, expected})}`);
    assert.equal(actual.width, width);
  }
  const oldRender = `/render/${sha}?w=1600&v=${first.rev}`;
  oldImages.set(oldRender, hash(await bytes(oldRender)));
  await verifyOriginals();
  passed('現在版のmicro/thumb/previewを即時取得して編集結果と照合・無版URLと原本は不変');

  const compare = await page.$('#edcompare'), box = await compare.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await until(async () => {
    const current = await currentImage();
    return current.loaded && new URL(current.src).pathname === '/img/' + sha;
  }, 'held original comparison');
  const comparison = await imageStats((await currentImage()).src);
  assert(Math.abs(comparison.mean - before.mean) < .5);
  await page.mouse.up();
  await ready(sha, true);
  passed('原本比較を押している間は実際の/img原本・離すと補正後へ復帰');

  await page.click('#edactions button[onclick="edUndo()"]');
  await ready(sha, false);
  assert.equal((await api('/api/edits/' + sha)).edits.length, 0);
  for (const [path, historicalHash] of oldImages) {
    const response = await fetch(BASE + path);
    if (response.status === 409) assert.match(response.headers.get('cache-control') || '', /no-store/);
    else {
      assert(response.ok, `stale ${path}: ${response.status}`);
      assert.equal(hash(Buffer.from(await response.arrayBuffer())), historicalHash,
        'an old immutable revision must never be filled with a newer image');
    }
  }
  passed(`一手戻すで原本表示・古い版 ${oldRevision} のURLに別の画像をimmutable配信しない`);

  // Hold a decoded original preview across auto completion to reproduce its late overwrite.
  await holdDecode('/preview/' + sha);
  await page.evaluate(sha => lbShow(items.findIndex(value => value.sha1 === sha), 0), sha);
  await page.waitForFunction(() => window.__autoDecodeGate.entered);
  await page.click('#edauto');
  const withHeldPreview = await ready(sha, true);
  await releaseDecode();
  const afterLatePreview = await currentImage();
  assert.equal(afterLatePreview.src, withHeldPreview.src, 'late original preview replaced completed auto render');
  assert.equal(afterLatePreview.tier, 'render');
  passed('編集前プレビューのデコードが後着しても自動補正を上書きしない');

  // Use real render endpoints and delay the smaller render after the newer render is visible.
  await holdDecode('/render/' + sha, 'old');
  const oldURL = `/render/${sha}?w=512&v=${withHeldPreview.rev}&test_gate=old`;
  const newestURL = `/render/${sha}?w=1600&v=${withHeldPreview.rev}&test_gate=new`;
  await page.evaluate(url => { window.__oldAutoRender = lbView.render(url); }, oldURL);
  await page.waitForFunction(() => window.__autoDecodeGate.entered);
  await page.evaluate(url => lbView.render(url), newestURL);
  assert.equal(new URL((await currentImage()).src).searchParams.get('test_gate'), 'new');
  await releaseDecode();
  await page.evaluate(() => window.__oldAutoRender);
  assert.equal(new URL((await currentImage()).src).searchParams.get('test_gate'), 'new');
  passed('編集レンダの完了順が逆転しても最後に要求した画像を表示');

  await page.click('#edactions button[onclick="edClear()"]');
  await ready(sha, false);
  await holdEditResponse(sha);
  await page.click('#edauto');
  await page.waitForFunction(() => window.__autoResponseGate.received);
  const pending = await page.evaluate(() => window.__autoResponseGate.result);
  await show(other);
  const otherVisible = await currentImage();
  assert(!otherVisible.autoDisabled, 'pending operation on another image must not disable this image');
  await releaseEditResponse();
  await page.waitForFunction(({sha, rev}) => items.find(value => value.sha1 === sha)?.erev === rev,
    {}, {sha, rev: pending.rev});
  assert.equal((await currentImage()).sha, other, 'late edit response changed navigation');
  await show(sha, true);
  assert.equal((await currentImage()).rev, pending.rev);
  const grid = await api('/api/images?' + new URLSearchParams({source, view: 'grid', limit: '10'}));
  assert.equal(grid.items.find(value => value.sha1 === sha).erev, pending.rev);
  passed('補正要求の応答待ちに画像を送っても元の項目の版を更新・戻ると補正済み表示');

  // Reload the actual page to prove the edited appearance survives a new browser state.
  await page.reload({waitUntil: 'networkidle2', timeout: 60000});
  await page.evaluate(async source => { await go({type: 'lib', key: 'all', criteria: {source}}); }, source);
  await show(sha, true);
  const persisted = await imageStats((await currentImage()).src);
  assert(persisted.mean > before.mean + 20);
  await clearCurrent(sha);
  await verifyOriginals();
  assert.deepEqual(errors, [], 'browser JavaScript errors');
  passed('再読込後も補正済み表示・原本に戻すで復帰・全原本と無版サムネ保持・JS例外なし');
  console.log(`\n${checks} checks passed; screenshot: /tmp/fg-auto-adjust-desktop.png`);
})().catch(async error => {
  console.error(error.stack || error);
  if (page) {
    console.error('current:', await currentImage().catch(() => null));
    await page.screenshot({path: '/tmp/fg-auto-adjust-failure.png'}).catch(() => {});
  }
  process.exitCode = 1;
}).finally(async () => {
  if (page) await page.close().catch(() => {});
  if (createdShas.size) await api('/api/trash', {shas: [...createdShas]}, 'POST').catch(error => console.error('cleanup:', error.message));
  if (browser) await browser.close();
});
