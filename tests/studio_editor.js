// Inline fluent_scene iframe + gallery save/reopen/original restoration integration.
// FG_URL=http://127.0.0.1:<isolated-test-port> node tests/studio_editor.js
// Requires an exclusive disposable /tmp server; counts must remain stable during saves.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const puppeteer = require('puppeteer-core');
const BASE = process.env.FG_URL;
assert(BASE, 'FG_URL must point to a disposable test server');
const target = new URL(BASE);
assert(['127.0.0.1', 'localhost'].includes(target.hostname) && target.port !== '8790');
const nonce = crypto.randomBytes(8).toString('hex'), source = `crawl:_studio_${nonce}`;
const album = `_studio_${nonce}`, created = new Set(), retainedBytes = new Map(), errors = [];
let browser, page, albumCreated = false, nestedAlbumCreated = false, missingOriginal = '', unknownOriginalRoute = false, checks = 0, initialCounts;
const emptyOriginalRequests = [], metadataRequests = [];
const saveRequests = [];
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
async function counts() {
  const [all, scoped, albums] = await Promise.all([api('/api/images?limit=1'),
    api('/api/images?' + new URLSearchParams({source, limit: '100'})), api('/api/albums')]);
  return {all: all.total, source: scoped.total, shas: scoped.items.map(item => item.sha1).sort(),
    album: albums.find(value => value.name === album)?.count};
}
async function assertCounts(label) { assert.deepEqual(await counts(), initialCounts, label + ' changed gallery/source/album counts or membership'); }
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
async function openFilterPanel() {
  if (await page.$eval('#edstudio', panel => panel.hidden || panel.classList.contains('is-concealed') || getComputedStyle(panel).visibility === 'hidden' || getComputedStyle(panel).display === 'none'))
    await page.click('#edfilteropen');
  await page.waitForSelector('#edstudio', {visible: true});
}
async function openStudio(sha) {
  await show(sha);
  await openFilterPanel();
  await page.click('#lbstudiobtn');
  await page.waitForFunction(() => !studioSession || studioSession.ready && !$('edapply').disabled,
    {timeout: 120000});
  assert(await page.evaluate(() => !!studioSession?.ready), 'opening Studio was canceled before it became ready');
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
async function renderSignature(sha, meta) {
  meta ||= await api('/api/meta/' + sha);
  const response = await fetch(BASE + `/render/${sha}?v=${meta.edits_rev}&w=0`);
  assert(response.ok, `Cannot read current full-resolution render: ${response.status}`);
  assert.match(response.headers.get('content-type'), /image\/png/, 'private Studio render must retain PNG pixels');
  return page.evaluate(async url => __testImageSignature(await (await fetch(url)).blob()), `/render/${sha}?v=${meta.edits_rev}&w=0`);
}
async function closeStudio() {
  await page.click('#studio-close');
  await page.waitForFunction(() => !studioSession && !$('studio-frame') && !document.querySelector('dialog[open]'));
  assert(!page.frames().some(frame => frame.url().includes('/fluent-scene/edit.html')));
}
async function saveStudio() {
  const selected = await page.evaluate(() => items[lbIdx].sha1);
  const waiting = page.waitForResponse(response => response.request().method() === 'POST' &&
    /^\/api\/studio\/[a-f0-9]+\/save$/.test(new URL(response.url()).pathname), {timeout: 120000});
  await page.click('#edapply');
  const response = await waiting, saved = await response.json();
  assert(response.ok(), JSON.stringify(saved));
  assert.equal(saved.sha1, selected, 'Apply must update the selected logical image');
  await page.waitForFunction(sha => !studioSession && items[lbIdx]?.sha1 === sha &&
    $('lbimg').complete && $('lbimg').naturalWidth > 0 &&
    new URL($('lbimg').currentSrc || $('lbimg').src).pathname === '/render/' + sha, {timeout: 60000}, saved.sha1);
  assert.equal(hash(await bytes(saved.sha1)), retainedBytes.get(saved.sha1), 'Apply overwrote original image bytes');
  await assertCounts('Apply');
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
      const samples = [[.03, .04], [.47, .42], [.86, .83]].map(([x, y]) =>
        Array.from(context.getImageData(Math.floor(x * image.width), Math.floor(y * image.height), 1, 1).data));
      const result = {width: image.width, height: image.height, samples,
        hash: [...new Uint8Array(digest)].map(value => value.toString(16).padStart(2, '0')).join('')};
      image.close(); return result;
    };
  });
  await page.setRequestInterception(true);
  page.on('request', request => {
    const path = new URL(request.url()).pathname;
    if (request.method() === 'POST' && /^\/api\/studio\/[a-f0-9]+\/save$/.test(path)) saveRequests.push(path);
    if (path.startsWith('/api/meta/')) metadataRequests.push(path);
    if (unknownOriginalRoute && path.startsWith('/api/original/')) {
      emptyOriginalRequests.push(path);
      return request.respond({status: 404, body: ''});
    }
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
  assert.equal(await page.$eval('#lbstudiobtn', button => button.disabled), false);
  const legacyPage = await browser.newPage(), legacyErrors = [];
  legacyPage.on('pageerror', error => legacyErrors.push(error.message));
  await legacyPage.setRequestInterception(true);
  legacyPage.on('request', request => new URL(request.url()).pathname === '/studio-gallery.js'
    ? request.respond({status: 404, body: ''}) : request.continue());
  await legacyPage.goto(BASE, {waitUntil: 'networkidle2', timeout: 60000});
  assert.equal(await legacyPage.$eval('#lbstudiobtn', button => button.disabled), true);
  assert.equal(await legacyPage.evaluate(() => typeof studioOpen), 'undefined');
  assert.deepEqual(legacyErrors, [], 'missing editor script must not throw browser exceptions');
  await legacyPage.close();
  passed('old server without Studio script keeps its button disabled; loaded editor enables the button');
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
  // Legacy derived-image chains still need restoration, independently of the
  // current same-image Studio save contract. Build that chain with folder jobs.
  const nestedAlbum = album + '_nested';
  await api('/api/albums', {name: nestedAlbum, criteria: {filter_set: job.set_id}, folder: '', agent: {}, goal: ''});
  nestedAlbumCreated = true;
  const nestedPlan = await api('/api/filters/plan', {text: '色反転'});
  const nestedJob = await api(`/api/albums/${nestedAlbum}/filter`, {edit: nestedPlan.edit});
  await until(async () => { const status = await api('/api/filters/status');
    return status.set_id === nestedJob.set_id && !status.running && !status.committing; }, 'nested folder fixture');
  const nestedItems = (await api('/api/images?' + new URLSearchParams({filter_set: nestedJob.set_id, limit: '10'}))).items;
  assert.equal(nestedItems.length, 2);
  const nestedMeta = await Promise.all(nestedItems.map(item => api('/api/meta/' + item.sha1)));
  const nested = nestedMeta.find(meta => meta.filter_source_sha === folderOutput.sha1); assert(nested);
  for (const item of nestedItems) { created.add(item.sha1); retainedBytes.set(item.sha1, hash(await bytes(item.sha1))); }
  initialCounts = await counts(); assert.equal(initialCounts.source, 2); assert.equal(initialCounts.album, 2);
  await show(sha);
  const oldServerMeta = await api('/api/meta/' + sha), savesBeforeGate = saveRequests.length;
  await page.evaluate(sha => {
    const realFetch = window.fetch;
    window.fetch = async (input, init) => {
      const response = await realFetch(input, init);
      if (String(input) === '/api/meta/' + sha) {
        window.fetch = realFetch;
        const meta = await response.json(); delete meta.studio_save_mode;
        return new Response(JSON.stringify(meta), {status: 200, headers: {'Content-Type': 'application/json'}});
      }
      return response;
    };
  }, sha);
  await openFilterPanel(); await page.click('#lbstudiobtn');
  await page.waitForFunction(() => !studioSession && !$('studio-frame') &&
    $('edstatus').textContent.includes('更新済みサーバー'), {timeout: 60000});
  assert.equal(saveRequests.length, savesBeforeGate, 'new UI posted a save to an old server that would duplicate images');
  assert.deepEqual(await api('/api/meta/' + sha), oldServerMeta);
  await assertCounts('Old server compatibility gate');
  passed('an old server without the in-place save capability is rejected before opening or posting a save');
  await restore(folderOutput.sha1, sha);
  passed('folder pipeline baked image with no live edits restores its original and has accurate history text');
  unknownOriginalRoute = true;
  await restore(folderOutput.sha1, sha);
  unknownOriginalRoute = false;
  assert(emptyOriginalRequests.includes('/api/original/' + folderOutput.sha1));
  passed('old server empty 404 falls back through folder metadata and displays exact original pixels');

  const folderFrame = await openStudio(folderOutput.sha1);
  assert.equal(await page.evaluate(() => studioSession.source.sha1), sha);
  assert.equal(await folderFrame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter').length), 0);
  assert.equal(await page.$$eval('#edstudio #studio-frame', frames => frames.length), 1);
  assert.equal(await page.$$eval('dialog[open]', dialogs => dialogs.length), 0);
  const folderBaseline = await folderFrame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image));
  assert.equal(folderBaseline.hash, originalSignature.hash, 'folder editor started with previously baked pixels');
  await invert(folderFrame);
  await page.screenshot({path: '/tmp/fg-studio-ready.png'});
  await page.click('#studio-reset');
  await folderFrame.waitForFunction(() => __studio.nodes.every(node => node.type !== 'filter'));
  const resetPixels = await folderFrame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image));
  assert.equal(resetPixels.hash, originalSignature.hash, 'Studio Reset failed to recover the actual original pixels');
  await invert(folderFrame);
  const editedDerived = await saveStudio();
  assert.equal(editedDerived.sha1, folderOutput.sha1);
  assert.equal(editedDerived.meta.studio_edit.source_sha, sha);
  await restore(folderOutput.sha1, sha);
  await assertCounts('Legacy image with private Studio edit Reset');
  passed('folder-derived editing starts from original pixels; saving keeps its existing SHA and Original Reset still follows the real source');

  let frame = await openStudio(sha);
  assert.deepEqual(await frame.evaluate(() => __studio.gallery.size), [1280, 960]);
  assert.equal(await frame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter').length), 0);
  assert.deepEqual(await frame.evaluate(() => __studio.nodes.map(node => node.type).sort()), ['out', 'src']);
  passed('actual Studio iframe opens in the separate filter float and initializes without replacing the inline photo controls');
  await invert(frame);
  const originalThumb = await page.evaluate(async sha => __testImageSignature(await (await fetch('/thumb/' + sha)).blob()), sha);
  const saved = await saveStudio(), savedSignature = await renderSignature(sha, saved.meta);
  assert.equal(saved.sha1, sha); assert.deepEqual([savedSignature.width, savedSignature.height], [1280, 960]);
  assert.equal(saved.meta.edits.at(-1).op, 'studio');
  assert.equal(saved.meta.edits.at(-1).params.render_sha, saved.meta.studio_edit.render_sha);
  assert.equal(saved.meta.studio_edit.source_sha, sha);
  assert.equal(saved.meta.studio_edit.recipe.graph.n.filter(node => node.t === 'filter' && node.f === 'invert').length, 1);
  for (let sample = 0; sample < savedSignature.samples.length; sample++) {
    for (let channel = 0; channel < 3; channel++) assert(Math.abs(savedSignature.samples[sample][channel] -
      (255 - originalSignature.samples[sample][channel])) <= 3, 'private render does not contain the actual invert effect');
  }
  const editedThumb = await page.evaluate(async url => __testImageSignature(await (await fetch(url)).blob()),
    `/thumb/${sha}?v=${saved.meta.edits_rev}`);
  assert.notEqual(editedThumb.hash, originalThumb.hash, 'saved private render did not update its thumbnail');
  for (let sample = 0; sample < editedThumb.samples.length; sample++) for (let channel = 0; channel < 3; channel++) {
    assert(Math.abs(editedThumb.samples[sample][channel] - savedSignature.samples[sample][channel]) < 8,
      'thumbnail does not display the saved private render');
  }
  const privateImage = await fetch(BASE + '/api/meta/' + saved.meta.studio_edit.render_sha);
  assert.equal(privateImage.status, 404, 'private render was indexed as another gallery image');
  passed('Apply updates the same image with a full-resolution private PNG and thumbnail; gallery/source/album counts and original bytes remain unchanged');

  frame = await openStudio(sha);
  assert.equal(await page.evaluate(() => studioSession.source.sha1), sha, 'reopen must start from original source');
  assert.equal(await frame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter' && node.name === 'invert').length), 1);
  const reopenedSignature = await frame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image));
  assert.equal(reopenedSignature.hash, savedSignature.hash, 'reopened recipe applied its effect twice');
  const repeated = await saveStudio();
  assert.equal((await renderSignature(sha, repeated.meta)).hash, savedSignature.hash, 'repeated Apply changed identical filter pixels');
  await assertCounts('Repeated Apply');
  await page.reload({waitUntil: 'networkidle2', timeout: 60000});
  await page.waitForFunction(() => typeof studioOpen === 'function');
  // URL restoration opens the image after a 700ms boot timer. Wait for that
  // actual navigation before starting another edit on the restored photograph.
  await page.waitForFunction(sha => $('lb').classList.contains('show') && items[lbIdx]?.sha1 === sha &&
    lbMeta?.sha1 === sha && $('lbimg').complete, {timeout: 30000}, sha);
  await show(sha);
  assert.equal((await renderSignature(sha)).hash, savedSignature.hash, 'saved appearance did not survive page reload');
  assert.equal(await page.evaluate(() => items[lbIdx].sha1), sha);
  await assertCounts('Reload');
  frame = await openStudio(sha);
  assert.equal(await frame.evaluate(() => __studio.nodes.filter(node => node.type === 'filter' && node.name === 'invert').length), 1);
  assert.equal((await frame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image))).hash, savedSignature.hash);
  await closeStudio();
  passed('repeated Apply and reload/reopen keep the same image and counts; the saved recipe is restored without double application');

  frame = await openStudio(sha);
  const exposureId = await frame.evaluate(() => {
    __studio.select(__studio.nodes.find(node => node.type === 'filter' && node.name === 'invert').id);
    const node = __studio.addFilterNode('exposure', {}); __studio.applyGraph(true); __studio.select(node.id); return node.id;
  });
  await frame.waitForSelector('#params input[type="range"]', {visible: true});
  await frame.$eval('#params input[type="range"]', input => { input.value = '.4'; input.dispatchEvent(new Event('input', {bubbles: true})); });
  await frame.waitForFunction(id => __studio.nodes.find(node => node.id === id).vals.some(value => value !== 0), {}, exposureId);
  const reeditExpected = await frame.evaluate(async () => __testImageSignature((await __studio.gallery.export()).image));
  const revised = await saveStudio(), revisedSignature = await renderSignature(sha, revised.meta);
  assert.notEqual(revisedSignature.hash, savedSignature.hash, 'parameter re-edit did not affect saved pixels');
  assert.equal(revisedSignature.hash, reeditExpected.hash, 'parameter re-edit was not saved in the private PNG');
  assert.notEqual(revised.meta.edits_rev, repeated.meta.edits_rev);
  await page.click('#editpanel button[onclick="edUndo()"]');
  await page.waitForFunction(({sha, revision}) => !studioSession && items[lbIdx]?.sha1 === sha &&
    lbMeta.edits_rev !== revision && !$('edauto').disabled && $('lbimg').complete,
    {timeout: 60000}, {sha, revision: revised.meta.edits_rev});
  assert.equal((await renderSignature(sha)).hash, savedSignature.hash, 'Undo did not restore previous private render');
  await assertCounts('Undo');
  const displayedUndo = await page.evaluate(() => __testImageSignature($('lbimg')));
  for (let sample = 0; sample < displayedUndo.samples.length; sample++) for (let channel = 0; channel < 3; channel++)
    assert(Math.abs(displayedUndo.samples[sample][channel] - savedSignature.samples[sample][channel]) < 8, 'Undo main image still shows the later render');
  await page.click('#editpanel button[onclick="edClear()"]');
  await page.waitForFunction(sha => items[lbIdx]?.sha1 === sha && !lbMeta.edits.length && $('lbimg').complete &&
    new URL($('lbimg').currentSrc || $('lbimg').src).pathname === '/img/' + sha, {timeout: 60000}, sha);
  assert.deepEqual(await page.evaluate(() => __testImageSignature($('lbimg'))), originalSignature);
  await assertCounts('Original Reset');
  passed('re-edit saves under the same SHA; existing Undo restores its previous private render and Original Reset restores exact original pixels without changing counts');

  // Previously baked chains remain readable and reset to the root original.
  await api('/api/edits/' + sha, {action: 'push', edit: {op: 'adjust', params: {exposure: 0.1}}}, 'PUT');
  await restore(nested.sha1, sha);
  await assertCounts('Legacy chain Reset');
  passed('legacy nested folder derivation resolves to root original and clears its pending edit history');
  unknownOriginalRoute = true;
  await restore(nested.sha1, sha);
  unknownOriginalRoute = false;
  assert(emptyOriginalRequests.includes('/api/original/' + nested.sha1));
  passed('old server empty 404 follows every nested parent and displays exact root original pixels');
  await show(nested.sha1); missingOriginal = nested.sha1;
  metadataRequests.length = 0;
  await page.click('#editpanel button[onclick="edClear()"]');
  await page.waitForFunction(() => document.body.textContent.includes('テスト: 原本が見つかりません'));
  assert.equal(await page.evaluate(() => items[lbIdx]?.sha1), nested.sha1);
  assert(!/(元画像に戻りました|原本に戻しました)/.test(await page.$eval('#edstatus', element => element.textContent)));
  assert(!metadataRequests.includes('/api/meta/' + nested.sha1), 'JSON 404 incorrectly triggered the metadata fallback');
  missingOriginal = '';
  passed('JSON 404 preserves its error without metadata fallback, navigation or false restoration success');

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
  await page.click('#edapply');
  await page.waitForFunction(() => studioSession?.ready && !studioSession.saving && !$('edapply').disabled &&
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
  await page.click('#edapply');
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
  assert.equal(lateSha, sha, 'late save created a new image instead of updating its original target');
  await assertCounts('Late save');
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
  if (nestedAlbumCreated) await api('/api/albums/' + album + '_nested', undefined, 'DELETE').catch(() => {});
  if (created.size) await api('/api/trash', {shas: [...created]}).catch(error => console.error('cleanup:', error.message));
});
