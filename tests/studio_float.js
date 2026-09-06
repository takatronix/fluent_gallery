// Basic photo controls stay inline; only the language/native filter panel floats.
// FG_URL=http://127.0.0.1:<exclusive-disposable-port> node tests/studio_float.js
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const puppeteer = require('puppeteer-core');
const BASE = process.env.FG_URL;
assert(BASE, 'Set FG_URL to a disposable server');
const target = new URL(BASE);
assert(['127.0.0.1', 'localhost'].includes(target.hostname) && target.port !== '8790');
const source = 'crawl:_studio_float_' + crypto.randomBytes(8).toString('hex');
let browser, page, sha, checks = 0, allowFixtureSave = false;
const errors = [], forbidden = [], networkErrors = [];
const pass = label => { checks++; console.log('PASS ' + label); };
const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
async function api(path, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(BASE + path, {method, headers: body === undefined ? {} : {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body)});
  const text = await response.text(); assert(response.ok, `${method} ${path}: ${response.status} ${text}`);
  return text ? JSON.parse(text) : null;
}
async function photoBox() {
  return page.$eval('#lbimg', image => { const r = image.getBoundingClientRect();
    return {x: r.x, y: r.y, width: r.width, height: r.height}; });
}
async function stablePhoto() {
  let previous, stable = 0;
  for (let step = 0; step < 80; step++) {
    const current = await photoBox();
    if (previous && Object.keys(current).every(key => Math.abs(current[key] - previous[key]) < .05)) stable++;
    else stable = 0;
    if (stable >= 4) return current;
    previous = current;
    await new Promise(resolve => setTimeout(resolve, 30));
  }
  throw new Error('Photograph layout never stabilized');
}
function sameBox(actual, expected, label) {
  for (const key of Object.keys(expected)) assert(Math.abs(actual[key] - expected[key]) < 1.1,
    `${label} changed photograph ${key}: ${JSON.stringify(actual)} versus ${JSON.stringify(expected)}`);
}
async function panelBox() {
  // The panel clamps its remembered position on the next animation frame.
  await page.waitForFunction(() => {
    const panel = $('edstudio'), r = panel.getBoundingClientRect();
    return getComputedStyle(panel).display !== 'none' && r.width > 0 && r.height > 0 &&
      r.left >= -1 && r.top >= -1 && r.right <= innerWidth + 1 && r.bottom <= innerHeight + 1;
  }, {timeout: 2000});
  const state = await page.$eval('#edstudio', panel => {
    const r = panel.getBoundingClientRect(), style = getComputedStyle(panel);
    return {x: r.x, y: r.y, width: r.width, height: r.height, right: r.right, bottom: r.bottom,
      position: style.position, display: style.display, viewport: [innerWidth, innerHeight]};
  });
  assert.equal(state.position, 'fixed', 'filter panel participates in photograph layout');
  assert.notEqual(state.display, 'none');
  assert(state.width > 200 && state.height > 20, JSON.stringify(state));
  assert(state.x >= -1 && state.y >= -1 && state.right <= state.viewport[0] + 1 && state.bottom <= state.viewport[1] + 1,
    'floating panel escaped the viewport: ' + JSON.stringify(state));
  return state;
}
async function layout() {
  const photo = await stablePhoto();
  const basic = await page.$eval('#editpanel', panel => { const r = panel.getBoundingClientRect();
    return {x: r.x, y: r.y, width: r.width, height: r.height}; });
  return {photo, basic};
}
function sameLayout(actual, expected, label) {
  sameBox(actual.photo, expected.photo, label + ' photo');
  // History status text changes font metrics by a few pixels when a draft starts.
  // Its position and width, and every part of the photo box, must remain fixed.
  for (const key of Object.keys(expected.basic)) {
    const tolerance = key === 'height' ? 4 : 1.1;
    assert(Math.abs(actual.basic[key] - expected.basic[key]) < tolerance,
      `${label} changed inline controls ${key}: ${JSON.stringify(actual.basic)} versus ${JSON.stringify(expected.basic)}`);
  }
}
async function basicOpen(mobile = false) {
  if (!await page.evaluate(() => $('lb').classList.contains('editing'))) {
    if (mobile) await touch('#edbtn'); else await page.click('#edbtn');
  }
  await page.waitForSelector('#editpanel', {visible: true});
}
async function filterHidden() {
  return page.$eval('#edstudio', panel => panel.hidden || panel.classList.contains('is-concealed') ||
    getComputedStyle(panel).display === 'none' || +getComputedStyle(panel).opacity === 0);
}
async function waitFilterHidden() {
  await page.waitForFunction(() => {
    const panel = $('edstudio'), style = getComputedStyle(panel);
    return panel.hidden || style.display === 'none' || style.opacity === '0' && style.pointerEvents === 'none';
  });
}
async function filterOpen(mobile = false) {
  if (await filterHidden()) {
    if (mobile) await touch('#edfilteropen'); else await page.click('#edfilteropen');
  }
  await page.waitForFunction(() => !$('edstudio').hidden && !$('edstudio').classList.contains('is-concealed'));
  await panelBox();
}
async function pixels() {
  return page.evaluate(async () => {
    const image = await createImageBitmap($('lbimg'));
    const canvas = document.createElement('canvas'); canvas.width = image.width; canvas.height = image.height;
    const context = canvas.getContext('2d'); context.drawImage(image, 0, 0); image.close();
    const data = context.getImageData(0, 0, canvas.width, canvas.height).data;
    const digest = await crypto.subtle.digest('SHA-256', data);
    const sample = Array.from(context.getImageData(Math.floor(canvas.width / 2), Math.floor(canvas.height / 2), 1, 1).data);
    return {width: canvas.width, height: canvas.height, sample,
      hash: [...new Uint8Array(digest)].map(value => value.toString(16).padStart(2, '0')).join('')};
  });
}
async function nativeIdle() {
  await page.waitForFunction(() => {
    const state = studioSession;
    return state?.ready && !studioIsBusy() && !state.adjustTask && !state.adjustTimer &&
      state.previewInputKey === studioInputKey(state, studioPreviewEdits(state)) &&
      state.paintedAdjustVersion === state.adjustVersion && $('lbimg').complete;
  }, {timeout: 60000});
}
async function waitPixelChange(previous) {
  const deadline = Date.now() + 30000;
  do { const current = await pixels(); if (current.hash !== previous.hash) return current;
    await new Promise(resolve => setTimeout(resolve, 60)); } while (Date.now() < deadline);
  throw new Error('Main photograph pixels did not change');
}
async function openPhoto(edited = false) {
  await page.evaluate(async ({sha, source}) => {
    await go({type: 'source', key: source, criteria: {source}});
    const index = items.findIndex(item => item.sha1 === sha);
    if ($('lb').classList.contains('show')) await lbShow(index, 0); else openLb(index);
  }, {sha, source});
  await page.waitForFunction(({sha, edited}) => lbMeta?.sha1 === sha && $('lbimg').complete && $('lbimg').naturalWidth > 0 &&
    new URL($('lbimg').currentSrc || $('lbimg').src).pathname === (edited ? '/render/' : '/img/') + sha,
    {timeout: 60000}, {sha, edited});
  return stablePhoto();
}
async function draft() {
  return page.evaluate(() => ({session: studioSession.session, graph: structuredClone(studioSession.graph),
    photoEdits: structuredClone(studioSession.photoEdits), history: structuredClone(studioSession.history),
    sliders: structuredClone(studioSession.adjustValues)}));
}
async function touch(selector) {
  const point = await page.$eval(selector, element => {
    element.scrollIntoView({block: 'nearest'});
    const r = element.getBoundingClientRect(); return {x: r.x + r.width / 2, y: r.y + r.height / 2};
  });
  await page.touchscreen.tap(point.x, point.y);
}
(async () => {
  assert((await api('/api/settings')).root.startsWith('/tmp/'), 'Disposable /tmp data root required');
  browser = await puppeteer.launch({executablePath: process.env.CHROME || '/usr/bin/google-chrome',
    headless: 'new', args: ['--no-sandbox'], defaultViewport: {width: 1500, height: 1100}});
  page = await browser.newPage(); page.on('pageerror', error => errors.push(error.message));
  page.on('response', response => { if (!response.ok() && !response.url().includes('/api/enrich/'))
    networkErrors.push({url: response.url(), status: response.status()}); });
  await page.setRequestInterception(true);
  page.on('request', request => {
    const path = new URL(request.url()).pathname;
    if (!allowFixtureSave && (request.method() === 'PUT' && path.startsWith('/api/edits/') || request.method() === 'POST' && path.endsWith('/save'))) {
      forbidden.push(path); return request.respond({status: 409, contentType: 'application/json', body: '{"detail":"draft-only floating editor test"}'});
    }
    if (path === '/api/enrich/one') return request.respond({status: 409, contentType: 'application/json', body: '{"detail":"no VLM during UI fixture tests"}'});
    request.continue();
  });
  await page.goto(BASE, {waitUntil: 'networkidle2', timeout: 60000});
  const upload = await page.evaluate(async source => {
    const canvas = document.createElement('canvas'); canvas.width = 480; canvas.height = 360;
    const context = canvas.getContext('2d');
    for (let x = 0; x < canvas.width; x++) { context.fillStyle = `rgb(${30 + Math.round(x / 8)},80,140)`; context.fillRect(x, 0, 1, canvas.height); }
    [...source].forEach((letter, index) => { context.fillStyle = `rgb(${letter.charCodeAt(0)},10,20)`; context.fillRect(index * 2, 2, 1, 1); });
    const form = new FormData(); form.append('source', source);
    form.append('file', await new Promise(resolve => canvas.toBlob(resolve, 'image/png')), 'floating-editor.png');
    const response = await fetch('/api/upload', {method: 'POST', body: form}); return {ok: response.ok, result: await response.json()};
  }, source);
  assert(upload.ok && upload.result.added === 1, JSON.stringify(upload));
  const listing = await api('/api/images?' + new URLSearchParams({source, limit: '10'}));
  assert.equal(listing.items.length, 1); sha = listing.items[0].sha1;
  const metadataBefore = await api('/api/meta/' + sha);
  const originalBytes = hash(Buffer.from(await (await fetch(BASE + '/img/' + sha)).arrayBuffer()));
  await openPhoto();
  await basicOpen();
  const baseline = await layout(), originalPixels = await pixels();
  assert.notEqual(await page.$eval('#editpanel', panel => getComputedStyle(panel).position), 'fixed');
  assert.equal(await page.$$eval('#editpanel [id$="-panel-header"], #editpanel #editpanel-header', elements => elements.length), 0);
  assert.equal(await page.$$eval('#editpanel #edstudio', elements => elements.length), 0);
  assert.equal(await page.$$eval('#editpanel #edfilteropen', elements => elements.length), 1);
  assert(await page.$eval('#edfilteropen', button => {
    const group = button.closest('.ed-filter-tools'), blur = group?.previousElementSibling;
    if (group?.parentElement.id !== 'edfilters' || group.getAttribute('role') !== 'group' || blur?.textContent !== 'ぼかし') return false;
    const a = blur.getBoundingClientRect(), b = group.getBoundingClientRect();
    return b.left >= a.right && Math.abs(b.y + b.height / 2 - a.y - a.height / 2) < 1.1;
  }), 'language filter tools must form a group immediately to the right of Blur');
  assert(await filterHidden(), 'basic Edit unexpectedly opened the filter float');
  for (const id of ['ed_exposure', 'ed_temperature', 'edauto', 'cropbtn', 'edapply'])
    assert(await page.$eval('#editpanel #' + id, element => element.getBoundingClientRect().width > 0));
  pass('Edit keeps the original photo controls inline and leaves the separate filter panel closed');

  await filterOpen();
  assert.equal(await page.evaluate(() => !!studioSession), false, 'opening language controls eagerly started the native renderer');
  sameLayout(await layout(), baseline, 'opening filter controls'); await panelBox();
  pass('the filter launcher opens only a floating panel without moving the photograph or inline photo controls');

  await page.$eval('#studio-text', input => input.value = '色反転'); await page.click('#studio-generate');
  await page.click('#filter-panel-minimize');
  await page.waitForFunction(() => $('edstudio').classList.contains('is-minimized'));
  await nativeIdle(); const inverted = await waitPixelChange(originalPixels);
  for (let channel = 0; channel < 3; channel++) assert(Math.abs(inverted.sample[channel] - (255 - originalPixels.sample[channel])) <= 2);
  sameLayout(await layout(), baseline, 'native initialization while minimized'); await panelBox();
  await page.click('#filter-panel-minimize');
  await page.waitForFunction(() => !$('edstudio').classList.contains('is-minimized')); await panelBox();
  await page.screenshot({path: '/tmp/fg-filter-only-desktop.png'});
  pass('native filters initialize while minimized and update the main pixels while the inline layout stays unchanged');

  const panelBeforeDrag = await panelBox(), layoutBeforeDrag = await layout();
  const handle = await page.$eval('#filter-panel-drag', element => { const r = element.getBoundingClientRect(); return {x: r.left + Math.min(50, r.width / 2), y: r.top + r.height / 2}; });
  await page.mouse.move(handle.x, handle.y); await page.mouse.down();
  await page.mouse.move(handle.x - 180, handle.y + 70, {steps: 10}); await page.mouse.up();
  const panelAfterDrag = await panelBox();
  assert(Math.abs(panelAfterDrag.x - panelBeforeDrag.x) + Math.abs(panelAfterDrag.y - panelBeforeDrag.y) > 50);
  sameLayout(await layout(), layoutBeforeDrag, 'dragging filter panel');
  pass('pointer dragging moves only the filter panel and leaves both photograph and inline controls in place');

  await page.$eval('#ed_exposure', input => { input.value = '20'; input.dispatchEvent(new Event('input', {bubbles: true})); });
  await nativeIdle(); const adjusted = await waitPixelChange(inverted);
  assert(adjusted.sample[0] < inverted.sample[0] - 5, 'inline exposure was not rendered before native invert');
  assert.equal(await page.$eval('#lbimg', image => image.style.filter), '');
  const beforeCollapse = await draft(), expanded = await panelBox(), adjustedLayout = await layout();
  await page.click('#filter-panel-minimize');
  await page.waitForFunction(() => $('edstudio').classList.contains('is-minimized'));
  const minimized = await panelBox(); assert(minimized.height < expanded.height / 2);
  assert.equal(await page.$eval('#filter-panel-minimize', element => element.getAttribute('aria-expanded')), 'false');
  assert.deepEqual(await draft(), beforeCollapse); assert.deepEqual(await pixels(), adjusted);
  sameLayout(await layout(), adjustedLayout, 'minimizing filter draft');
  await page.click('#filter-panel-minimize');
  await page.waitForFunction(() => !$('edstudio').classList.contains('is-minimized'));
  assert.deepEqual(await draft(), beforeCollapse); assert.deepEqual(await pixels(), adjusted);
  await page.click('#filter-panel-close'); await waitFilterHidden();
  assert.deepEqual(await draft(), beforeCollapse); assert.deepEqual(await pixels(), adjusted);
  sameLayout(await layout(), adjustedLayout, 'hiding filter draft');
  assert(await page.evaluate(() => $('lb').classList.contains('editing')));
  await page.click('#editpanel button[onclick="edUndo()"]'); await nativeIdle();
  assert.deepEqual(await pixels(), inverted, 'inline Undo did not cancel the pending slider with filters hidden');
  assert(await filterHidden(), 'Undo unexpectedly reopened the filter panel');
  const hiddenDraft = await draft();
  await page.focus('#edfilteropen'); await page.keyboard.press('Space');
  await page.waitForFunction(() => $('lb').classList.contains('show') && $('lb').classList.contains('editing') &&
    !$('edstudio').hidden && !$('edstudio').classList.contains('is-concealed'));
  assert.deepEqual(await draft(), hiddenDraft);
  pass('minimize and close preserve the filter draft; inline Undo works while the float is hidden and reopening restores its state');

  await page.click('#studio-close');
  await page.waitForFunction(sha => !studioSession && !$('studio-frame') && $('lbimg').complete &&
    new URL($('lbimg').currentSrc || $('lbimg').src).pathname === '/img/' + sha, {timeout: 60000}, sha);
  assert.deepEqual(await pixels(), originalPixels);
  sameLayout(await layout(), baseline, 'canceling native draft');
  if (!await filterHidden()) { await page.click('#filter-panel-close'); await waitFilterHidden(); }
  pass('native Cancel restores original pixels while the ordinary inline photo controls remain open');

  await page.setViewport({width: 390, height: 844, isMobile: true, hasTouch: true, deviceScaleFactor: 1});
  await page.waitForFunction(() => typeof go === 'function' && typeof edToggle === 'function');
  await openPhoto(); await basicOpen(true);
  await page.$eval('#edfilteropen', element => element.scrollIntoView({block: 'nearest'}));
  const mobileBaseline = await layout();
  await filterOpen(true); sameLayout(await layout(), mobileBaseline, 'opening mobile filter controls');
  await page.$eval('#studio-text', input => input.value = '色反転'); await touch('#studio-generate');
  await nativeIdle(); await waitPixelChange(originalPixels);
  sameLayout(await layout(), mobileBaseline, 'opening mobile native filters'); await panelBox();
  for (const id of ['studio-text', 'studio-generate', 'studio-reset']) {
    await page.$eval('#' + id, element => element.scrollIntoView({block: 'nearest'}));
    assert(await page.$eval('#' + id, element => { const r = element.getBoundingClientRect();
      const hit = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
      return r.top >= 0 && r.bottom <= innerHeight + 1 && (hit === element || element.contains(hit)); }), id + ' not reachable');
    sameLayout(await layout(), mobileBaseline, 'scrolling mobile filter controls');
  }
  const touchBefore = await panelBox(), mobileBeforeDrag = await layout();
  const mobileHandle = await page.$eval('#filter-panel-drag', element => { const r = element.getBoundingClientRect();
    return {x: r.x + Math.min(50, r.width / 2), y: r.y + r.height / 2}; });
  const client = await page.createCDPSession(), touchDelta = touchBefore.y > 80 ? -70 : 70;
  await client.send('Input.dispatchTouchEvent', {type: 'touchStart', touchPoints: [{...mobileHandle, id: 1}]});
  await client.send('Input.dispatchTouchEvent', {type: 'touchMove', touchPoints: [{x: mobileHandle.x, y: mobileHandle.y + touchDelta, id: 1}]});
  await client.send('Input.dispatchTouchEvent', {type: 'touchEnd', touchPoints: []}); await client.detach();
  assert(Math.abs((await panelBox()).y - touchBefore.y) > 20);
  sameLayout(await layout(), mobileBeforeDrag, 'touch-dragging filter panel');
  await touch('#filter-panel-minimize'); await panelBox();
  await touch('#filter-panel-minimize'); await panelBox();
  await page.screenshot({path: '/tmp/fg-studio-float-mobile.png'});
  const mobileDraft = await draft();
  await touch('#filter-panel-close'); await waitFilterHidden();
  assert.deepEqual(await draft(), mobileDraft); sameLayout(await layout(), mobileBaseline, 'hiding mobile filter panel');
  pass('mobile touch dragging, minimizing and hiding affect only the filter float and preserve the inline layout and draft');

  await page.setViewport({width: 844, height: 390, isMobile: true, hasTouch: true, deviceScaleFactor: 1});
  await page.$eval('#edfilteropen', element => element.scrollIntoView({block: 'nearest'}));
  const landscapeBaseline = await layout();
  await filterOpen(true); await panelBox();
  sameLayout(await layout(), landscapeBaseline, 'reopening filter panel in phone landscape');
  await touch('#filter-panel-minimize'); await panelBox();
  await touch('#filter-panel-minimize'); await panelBox();
  await touch('#studio-close');
  await page.waitForFunction(() => !studioSession && !$('studio-frame'));
  if (!await filterHidden()) { await touch('#filter-panel-close'); await waitFilterHidden(); }
  pass('landscape reopening clamps the remembered filter position while keeping basic photo controls inline');

  assert.deepEqual(await api('/api/meta/' + sha), metadataBefore);
  assert.equal(hash(Buffer.from(await (await fetch(BASE + '/img/' + sha)).arrayBuffer())), originalBytes);
  assert.equal((await api('/api/images?' + new URLSearchParams({source, limit: '10'}))).total, 1);
  assert.deepEqual(forbidden, []); assert.deepEqual(errors, []);
  pass('draft-only interactions preserve original image bytes, metadata and gallery count without JavaScript errors');

  // A fresh desktop page isolates the saved crop's layout from mobile scrolling.
  allowFixtureSave = true;
  await page.setViewport({width: 1500, height: 1100, isMobile: false, hasTouch: false, deviceScaleFactor: 1});
  await page.goto(BASE, {waitUntil: 'networkidle2', timeout: 60000});
  await page.waitForFunction(() => typeof go === 'function' && typeof edToggle === 'function');
  await api('/api/edits/' + sha, {action: 'push', edit: {op: 'crop', params: {fx: .2, fy: .1, fw: .4, fh: .8}}}, 'PUT');
  await openPhoto(true); await basicOpen();
  const croppedLayout = await layout(), croppedPixels = await pixels();
  assert.deepEqual([croppedPixels.width, croppedPixels.height], [192, 288]);
  await filterOpen();
  await page.$eval('#studio-text', input => input.value = '色反転'); await page.click('#studio-generate');
  await nativeIdle(); await waitPixelChange(croppedPixels);
  sameLayout(await layout(), croppedLayout, 'opening filter float on a small portrait crop');
  await page.click('#filter-panel-close'); await waitFilterHidden();
  const saveResponse = page.waitForResponse(response => response.request().method() === 'POST' &&
    new URL(response.url()).pathname === `/api/studio/${sha}/save`, {timeout: 120000});
  await page.click('#edapply');
  const response = await saveResponse, saved = await response.json(); assert(response.ok(), JSON.stringify(saved));
  assert.equal(saved.sha1, sha);
  await page.waitForFunction(sha => !studioSession && lbMeta.edits.at(-1).op === 'studio' && $('lbimg').complete &&
    new URL($('lbimg').currentSrc || $('lbimg').src).pathname === '/render/' + sha, {timeout: 60000}, sha);
  const savedLayout = await layout(), savedPixels = await pixels();
  assert.deepEqual([savedPixels.width, savedPixels.height], [192, 288]);
  await filterOpen(); await page.click('#lbstudiobtn'); await nativeIdle();
  sameLayout(await layout(), savedLayout, 'reopening the saved small portrait recipe');
  assert.deepEqual(await pixels(), savedPixels);
  await page.click('#filter-panel-close'); await waitFilterHidden();
  sameLayout(await layout(), savedLayout, 'hiding the saved small portrait recipe');
  await filterOpen(); await page.click('#studio-close');
  await page.waitForFunction(sha => !studioSession && $('lbimg').complete &&
    new URL($('lbimg').currentSrc || $('lbimg').src).pathname === '/render/' + sha, {timeout: 60000}, sha);
  sameLayout(await layout(), savedLayout, 'canceling the saved small portrait recipe');
  assert.equal(hash(Buffer.from(await (await fetch(BASE + '/img/' + sha)).arrayBuffer())), originalBytes);
  assert.equal((await api('/api/images?' + new URLSearchParams({source, limit: '10'}))).total, 1);
  assert.deepEqual(errors, []);
  pass('inline Apply saves a hidden filter draft; saved crops retain their displayed size when the filter float reopens, hides or cancels');
  console.log(`${checks} checks passed`);
})().catch(async error => {
  console.error(error.stack || error); process.exitCode = 1;
  if (page) { console.error('state:', await page.evaluate(() => ({editing: $('lb').className,
    panel: $('edstudio').outerHTML.slice(0, 600), status: $('studio-status').textContent,
    panelRect: $('edstudio').getBoundingClientRect().toJSON(),
    viewport: {width: innerWidth, height: innerHeight, top: visualViewport?.offsetTop, left: visualViewport?.offsetLeft},
    ancestors: ['edstudio','lb','lbinner'].map(id => {const el = $(id); if(!el) return {id}; const s = getComputedStyle(el); return {id, scrollTop:el.scrollTop, rect:el.getBoundingClientRect().toJSON(), transform:s.transform, position:s.position, contain:s.contain, backdropFilter:s.backdropFilter};}),
    edstatus: $('edstatus').textContent, src: $('lbimg').src,
    dimensions: [$('lbimg').naturalWidth, $('lbimg').naturalHeight], ready: studioSession?.ready})).catch(() => null));
    console.error('network errors:', networkErrors, 'JavaScript errors:', errors);
    await page.screenshot({path: '/tmp/fg-studio-float-failure.png'}).catch(() => {}); }
}).finally(async () => {
  if (browser) await browser.close();
  if (sha) await api('/api/trash', {shas: [sha]}).catch(error => console.error('cleanup:', error.message));
});
