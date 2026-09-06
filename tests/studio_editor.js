// Real fluent_scene iframe + gallery save/reopen/original restoration integration.
// FG_URL=http://127.0.0.1:<isolated-test-port> node tests/studio_editor.js
// Requires a disposable /tmp data root; fixtures and derived PNGs belong only to this run.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const puppeteer = require('puppeteer-core');
const BASE = process.env.FG_URL;
assert(BASE, 'FG_URL must point to a disposable test server');
const target = new URL(BASE);
assert(['127.0.0.1', 'localhost'].includes(target.hostname) && target.port !== '8790');
const nonce = crypto.randomBytes(8).toString('hex'), source = `crawl:_studio_${nonce}`;
const album = `_studio_${nonce}`, created = new Set(), retainedBytes = new Map(), errors = [];
let browser, page, albumCreated = false, missingOriginal = '', checks = 0;
const passed = label => { checks++; console.log(`PASS ${label}`); };
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(BASE + path, {method, headers: body === undefined ? {} : {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body)});
  const text = await response.text(); assert(response.ok, `${method} ${path}: ${response.status} ${text}`);
  return text ? JSON.parse(text) : null;
}
async function bytes(sha) {
  const response = await fetch(BASE + '/img/' + sha); assert(response.ok);
  return Buffer.from(await response.arrayBuffer());
}
async function until(probe, description, timeout = 120000) {
  const start = Date.now();
  while (Date.now() - start < timeout) { const result = await probe(); if (result) return result;
    await new Promise(resolve => setTimeout(resolve, 80)); }
  throw new Error(`Timeout: ${description}`);
}
async function show(sha) {
  const selectedSource = (await api('/api/meta/' + sha)).source;
  await page.evaluate(async ({source, sha}) => {
    if (studioSession) studioClose();
    await go({type: 'source', key: source, criteria: {source}});
    const index = items.findIndex(item => item.sha1 === sha);
    if (index < 0) throw new Error('Missing test image ' + sha);
    if ($('lb').classList.contains('show')) await lbShow(index, 0); else openLb(index);
    $('lb').classList.add('editing');
  }, {source: selectedSource, sha});
  await page.waitForFunction(sha => items[lbIdx]?.sha1 === sha && lbMeta?.sha1 === sha &&
    $('lbimg').complete && $('lbimg').naturalWidth > 0, {timeout: 60000}, sha);
}
async function openStudio(sha) {
  await show(sha);
  await page.click('#lbstudiobtn');
  await page.waitForFunction(() => studioSession?.ready && !$('studio-save').disabled,
    {timeout: 120000});
  const frame = await (await page.$('#studio-frame')).contentFrame();
  assert(frame && frame.url().includes('/fluent-scene/edit.html?gallery=1'));
  assert(await frame.evaluate(() => !!window.__studio?.gallery.loaded));
  return frame;
}
async function invert(frame) {
  await frame.evaluate(() => {
    const source = __studio.nodes.find(node => node.type === 'src');
    __studio.select(source.id); __studio.addFilterNode('invert', {}); __studio.applyGraph(true);
  });
}
async function imageSignature(sha) {
  return page.evaluate(async url => __testImageSignature(await (await fetch(url)).blob()), '/img/' + sha);
}
async function closeStudio() {
  await page.click('#studio-close');
  await page.waitForFunction(() => !studioSession && !$('studio-dialog').open && !$('studio-frame'));
  assert(!page.frames().some(frame => frame.url().includes('/fluent-scene/edit.html')));
}
async function saveStudio() {
  const waiting = page.waitForResponse(response => response.request().method() === 'POST' &&
    /^\/api\/studio\/[a-f0-9]+\/save$/.test(new URL(response.url()).pathname), {timeout: 120000});
  await page.click('#studio-save');
  const response = await waiting, saved = await response.json();
  assert(response.ok(), JSON.stringify(saved)); created.add(saved.sha1);
  await page.waitForFunction(sha => !studioSession && items[lbIdx]?.sha1 === sha &&
    $('lbimg').complete && $('lbimg').naturalWidth > 0, {timeout: 60000}, saved.sha1);
  retainedBytes.set(saved.sha1, hash(await bytes(saved.sha1)));
  return saved;
}
async function restore(derived, original) {
  const expectedPixels = await imageSignature(original);
  await show(derived);
  const label = await page.$eval('#edhist', element => element.textContent);
  assert(!label.includes('履歴なし = 原本そのまま'), `baked image mislabeled: ${label}`);
  await page.click('#editpanel button[onclick="edClear()"]');
  await page.waitForFunction(sha => items[lbIdx]?.sha1 === sha && lbMeta?.sha1 === sha &&
    !lbMeta?.edits?.length && $('lbimg').complete && $('lbimg').naturalWidth > 0 &&
    new URL($('lbimg').currentSrc || $('lbimg').src).pathname === '/img/' + sha &&
    $('edstatus').textContent.includes('元画像に戻りました'), {timeout: 60000}, original);
  const displayedPixels = await page.evaluate(() => __testImageSignature($('lbimg')));
  assert.deepEqual(displayedPixels, expectedPixels, 'visible lightbox still contains baked or resized pixels after Reset');
  assert.equal((await api('/api/edits/' + original)).edits.length, 0);
}

(async () => {
  const settings = await api('/api/settings');
  assert(settings.root.startsWith('/tmp/'), `Refusing non-test root ${settings.root}`);
  browser = await puppeteer.launch({executablePath: process.env.CHROME || '/usr/bin/google-chrome',
    headless: 'new', args: ['--no-sandbox'], defaultViewport: {width: 1600, height: 1100}});
  page = await browser.newPage(); page.on('pageerror', error => errors.push(error.message));
  await page.evaluateOnNewDocument(() => {
    window.__testImageSignature = async blob => {
      const image = await createImageBitmap(blob), canvas = document.createElement('canvas');
      canvas.width = image.width; canvas.height = image.height;
      const context = canvas.getContext('2d'); context.drawImage(image, 0, 0);
      const raw = context.getImageData(0, 0, image.width, image.height).data;
      const digest = await crypto.subtle.digest('SHA-256', raw);
      const samples = [[40, 40], [600, 400], [1100, 800]].map(([x, y]) =>
        Array.from(context.getImageData(x, y, 1, 1).data));
      const result = {width: image.width, height: image.height, samples,
        hash: [...new Uint8Array(digest)].map(value => value.toString(16).padStart(2, '0')).join('')};
      image.close(); return result;
    };
  });
  await page.setRequestInterception(true);
  page.on('request', request => {
    const path = new URL(request.url()).pathname;
    if (path === '/api/enrich/one') return request.respond({status: 409, contentType: 'application/json',
      body: JSON.stringify({detail: 'VLM disabled for isolated editor test'})});
    if (missingOriginal && path === '/api/original/' + missingOriginal) return request.respond({status: 404,
      contentType: 'application/json', body: JSON.stringify({detail: 'テスト: 原本が見つかりません'})});
    return request.continue();
  });
  page.on('response', response => {
    if (response.ok() && /^\/api\/studio\/[a-f0-9]+\/save$/.test(new URL(response.url()).pathname)) {
      response.json().then(result => { if (result.sha1) created.add(result.sha1); }).catch(() => {});
    }
  });
  await page.goto(BASE, {waitUntil: 'networkidle2', timeout: 60000});
  await page.waitForFunction(() => typeof studioOpen === 'function' && typeof go === 'function');
  await page.evaluate(() => localStorage.setItem('fg_lbseg', '0'));
  const upload = await page.evaluate(async ({source, nonce}) => {
    const form = new FormData(); form.append('source', source);
    for (let index = 0; index < 2; index++) {
      const canvas = document.createElement('canvas'); canvas.width = 1280; canvas.height = 960;
      const context = canvas.getContext('2d');
      for (let x = 0; x < canvas.width; x++) {
        context.fillStyle = `rgb(${32 + Math.round(x / 25)},${70 + index * 20},140)`;
        context.fillRect(x, 0, 1, canvas.height);
      }
      context.fillStyle = '#fafafa'; context.fillRect(1011, 100, 1, 700);
      [...nonce + index].forEach((character, x) => {
        context.fillStyle = `rgb(${character.charCodeAt(0)},20,30)`; context.fillRect(10 + x * 2, 10, 1, 1);
      });
      form.append('file', await new Promise(resolve => canvas.toBlob(resolve, 'image/png')), `studio-${index}.png`);
    }
    const response = await fetch('/api/upload', {method: 'POST', body: form});
    return {ok: response.ok, result: await response.json()};
  }, {source, nonce});
  assert(upload.ok && upload.result.added === 2, JSON.stringify(upload));
  const originals = (await api('/api/images?' + new URLSearchParams({source, limit: '10'}))).items;
  assert.equal(originals.length, 2);
  const [sha, other] = originals.map(item => item.sha1);
  for (const item of originals) { created.add(item.sha1); retainedBytes.set(item.sha1, hash(await bytes(item.sha1))); }
  const originalSignature = await imageSignature(sha);

  // Create a real folder-pipeline output whose empty edits used to make Reset a no-op.
  await api('/api/albums', {name: album, criteria: {source}, folder: '', agent: {}, goal: ''}); albumCreated = true;
  const plan = await api('/api/filters/plan', {text: 'モノクロ'});
  const job = await api(`/api/albums/${album}/filter`, {edit: plan.edit});
  await until(async () => { const status = await api('/api/filters/status');
    return status.set_id === job.set_id && !status.running && !status.committing; }, 'folder fixture');
  const folderItems = (await api('/api/images?' + new URLSearchParams({filter_set: job.set_id, limit: '10'}))).items;
  assert.equal(folderItems.length, 2);
  const folderMeta = await Promise.all(folderItems.map(item => api('/api/meta/' + item.sha1)));
  const folderOutput = folderMeta.find(meta => meta.filter_source_sha === sha); assert(folderOutput);
  for (const item of folderItems) { created.add(item.sha1); retainedBytes.set(item.sha1, hash(await bytes(item.sha1))); }
  await restore(folderOutput.sha1, sha);
  passed('folder pipeline baked image with no live edits restores its original and has accurate history text');

  const folderFrame = await openStudio(folderOutput.sha1);
  assert.equal(await page.evaluate(() => studioSession.source.sha1), sha);
  assert.equal(await folderFrame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter').length), 0);
  assert((await page.$eval('#studio-status', element => element.textContent)).includes('元画像から編集'));
  const folderBaseline = await folderFrame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image));
  assert.equal(folderBaseline.hash, originalSignature.hash, 'folder editor started with previously baked pixels');
  await invert(folderFrame);
  await page.screenshot({path: '/tmp/fg-studio-ready.png'});
  await folderFrame.click('#tbReset');
  const resetPixels = await folderFrame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image));
  assert.equal(resetPixels.hash, originalSignature.hash, 'Studio Reset failed to recover the actual original pixels');
  await closeStudio();
  passed('opening a folder-derived image starts from original pixels; the actual Studio Reset removes added filters');

  let frame = await openStudio(sha);
  assert.deepEqual(await frame.evaluate(() => __studio.gallery.size), [1280, 960]);
  assert.equal(await frame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter').length), 0);
  assert.deepEqual(await frame.evaluate(() => __studio.nodes.map(node => node.type).sort()), ['out', 'src']);
  passed('actual Studio iframe opens via gallery button and completes image init without default filters');
  await invert(frame);
  const saved = await saveStudio(), savedSignature = await imageSignature(saved.sha1);
  assert.notEqual(saved.sha1, sha); assert.deepEqual([savedSignature.width, savedSignature.height], [1280, 960]);
  assert(!saved.meta.edits?.length); assert.equal(saved.meta.studio.source_sha, sha);
  assert.equal(saved.meta.studio.recipe.graph.n.filter(node => node.t === 'filter' && node.f === 'invert').length, 1);
  for (let sample = 0; sample < savedSignature.samples.length; sample++) {
    for (let channel = 0; channel < 3; channel++) assert(Math.abs(savedSignature.samples[sample][channel] -
      (255 - originalSignature.samples[sample][channel])) <= 3, 'saved pixels do not contain the actual invert effect');
  }
  passed('save adds a full-resolution baked PNG with real filtered pixels and a source-linked recipe');
  frame = await openStudio(saved.sha1);
  assert.equal(await page.evaluate(() => studioSession.source.sha1), sha, 'reopen must start from original source');
  assert.equal(await frame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter' && node.name === 'invert').length), 1);
  const reopenedSignature = await frame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image));
  assert.equal(reopenedSignature.hash, savedSignature.hash, 'reopened recipe applied its effect twice');
  await closeStudio();
  passed('reopen restores recipe on original pixels without double application; close removes iframe and its render loop');

  // A second saved derivation proves Reset follows the whole chain, not one parent.
  const form = new FormData(); form.append('image', new Blob([await bytes(saved.sha1)], {type: 'image/png'}), 'nested.png');
  form.append('recipe', JSON.stringify({...saved.meta.studio.recipe,
    source_edits_rev: (await api('/api/meta/' + saved.sha1)).edits_rev}));
  const nestedResponse = await fetch(BASE + `/api/studio/${saved.sha1}/save`, {method: 'POST', body: form});
  const nested = await nestedResponse.json(); assert(nestedResponse.ok, JSON.stringify(nested)); created.add(nested.sha1);
  retainedBytes.set(nested.sha1, hash(await bytes(nested.sha1)));
  await api('/api/edits/' + sha, {action: 'push', edit: {op: 'adjust', params: {exposure: 0.1}}}, 'PUT');
  await restore(nested.sha1, sha);
  passed('nested baked derivation resolves to root original and clears its pending edit history');
  await show(nested.sha1); missingOriginal = nested.sha1;
  await page.click('#editpanel button[onclick="edClear()"]');
  await page.waitForFunction(() => document.body.textContent.includes('テスト: 原本が見つかりません'));
  assert.equal(await page.evaluate(() => items[lbIdx]?.sha1), nested.sha1);
  assert(!(await page.$eval('#edstatus', element => element.textContent)).includes('原本に戻しました'));
  missingOriginal = '';
  passed('missing original reports error without navigating or claiming successful restoration');

  frame = await openStudio(sha); await invert(frame);
  await page.evaluate(() => {
    window.__studioRealFetch = window.fetch;
    window.fetch = async (input, init) => {
      if (init?.method === 'POST' && String(input).startsWith('/api/studio/')) {
        window.fetch = window.__studioRealFetch;
        return new Response(JSON.stringify({detail: 'テスト: 保存に失敗しました'}), {status: 500,
          headers: {'Content-Type': 'application/json'}});
      }
      return window.__studioRealFetch(input, init);
    };
  });
  await page.click('#studio-save');
  await page.waitForFunction(() => studioSession?.ready && !studioSession.saving && !$('studio-save').disabled &&
    $('studio-status').textContent.includes('テスト: 保存に失敗しました'), {timeout: 120000});
  assert.equal(await frame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter' && node.name === 'invert').length), 1);
  passed('failed save retains the real editor, current graph and enabled retry button');
  await page.evaluate(() => {
    const realFetch = window.fetch;
    window.__studioSaveGate = {realFetch, received: false, release: null, state: studioSession};
    window.fetch = async (input, init) => {
      const response = await realFetch(input, init), gate = window.__studioSaveGate;
      if (!gate.received && init?.method === 'POST' && String(input).startsWith('/api/studio/')) {
        gate.saved = await response.clone().json(); gate.received = true;
        await new Promise(resolve => { gate.release = resolve; });
      }
      return response;
    };
  });
  await page.click('#studio-save');
  await page.waitForFunction(() => window.__studioSaveGate.received, {timeout: 120000});
  const lateSha = await page.evaluate(() => window.__studioSaveGate.saved.sha1); created.add(lateSha);
  await closeStudio();
  frame = await openStudio(other);
  const newSession = await page.evaluate(() => studioSession.session);
  await page.evaluate(() => { const gate = window.__studioSaveGate; window.fetch = gate.realFetch; gate.release(); });
  await page.waitForFunction(() => !window.__studioSaveGate.state.saving, {timeout: 30000});
  assert.equal(await page.evaluate(() => studioSession.session), newSession);
  assert.equal(await page.evaluate(() => items[lbIdx]?.sha1), other);
  assert.equal(await page.evaluate(() => studioSession.source.sha1), other);
  await closeStudio();
  passed('late save response after close leaves a different image and its newly opened editor untouched');
  for (const [imageSha, expected] of retainedBytes) assert.equal(hash(await bytes(imageSha)), expected, imageSha + ' bytes changed');
  assert.deepEqual(errors, [], 'unexpected browser JavaScript exceptions');
  passed('all original and baked image bytes remain unchanged; browser has no JavaScript exceptions');
  console.log(`${checks} checks passed`);
})().catch(async error => {
  console.error(error.stack || error); process.exitCode = 1;
  if (page) {
    console.error('state:', await page.evaluate(() => ({selected: items[lbIdx]?.sha1, meta: lbMeta?.sha1,
      status: document.getElementById('studio-status')?.textContent, ready: studioSession?.ready,
      edstatus: document.getElementById('edstatus')?.textContent})).catch(() => null));
    await page.screenshot({path: '/tmp/fg-studio-editor-failure.png'}).catch(() => {});
  }
}).finally(async () => {
  if (page) await page.evaluate(() => {
    const gate = window.__studioSaveGate;
    if (gate?.release) { window.fetch = gate.realFetch; gate.release(); }
    if (studioSession) studioClose();
  }).catch(() => {});
  if (browser) await browser.close();
  if (albumCreated) await api('/api/albums/' + album, undefined, 'DELETE').catch(() => {});
  if (created.size) await api('/api/trash', {shas: [...created]}).catch(error => console.error('cleanup:', error.message));
});
