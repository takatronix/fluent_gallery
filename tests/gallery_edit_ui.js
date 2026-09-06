// Photo adjustment and fluent_scene filters share the existing gallery panel and main preview.
// FG_URL=http://127.0.0.1:<disposable-test-port> node tests/gallery_edit_ui.js
// Only the unique fixtures created by this run are edited; original files must stay byte-identical.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const puppeteer = require('puppeteer-core');
const BASE = process.env.FG_URL;
assert(BASE, 'Set FG_URL to an isolated test server');
const target = new URL(BASE);
assert(['127.0.0.1', 'localhost'].includes(target.hostname) && target.port !== '8790');
const nonce = crypto.randomBytes(8).toString('hex'), source = `crawl:_gallery_edit_${nonce}`;
const album = `_gallery_edit_${nonce}`;
// Keep this fixture below the desktop/mobile CPU preview caps so preview and
// export have identical pixel dimensions; the Studio suite covers large exports.
const FIXTURE_WIDTH = 480, FIXTURE_HEIGHT = 360;
const created = new Set(), originals = new Map(), browserErrors = [], networkErrors = [];
let browser, page, checks = 0, albumCreated = false, initialCounts;
const pass = label => { checks++; console.log(`PASS ${label}`); };
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
async function waitForImage(sha, edited = false) {
  await page.waitForFunction(({sha, edited}) => {
    const image = $('lbimg');
    if ($('edstatus').textContent.includes('画像を表示できませんでした')) throw new Error($('edstatus').textContent);
    return items[lbIdx]?.sha1 === sha && lbMeta?.sha1 === sha && image.complete && image.naturalWidth > 0 &&
      $('editpanel').getAttribute('aria-busy') !== 'true' &&
      new URL(image.currentSrc || image.src).pathname === (edited ? '/render/' : '/img/') + sha;
  }, {timeout: 60000}, {sha, edited});
}
async function assertGalleryRemainsVisible() {
  const state = await page.evaluate(() => {
    const preview = $('lbimg').getBoundingClientRect(), panel = $('editpanel').getBoundingClientRect();
    return {lightbox: $('lb').classList.contains('show'), editing: $('lb').classList.contains('editing'),
      preview: {width: preview.width, height: preview.height, display: getComputedStyle($('lbimg')).display},
      panel: {width: panel.width, height: panel.height, display: getComputedStyle($('editpanel')).display},
      modal: [...document.querySelectorAll('dialog[open]')].some(dialog => dialog.matches(':modal'))};
  });
  assert(state.lightbox && state.editing, JSON.stringify(state));
  assert(state.preview.width > 100 && state.preview.height > 100 && state.preview.display !== 'none', JSON.stringify(state));
  assert(state.panel.width > 100 && state.panel.height > 80 && state.panel.display !== 'none', JSON.stringify(state));
  assert(!state.modal, 'editing must not open a separate full-screen Studio modal');
}
async function signature(selector = '#lbimg') {
  return page.evaluate(async selector => {
    let element;
    if (selector === 'draft' || selector === 'pending') {
      const state = studioSession;
      const edits = structuredClone(state.photoEdits);
      if (selector === 'pending') {
        const params = edVals();
        if (Object.keys(params).length) edits.push({op: 'adjust', params});
      }
      const response = await fetch(`/api/studio/${state.source.sha1}/preview`, {method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({edits, source_edits_rev: state.source.edits_rev})});
      if (!response.ok) throw new Error('Draft preview failed: ' + response.status);
      element = await response.blob();
    } else if (selector.startsWith('/')) {
      const response = await fetch(selector);
      if (!response.ok) throw new Error('Saved render failed: ' + response.status);
      element = await response.blob();
    } else element = document.querySelector(selector);
    if (!element) throw new Error('No main preview ' + selector);
    const image = await createImageBitmap(element);
    const canvas = document.createElement('canvas'); canvas.width = image.width; canvas.height = image.height;
    const context = canvas.getContext('2d'); context.drawImage(image, 0, 0);
    const data = context.getImageData(0, 0, canvas.width, canvas.height).data;
    const digest = await crypto.subtle.digest('SHA-256', data);
    const samples = [[.1, .1], [.5, .5], [.85, .75]].map(([x, y]) =>
      Array.from(context.getImageData(Math.floor(x * image.width), Math.floor(y * image.height), 1, 1).data));
    const output = {width: image.width, height: image.height, samples,
      hash: [...new Uint8Array(digest)].map(value => value.toString(16).padStart(2, '0')).join('')};
    image.close(); return output;
  }, selector);
}
async function show(sha) {
  const meta = await api('/api/meta/' + sha);
  await page.evaluate(async ({sha, source}) => {
    await go({type: 'source', key: source, criteria: {source}});
    const index = items.findIndex(item => item.sha1 === sha);
    if (index < 0) throw new Error('Missing unique fixture ' + sha);
    if ($('lb').classList.contains('show')) await lbShow(index, 0); else openLb(index);
  }, {sha, source: meta.source});
  await waitForImage(sha, !!meta.edits?.length);
}
async function originalEditingControls(sha) {
  await show(sha);
  const originalPixels = await signature();
  assert.match(await page.$eval('#edbtn', button => button.textContent), /編集/);
  if (await page.evaluate(() => $('lb').classList.contains('editing'))) await page.click('#edbtn');
  await page.click('#edbtn');
  await page.waitForSelector('#editpanel', {visible: true});
  const expected = ['#ed_exposure', '#ed_contrast', '#ed_saturation', '#ed_temperature', '#edauto', '#cropbtn',
    '#edactions button[onclick="edUndo()"]', '#edactions button[onclick="edClear()"]',
    '#edactions button[onclick="edApplyAdjust()"]'];
  for (const selector of expected) await page.waitForSelector(selector, {visible: true});
  await assertGalleryRemainsVisible();
  await page.evaluate(() => document.activeElement.blur()); await page.keyboard.press('e');
  await page.waitForFunction(() => !$('lb').classList.contains('editing'));
  await page.keyboard.press('e'); await page.waitForFunction(() => $('lb').classList.contains('editing'));
  for (const selector of expected) await page.waitForSelector(selector, {visible: true});
  await assertGalleryRemainsVisible();
  pass('Edit button and E open the same original controls alongside the gallery main image');

  await page.$eval('#ed_exposure', input => { input.value = '20'; input.dispatchEvent(new Event('input', {bubbles: true})); });
  assert((await page.$eval('#lbimg', image => image.style.filter)).includes('brightness('));
  await page.click('#edactions button[onclick="edApplyAdjust()"]');
  await waitForImage(sha, true);
  const adjusted = await signature();
  assert.notEqual(adjusted.hash, originalPixels.hash, 'exposure applied only to a detached editor preview');
  assert(adjusted.samples[1][0] > originalPixels.samples[1][0] + 5, 'main image did not brighten');
  assert.equal((await api('/api/edits/' + sha)).edits.at(-1).op, 'adjust');
  await assertGalleryRemainsVisible();
  await page.click('#edactions button[onclick="edUndo()"]'); await waitForImage(sha);
  assert.deepEqual(await signature(), originalPixels, 'undo did not restore displayed original pixels');
  pass('existing exposure slider applies to the actual main preview; Undo restores original pixels');
  await page.click('#edauto'); await waitForImage(sha, true);
  assert.equal((await api('/api/edits/' + sha)).edits.at(-1).op, 'auto');
  await assertGalleryRemainsVisible();
  await page.click('#edactions button[onclick="edUndo()"]'); await waitForImage(sha);
  assert.deepEqual(await signature(), originalPixels);
  pass('original Auto button still adjusts the selected gallery image and Undo restores it');

  await page.click('#cropbtn'); assert(await page.evaluate(() => cropMode));
  const rect = await page.$eval('#lbimg', image => { const r = image.getBoundingClientRect();
    return {left: r.left, top: r.top, width: r.width, height: r.height}; });
  await page.mouse.move(rect.left + rect.width * .25, rect.top + rect.height * .25);
  await page.mouse.down();
  await page.mouse.move(rect.left + rect.width * .75, rect.top + rect.height * .75, {steps: 5});
  await page.mouse.up(); await waitForImage(sha, true);
  assert.equal((await api('/api/edits/' + sha)).edits.at(-1).op, 'crop');
  assert(await page.$eval('#lbimg', (image, width) => image.naturalWidth < width, originalPixels.width));
  await page.click('#edactions button[onclick="edUndo()"]'); await waitForImage(sha);
  assert.deepEqual(await signature(), originalPixels);
  pass('original Crop tool edits the actual selected image and remains undoable in the same panel');
  return originalPixels;
}

async function filterIdle() {
  await page.waitForFunction(() => studioSession?.ready && !studioIsBusy() &&
    $('lbimg').complete && $('lbimg').naturalWidth > 0, {timeout: 120000});
}
async function pendingSlidersIdle() {
  await filterIdle();
  await page.waitForFunction(() => {
    const state = studioSession;
    return state && !state.adjustTask && !state.adjustTimer &&
      state.previewInputKey === studioInputKey(state, studioPreviewEdits(state)) &&
      state.paintedAdjustVersion === state.adjustVersion;
  }, {timeout: 60000});
}
function nearSamples(actual, expected, tolerance = 3) {
  return actual.every((pixel, index) => pixel.slice(0, 3).every((value, channel) =>
    Math.abs(value - expected[index][channel]) <= tolerance));
}
async function waitSamples(expected, label, tolerance = 3) {
  const start = Date.now(); let latest;
  while (Date.now() - start < 30000) {
    latest = await signature();
    if (nearSamples(latest.samples, expected, tolerance)) return latest;
    await new Promise(resolve => setTimeout(resolve, 80));
  }
  throw new Error(`${label}: actual=${JSON.stringify(latest)} expected=${JSON.stringify(expected)}`);
}
async function openFilterPanel() {
  if (await page.$eval('#edstudio', panel => panel.hidden || panel.classList.contains('is-concealed') || getComputedStyle(panel).visibility === 'hidden' || getComputedStyle(panel).display === 'none'))
    await page.click('#edfilteropen');
  await page.waitForSelector('#edstudio', {visible: true});
}
async function languageInvert() {
  await openFilterPanel();
  await page.$eval('#studio-text', input => { input.value = '色反転'; });
  await page.click('#studio-generate'); await filterIdle();
  const frame = await (await page.$('#studio-frame')).contentFrame();
  await frame.waitForFunction(() => __studio.nodes.some(node => node.type === 'filter' && node.name === 'invert'));
  return frame;
}
async function integratedFiltersAndMobile(sha, originalPixels) {
  assert.equal(await page.$$eval('#lbbar #lbstudiobtn', elements => elements.length), 0);
  for (const selector of ['#studio-text', '#studio-generate', '#studio-random', '#studio-reset', '#lbstudiobtn']) {
    assert.equal(await page.$$eval('#edstudio ' + selector, elements => elements.length), 1);
  }
  const invertedSamples = originalPixels.samples.map(pixel => pixel.map((value, index) => index < 3 ? 255 - value : value));
  let frame = await languageInvert();
  assert(frame.url().includes('mode=inline'));
  assert.equal(await page.$$eval('#edstudio #studio-frame', elements => elements.length), 1);
  assert.equal(await page.$$eval('#editpanel #edstudio', elements => elements.length), 0);
  assert.equal(await page.$$eval('#editpanel #edfilteropen', elements => elements.length), 1);
  await waitSamples(invertedSamples, 'language filter did not change the actual gallery preview');
  await assertGalleryRemainsVisible();
  assert.equal((await api('/api/edits/' + sha)).edits.length, 0, 'filter draft mutated original metadata');
  pass('language filters float separately from the original inline photo controls and update the same main image');

  // Exercise the real Studio inspector slider, then the gallery's shared Undo.
  const exposureId = await frame.evaluate(() => {
    __studio.select(__studio.nodes.find(node => node.type === 'filter' && node.name === 'invert').id);
    const node = __studio.addFilterNode('exposure', {}); __studio.applyGraph(true); __studio.select(node.id);
    return node.id;
  });
  await page.waitForFunction(id => studioSession.graph?.n?.some(node => node.i === id), {}, exposureId);
  await frame.waitForSelector('#params input[type="range"]', {visible: true});
  await frame.$eval('#params input[type="range"]', input => {
    const current = +input.value, low = +input.min, high = +input.max;
    input.value = String(Math.max(low, current - (high - low) / 8));
    input.dispatchEvent(new Event('input', {bubbles: true}));
  });
  await page.waitForFunction(() => studioSession.history.length > 1);
  await frame.waitForFunction(id => __studio.nodes.find(node => node.id === id).vals.some(value => value !== 0), {}, exposureId);
  const before = Date.now();
  while (Date.now() - before < 10000) {
    if (!nearSamples((await signature()).samples, invertedSamples, 8)) break;
    await new Promise(resolve => setTimeout(resolve, 80));
  }
  assert(!nearSamples((await signature()).samples, invertedSamples, 8), 'individual parameter changed only inside iframe');
  await page.screenshot({path: '/tmp/fg-restored-edit-ui.png'});
  await page.click('#edactions button[onclick="edUndo()"]'); await filterIdle();
  await waitSamples(invertedSamples, 'shared Undo did not undo the filter parameter');
  pass('individual native filter parameters affect the main preview and the original Undo button reverses them');

  await page.click('#edauto'); await filterIdle();
  assert.equal(await page.evaluate(() => studioSession.photoEdits.at(-1).op), 'auto');
  const photoDraft = await page.evaluate(() => structuredClone(studioSession.photoEdits));
  const autoHistoryLength = await page.evaluate(() => studioSession.history.length);
  await page.evaluate(() => edAuto()); await filterIdle();
  assert.deepEqual(await page.evaluate(() => studioSession.photoEdits), photoDraft,
    'repeated Auto changed the identical photo draft');
  assert.equal(await page.evaluate(() => studioSession.history.length), autoHistoryLength,
    'repeated Auto created an identical Undo snapshot');
  const adjustedInput = await signature('draft');
  await page.click('#studio-reset'); await filterIdle();
  await frame.waitForFunction(() => __studio.nodes.every(node => node.type !== 'filter'));
  assert.deepEqual(await page.evaluate(() => studioSession.photoEdits), photoDraft, 'filter-only Reset discarded photo adjustments');
  await waitSamples(adjustedInput.samples, 'filter Reset did not reveal photo-adjusted pixels');
  await page.$eval('#ed_exposure', input => { input.value = '20'; input.dispatchEvent(new Event('input', {bubbles: true})); });
  const pendingResetInput = await signature('pending');
  await waitSamples(pendingResetInput.samples, 'pending photo slider did not refresh the actual native preview');
  await pendingSlidersIdle();
  await page.click('#edactions button[onclick="edClear()"]'); await filterIdle();
  assert.equal(await page.evaluate(() => studioSession.photoEdits.length), 0);
  assert.deepEqual(await page.evaluate(() => edVals()), {}, 'original Reset retained pending photo sliders');
  assert.equal(await page.$eval('#lbimg', image => image.style.filter), '');
  await waitSamples(originalPixels.samples, 'original Reset did not clear both photo and filter drafts');
  assert.equal((await api('/api/edits/' + sha)).edits.length, 0);
  pass('Auto works with filters as a draft; filter Reset preserves photo adjustments and original Reset clears both');

  frame = await languageInvert(); await waitSamples(invertedSamples, 'reapplied filter');
  await page.click('#edauto'); await filterIdle();
  const beforeSliders = await page.evaluate(() => ({photoEdits: structuredClone(studioSession.photoEdits),
    history: structuredClone(studioSession.history)}));
  const beforeSliderPixels = await signature();
  // Several slider positions represent one pending adjustment, not several
  // committed edits, and must run before the native invert in the real preview.
  await page.$eval('#ed_exposure', input => {
    for (const value of ['10', '30', '20']) {
      input.value = value; input.dispatchEvent(new Event('input', {bubbles: true}));
    }
  });
  const pendingInput = await signature('pending');
  const pendingInverted = pendingInput.samples.map(pixel => pixel.map((value, index) => index < 3 ? 255 - value : value));
  await waitSamples(pendingInverted, 'exposure must be rendered before native invert, without a CSS approximation');
  await pendingSlidersIdle();
  assert.equal(await page.$eval('#lbimg', image => image.style.filter), '', 'active native preview retained a CSS photo filter');
  const visibleBeforeSave = await signature();
  assert.deepEqual([visibleBeforeSave.width, visibleBeforeSave.height], [FIXTURE_WIDTH, FIXTURE_HEIGHT]);
  assert.notEqual(visibleBeforeSave.hash, beforeSliderPixels.hash, 'pending exposure did not change actual main image pixels');
  assert.deepEqual(await page.evaluate(() => ({photoEdits: structuredClone(studioSession.photoEdits),
    history: structuredClone(studioSession.history)})), beforeSliders, 'pending slider previews committed duplicate edits or Undo snapshots');
  const savedResponse = page.waitForResponse(response => response.ok() && response.request().method() === 'POST' &&
    /^\/api\/studio\/[a-f0-9]+\/save$/.test(new URL(response.url()).pathname), {timeout: 120000});
  await page.click('#edapply');
  const saved = await (await savedResponse).json(); created.add(saved.sha1);
  await page.waitForFunction(sha => !studioSession && items[lbIdx]?.sha1 === sha, {timeout: 60000}, saved.sha1);
  await waitForImage(saved.sha1, true);
  assert.equal(saved.sha1, sha, 'Apply created another gallery image');
  assert.deepEqual(await counts(), initialCounts, 'Apply changed gallery, source, or album membership');
  assert.equal(saved.meta.studio_edit.source_sha, sha);
  assert.deepEqual([saved.meta.w, saved.meta.h], [FIXTURE_WIDTH, FIXTURE_HEIGHT]);
  assert.equal(saved.meta.edits.at(-1).op, 'studio');
  assert.equal(saved.meta.edits.at(-1).params.render_sha, saved.meta.studio_edit.render_sha);
  assert(saved.meta.studio_edit.recipe.photo_edits.some(edit => edit.op === 'auto'));
  const savedAdjustments = saved.meta.studio_edit.recipe.photo_edits.filter(edit => edit.op === 'adjust');
  assert.equal(savedAdjustments.length, 1, 'pending exposure was committed more than once');
  assert.equal(savedAdjustments[0].params.exposure, .2);
  assert(saved.meta.studio_edit.recipe.graph.n.some(node => node.t === 'filter' && node.f === 'invert'));
  assert.deepEqual(await signature(`/render/${sha}?w=0&v=${saved.meta.edits_rev}`), visibleBeforeSave,
    'saved private PNG pixels differ from the actual main preview shown before Apply');
  assert(!nearSamples((await signature()).samples, originalPixels.samples, 8));
  assert.equal((await api('/api/edits/' + sha)).edits.at(-1).op, 'studio');
  await assertGalleryRemainsVisible();
  pass('pending photo sliders render before native filters; Apply saves exact private PNG pixels under the same image with unchanged counts');
  await page.screenshot({path: '/tmp/fg-gallery-edit-saved.png'});

  await page.click('#edactions button[onclick="edClear()"]'); await waitForImage(sha);
  assert.deepEqual(await signature(), originalPixels, 'Reset after saving did not restore the same original image');
  assert.deepEqual(await counts(), initialCounts, 'Reset changed gallery, source, or album membership');

  await show(sha); await page.setViewport({width: 390, height: 844});
  if (!await page.$eval('#edstudio', panel => panel.hidden || panel.classList.contains('is-concealed'))) await page.click('#filter-panel-close');
  const controls = ['#ed_exposure', '#ed_contrast', '#ed_saturation', '#ed_temperature', '#edauto', '#cropbtn',
    '#edactions button[onclick="edUndo()"]', '#edactions button[onclick="edClear()"]', '#edapply',
    '#edfilteropen'];
  for (const selector of controls) {
    await page.$eval(selector, element => element.scrollIntoView({block: 'center', inline: 'nearest'}));
    const reachable = await page.$eval(selector, element => {
      const r = element.getBoundingClientRect(), style = getComputedStyle(element);
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {visible: style.display !== 'none' && r.width > 0 && r.height > 0,
        inside: r.top >= 0 && r.bottom <= innerHeight + 1 && r.left >= 0 && r.right <= innerWidth + 1,
        clickable: hit === element || element.contains(hit)};
    });
    assert(reachable.visible && reachable.inside && reachable.clickable, selector + ': ' + JSON.stringify(reachable));
  }
  frame = await languageInvert(); await waitSamples(invertedSamples, 'mobile main preview');
  for (const selector of ['#studio-text', '#studio-generate', '#studio-random', '#studio-reset', '#lbstudiobtn']) {
    await page.$eval(selector, element => element.scrollIntoView({block: 'nearest'}));
    assert(await page.$eval(selector, element => { const r = element.getBoundingClientRect();
      const hit = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
      return r.top >= 0 && r.bottom <= innerHeight + 1 && (hit === element || element.contains(hit)); }), selector + ' not reachable in filter float');
  }
  await assertGalleryRemainsVisible();
  await page.screenshot({path: '/tmp/fg-gallery-edit-mobile.png'});
  // Cancel a pending slider in the same event turn, before its debounce fires.
  await page.evaluate(() => {
    const input = $('ed_exposure'); input.value = '30'; input.dispatchEvent(new Event('input', {bubbles: true}));
    $('studio-close').click();
  });
  await page.waitForFunction(() => !studioSession && !$('studio-frame'));
  await waitForImage(sha); assert.deepEqual(await signature(), originalPixels);
  assert.deepEqual(await page.evaluate(() => edVals()), {}, 'Cancel retained pending photo sliders');
  assert.equal(await page.$eval('#lbimg', image => image.style.filter), '');
  pass('mobile inline photo controls and floating filter controls remain reachable; Cancel restores the selected original');
}

(async () => {
  const settings = await api('/api/settings'); assert(settings.root.startsWith('/tmp/'), 'Disposable /tmp root required');
  browser = await puppeteer.launch({executablePath: process.env.CHROME || '/usr/bin/google-chrome',
    headless: 'new', args: ['--no-sandbox'], defaultViewport: {width: 1500, height: 1100}});
  page = await browser.newPage(); page.on('pageerror', error => browserErrors.push(error.message));
  page.on('requestfailed', request => networkErrors.push({url: request.url().slice(0, 180), error: request.failure()?.errorText}));
  page.on('response', response => { if (!response.ok() && !response.url().includes('/api/enrich/one'))
    networkErrors.push({url: response.url().slice(0, 180), status: response.status()}); });
  await page.setRequestInterception(true);
  page.on('request', request => new URL(request.url()).pathname === '/api/enrich/one'
    ? request.respond({status: 409, contentType: 'application/json', body: '{"detail":"VLM disabled for isolated UI test"}'})
    : request.continue());
  page.on('response', response => {
    if (response.ok() && /^\/api\/studio\/[a-f0-9]+\/save$/.test(new URL(response.url()).pathname)) {
      response.json().then(saved => { if (saved.sha1) created.add(saved.sha1); }).catch(() => {});
    }
  });
  await page.goto(BASE, {waitUntil: 'networkidle2', timeout: 60000});
  await page.waitForFunction(() => typeof edToggle === 'function');
  const upload = await page.evaluate(async ({source, nonce, width, height}) => {
    const canvas = document.createElement('canvas'); canvas.width = width; canvas.height = height;
    const context = canvas.getContext('2d');
    for (let x = 0; x < canvas.width; x++) {
      context.fillStyle = `rgb(${25 + Math.round(x / 32)},75,140)`; context.fillRect(x, 0, 1, canvas.height);
    }
    [...nonce].forEach((character, index) => { context.fillStyle = `rgb(${character.charCodeAt(0)},20,20)`;
      context.fillRect(10 + index * 2, 10, 1, 1); });
    const form = new FormData(); form.append('source', source);
    form.append('file', await new Promise(resolve => canvas.toBlob(resolve, 'image/png')), 'gallery-edit.png');
    context.fillStyle = '#ff0055'; context.fillRect(12, 11, 1, 1);
    form.append('file', await new Promise(resolve => canvas.toBlob(resolve, 'image/png')), 'fresh-gallery-edit.png');
    const response = await fetch('/api/upload', {method: 'POST', body: form});
    return {ok: response.ok, result: await response.json()};
  }, {source, nonce, width: FIXTURE_WIDTH, height: FIXTURE_HEIGHT});
  assert(upload.ok && upload.result.added === 2, JSON.stringify(upload));
  const listing = await api('/api/images?' + new URLSearchParams({source, limit: '10'}));
  assert.equal(listing.items.length, 2);
  const [sha, fresh] = listing.items.map(item => item.sha1);
  for (const image of [sha, fresh]) { created.add(image); originals.set(image, hash(await bytes(image))); }
  await api('/api/albums', {name: album, criteria: {source}, folder: '', agent: {}, goal: ''}); albumCreated = true;
  initialCounts = await counts(); assert.equal(initialCounts.source, 2); assert.equal(initialCounts.album, 2);
  await show(fresh);
  assert.equal(await page.evaluate(sha => edStates.has(sha), fresh), false, 'fresh image unexpectedly has an edit state');
  if (!await page.evaluate(() => $('lb').classList.contains('editing'))) await page.click('#edbtn');
  await openFilterPanel(); await page.click('#lbstudiobtn'); await filterIdle();
  assert.equal(await page.evaluate(() => studioSession.source.sha1), fresh);
  await assertGalleryRemainsVisible();
  await page.click('#studio-close'); await page.waitForFunction(() => !studioSession);
  await waitForImage(fresh);
  if (!await page.$eval('#edstudio', panel => panel.hidden)) await page.click('#filter-panel-close');
  assert.equal((await api('/api/edits/' + fresh)).edits.length, 0);
  pass('a fresh image with no prior edit state opens inline immediately and Cancel preserves the untouched photo');
  const originalPixels = await originalEditingControls(sha);
  await integratedFiltersAndMobile(sha, originalPixels);
  for (const [image, expected] of originals) assert.equal(hash(await bytes(image)), expected, 'original file changed');
  assert.deepEqual(browserErrors, [], 'browser JavaScript errors');
  pass('original image bytes remain unchanged and the integrated editor has no JavaScript exceptions');
  console.log(`${checks} checks passed`);
})().catch(async error => {
  console.error(error.stack || error); process.exitCode = 1;
  if (page) {
    console.error('display state:', await page.evaluate(() => ({sha: items[lbIdx]?.sha1, meta: lbMeta?.sha1,
      src: $('lbimg').src, currentSrc: $('lbimg').currentSrc, complete: $('lbimg').complete,
      naturalWidth: $('lbimg').naturalWidth, revision: lbMeta?.edits_rev, status: $('edstatus').textContent,
      busy: $('editpanel').getAttribute('aria-busy')})).catch(() => null));
    console.error('network errors:', networkErrors);
    await page.screenshot({path: '/tmp/fg-gallery-edit-failure.png'}).catch(() => {});
  }
}).finally(async () => {
  if (browser) await browser.close();
  if (albumCreated) await api('/api/albums/' + album, undefined, 'DELETE').catch(() => {});
  if (created.size) await api('/api/trash', {shas: [...created]}).catch(error => console.error('cleanup:', error.message));
});
