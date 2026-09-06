// フォルダの言語指示フィルタ: 焼き込み・原本保護・再利用・実UIの回帰検査。
// 実行: FG_URL=http://127.0.0.1:<isolated-test-port> node tests/folder_filters.js
// 専用ルートで動かしたテストサーバを指定する。PNG素材はcanvasで一意に生成する。
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const puppeteer = require('puppeteer-core');

const BASE = process.env.FG_URL;
assert(BASE, '専用テストサーバの FG_URL を指定してください');
const nonce = `${Date.now().toString(36)}_${crypto.randomBytes(4).toString('hex')}`;
const albumName = `_filtertest_${nonce}`;
const controlName = `_filtercontrol_${nonce}`;
const wideName = `_filterwide_${nonce}`;
const source = `crawl:${albumName}`;
const albumURL = `/api/albums/${encodeURIComponent(albumName)}`;
const createdShas = new Set();
const createdSets = new Set();
let browser;
let page;
let checks = 0;

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const passed = name => { checks++; console.log(`✅ ${name}`); };
async function api(url, body, method = body === undefined ? 'GET' : 'POST') {
  const response = await fetch(BASE + url, {
    method,
    headers: body === undefined ? {} : {'Content-Type': 'application/json'},
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const raw = await response.text();
  assert(response.ok, `${method} ${url}: ${response.status} ${raw}`);
  return raw ? JSON.parse(raw) : null;
}
async function until(probe, message, timeout = 60000) {
  const start = Date.now();
  let last;
  while (Date.now() - start < timeout) {
    last = await probe();
    if (last) return last;
    await sleep(100);
  }
  throw new Error(`Timeout: ${message}; last=${JSON.stringify(last)}`);
}
async function album(name = albumName) {
  return (await api('/api/albums')).find(value => value.name === name);
}
async function list(criteria, view) {
  return api('/api/images?' + new URLSearchParams({...criteria, limit: '100', ...(view ? {view} : {})}));
}
async function completed(setId, name = albumName) {
  createdSets.add(setId);
  const status = await until(async () => {
    const value = await api('/api/filters/status');
    return value.set_id === setId && !value.running && !value.committing && value;
  }, `filter ${setId} completion`, 120000);
  assert.equal(status.errors, 0, JSON.stringify(status));
  assert.equal(status.done, status.total, JSON.stringify(status));
  const current = await album(name);
  assert.equal(current.display_criteria?.filter_set, setId, JSON.stringify(current));
  assert(!current.filter_job, 'completed album must remove its pending filter_job');
  const output = await list({filter_set: setId});
  for (const image of output.items) createdShas.add(image.sha1);
  return {status, current, output};
}
async function openFolder(name = albumName) {
  await page.evaluate(async name => {
    await loadAlbums();
    const folder = albumsCache.find(value => value.name === name);
    await go({type: 'folder', key: name, criteria: folder.display_criteria || folder.criteria});
  }, name);
  try {
    await page.waitForSelector('#folder_filter_text', {visible: true});
  } catch (error) {
    console.error('folder DOM:', await page.evaluate(() => ({
      loc, albums: albumsCache.map(value => value.name),
      header: document.getElementById('viewhead')?.outerHTML.slice(0, 1800),
      field: (() => {
        const parents = [];
        for (let node = document.getElementById('folder_filter_text'); node; node = node.parentElement) {
          const style = getComputedStyle(node), rect = node.getBoundingClientRect();
          parents.push({tag: node.tagName, id: node.id, className: node.className, display: style.display,
            visibility: style.visibility, width: rect.width, height: rect.height});
        }
        return parents;
      })(),
    })));
    await page.screenshot({path: '/tmp/fg-folder-filter-failure.png'});
    throw error;
  }
}
async function setPrompt(text) {
  await page.$eval('#folder_filter_text', (input, value) => {
    input.value = value;
    input.dispatchEvent(new Event('input', {bubbles: true}));
  }, text);
}
async function uiApply(trigger) {
  const [response] = await Promise.all([
    page.waitForResponse(value => new URL(value.url()).pathname === albumURL + '/filter' &&
      value.request().method() === 'POST', {timeout: 60000}),
    trigger(),
  ]);
  const result = await response.json();
  assert(response.ok(), JSON.stringify(result));
  assert.equal(typeof result.set_id, 'string');
  return completed(result.set_id);
}
async function gridContains(shas) {
  await page.waitForFunction(expected => items.length === expected.length &&
    items.every(value => expected.includes(value.sha1)), {timeout: 20000}, shas);
}

(async () => {
  const initialStatus = await api('/api/filters/status');
  assert(!initialStatus.running && !initialStatus.committing, '専用サーバで既存フィルタ処理が実行されています');
  browser = await puppeteer.launch({
    executablePath: process.env.CHROME || '/usr/bin/google-chrome', headless: 'new',
    args: ['--no-sandbox'], defaultViewport: {width: 1600, height: 1000},
  });
  page = await browser.newPage();
  const errors = [];
  const filterRequests = [];
  page.on('pageerror', error => errors.push(error.message));
  page.on('request', request => {
    if (request.method() === 'POST' && /\/api\/(?:filters\/plan|albums\/[^/]+\/filter)$/.test(new URL(request.url()).pathname)) {
      filterRequests.push(request.url());
    }
  });
  await page.goto(BASE, {waitUntil: 'networkidle2', timeout: 60000});
  await page.waitForFunction(() => typeof go === 'function' && typeof loadAlbums === 'function');

  // 原本の既存SHAを触らないよう、輪郭にも一意性が残る模様を描く。
  const uploaded = await page.evaluate(async ({source, nonce}) => {
    const form = new FormData();
    form.append('source', source);
    for (let i = 0; i < 2; i++) {
      const canvas = document.createElement('canvas');
      canvas.width = 1456 + i * 16;
      canvas.height = 1092 + i * 12;
      const ctx = canvas.getContext('2d');
      ctx.fillStyle = '#102040'; ctx.fillRect(0, 0, canvas.width, canvas.height);
      ctx.fillStyle = '#ffb060'; ctx.fillRect(200 + i * 60, 150, 730, 640);
      ctx.fillStyle = '#ffffff';
      for (let j = 0; j < nonce.length; j++) {
        const height = 30 + nonce.charCodeAt(j) * 3;
        ctx.fillRect(24 + j * 45, 820, 14 + i * 5, height);
      }
      ctx.font = '36px sans-serif'; ctx.fillText(nonce + i, 80, 90);
      const blob = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'));
      form.append('file', blob, `filter-${i}.png`);
    }
    const response = await fetch('/api/upload', {method: 'POST', body: form});
    return {ok: response.ok, data: await response.json()};
  }, {source, nonce});
  assert(uploaded.ok, JSON.stringify(uploaded));
  assert.equal(uploaded.data.added, 2, JSON.stringify(uploaded));
  const originals = await list({source});
  assert.equal(originals.total, 2);
  const originalShas = originals.items.map(value => value.sha1).sort();
  originalShas.forEach(sha => createdShas.add(sha));
  const originalMeta = new Map(await Promise.all(originalShas.map(async sha => [sha, await api('/api/meta/' + sha)])));
  const originalBytes = new Map(await Promise.all(originalShas.map(async sha => {
    const response = await fetch(BASE + '/img/' + sha);
    assert(response.ok);
    return [sha, crypto.createHash('sha1').update(Buffer.from(await response.arrayBuffer())).digest('hex')];
  })));
  for (const name of [albumName, controlName]) {
    await api('/api/albums', {name, criteria: {source}, folder: '', agent: {}, goal: ''});
  }
  passed('一意な原本2枚と比較用フォルダを作成');

  const plan = await api('/api/filters/plan', {text: '境界線だけにして'});
  assert.equal(plan.edit.op, 'pipeline');
  assert(plan.edit.params.edits.some(value => value.op === 'filter' && value.params.name === 'canny'));
  const invalidPlan = await fetch(BASE + '/api/filters/plan', {
    method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify({text: ''}),
  });
  assert.equal(invalidPlan.status, 400);
  passed('「境界線だけにして」をCannyへ変換・空指示を拒否');

  await openFolder();
  for (const id of ['folder_filter_apply', 'folder_filter_random', 'folder_filter_reset', 'folder_filter_status']) {
    assert(await page.$('#' + id), `${id} must exist in the folder header`);
  }
  await setPrompt('境界線だけにして');
  const first = await uiApply(() => page.click('#folder_filter_apply'));
  assert.equal(first.status.total, 2);
  assert.equal(first.output.total, 2);
  assert.equal(first.status.cached, 0);
  assert.deepEqual(first.current.criteria, {source, exclude_filtered: true});
  assert(first.current.filter, 'album must retain its applied filter metadata');
  const outputShas = first.output.items.map(value => value.sha1).sort();
  assert(outputShas.every(sha => !originalShas.includes(sha)), 'materialized images need new SHA identities');
  await gridContains(outputShas);
  await page.screenshot({path: '/tmp/fg-folder-filter-desktop.png'});
  await page.setViewport({width: 390, height: 844});
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({path: '/tmp/fg-folder-filter-mobile.png'});
  await page.setViewport({width: 1600, height: 1000});
  passed('適用ボタン1回で全画像を処理し、完了後フォルダを結果へ切替');

  for (const item of first.output.items) {
    const meta = await api('/api/meta/' + item.sha1);
    assert.equal(meta.ext, 'png');
    assert(!meta.edits || meta.edits.length === 0, 'materialized output must not contain live edits');
    assert(!item.erev, 'materialized output must not have an edit revision');
    assert(originals.items.some(original => original.w === item.w && original.h === item.h));
    const originalSHA = meta.filter_source_sha;
    assert(originalShas.includes(originalSHA), `missing original provenance: ${JSON.stringify(meta)}`);
    assert.equal(meta.filter_source, source);
    assert.equal(meta.filter_recipe.op, 'pipeline');
    assert(meta.filter_recipe.params.edits.some(value => value.params?.name === 'canny'));
    assert.equal(typeof meta.filter_cache_key, 'string');
    assert(meta.filter_cache_key.length > 0);
    assert.equal(item.w, originalMeta.get(originalSHA).w);
    assert.equal(item.h, originalMeta.get(originalSHA).h);
    const response = await fetch(BASE + '/img/' + item.sha1);
    assert(response.ok);
    assert.equal(response.headers.get('content-type'), 'image/png');
    assert.match(response.headers.get('cache-control'), /immutable/);
    const png = Buffer.from(await response.arrayBuffer());
    assert.equal(png.subarray(1, 4).toString(), 'PNG');
    assert.equal(png.readUInt32BE(16), item.w);
    assert.equal(png.readUInt32BE(20), item.h);
    assert.equal(png[25], 2, 'materialized PNG must use RGB color type');
    assert.equal(crypto.createHash('sha1').update(png).digest('hex'), item.sha1);
    const pixels = await page.evaluate(async sha => {
      const image = await createImageBitmap(await (await fetch('/img/' + sha)).blob());
      const canvas = document.createElement('canvas'); canvas.width = image.width; canvas.height = image.height;
      const ctx = canvas.getContext('2d'); ctx.drawImage(image, 0, 0);
      const {data} = ctx.getImageData(0, 0, canvas.width, canvas.height);
      let white = 0, black = 0, other = 0;
      for (let i = 0; i < data.length; i += 4) {
        if (data[i] === 255 && data[i + 1] === 255 && data[i + 2] === 255) white++;
        else if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 0) black++;
        else other++;
      }
      image.close();
      return {white, black, other};
    }, item.sha1);
    assert.equal(pixels.other, 0, JSON.stringify(pixels));
    assert(pixels.white > 100 && pixels.black > pixels.white * 10, JSON.stringify(pixels));
    for (const route of ['/preview/', '/thumb/', '/micro/']) {
      const thumbnail = await fetch(BASE + route + item.sha1);
      assert(thumbnail.ok, route + item.sha1);
      assert.match(thumbnail.headers.get('content-type'), /^image\//);
      assert.match(thumbnail.headers.get('cache-control'), /immutable/);
    }
  }
  const grid = await list({filter_set: first.status.set_id}, 'grid');
  assert.equal(grid.total, 2);
  assert(grid.items.every(value => !value.erev));
  passed('別SHAの原寸RGB PNG・白黒輪郭・来歴・通常プレビュー・編集なしを検証');

  const control = await album(controlName);
  assert.deepEqual(control.criteria, {source});
  assert(!control.display_criteria?.filter_set && !control.filter && !control.filter_job);
  assert.deepEqual((await list(control.criteria)).items.map(value => value.sha1).sort(), originalShas);
  for (const sha of originalShas) {
    assert.deepEqual(await api('/api/meta/' + sha), originalMeta.get(sha));
    const bytes = Buffer.from(await (await fetch(BASE + '/img/' + sha)).arrayBuffer());
    assert.equal(crypto.createHash('sha1').update(bytes).digest('hex'), originalBytes.get(sha));
  }
  passed('原本の画像とメタデータ・同じ原本を見る別フォルダを保護');

  const [resetResponse] = await Promise.all([
    page.waitForResponse(response => new URL(response.url()).pathname === albumURL + '/filter/reset' && response.request().method() === 'POST'),
    page.click('#folder_filter_reset'),
  ]);
  assert(resetResponse.ok());
  await gridContains(originalShas);
  const reset = await album();
  assert.deepEqual(reset.criteria, {source, exclude_filtered: true});
  assert(!reset.display_criteria?.filter_set && !reset.filter && !reset.filter_job);
  passed('リセットボタン1回で原本表示へ戻す');

  const reapplied = await api(albumURL + '/filter', {edit: plan.edit});
  const reused = await completed(reapplied.set_id);
  assert.equal(reused.status.cached, 2, JSON.stringify(reused.status));
  assert.deepEqual(reused.output.items.map(value => value.sha1).sort(), outputShas);
  passed('同じ指示を再適用すると既存の焼き込み画像を再利用');

  await openFolder();
  await setPrompt('モノクロにして');
  const beforeIME = filterRequests.length;
  await page.$eval('#folder_filter_text', input => {
    input.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', code: 'Enter', isComposing: true, bubbles: true, cancelable: true}));
    input.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', keyCode: 229, bubbles: true, cancelable: true}));
  });
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  assert.equal(filterRequests.length, beforeIME, 'IME conversion confirmation must not apply a filter');
  await page.focus('#folder_filter_text');
  const enter = await uiApply(() => page.keyboard.press('Enter'));
  assert.equal(enter.output.total, 2);
  assert(enter.current.filter);
  await gridContains(enter.output.items.map(value => value.sha1));
  passed('IME変換中のEnterを無視し、通常のEnterで適用');

  const beforeRandom = filterRequests.length;
  const random = await uiApply(() => page.click('#folder_filter_random'));
  assert.equal(random.output.total, 2);
  assert.equal(filterRequests.slice(beforeRandom).filter(url => new URL(url).pathname === albumURL + '/filter').length, 1);
  await gridContains(random.output.items.map(value => value.sha1));
  passed('ランダムボタン1回でフィルタ選択から適用まで完了');

  // サイドバーの更新でfilter_jobが消えても、処理を始めた画面は完了を追跡する。
  await api(albumURL + '/filter/reset', {});
  await openFolder();
  await gridContains(originalShas);
  await setPrompt('境界線だけにして');
  const [refreshResponse] = await Promise.all([
    page.waitForResponse(response => new URL(response.url()).pathname === albumURL + '/filter' && response.request().method() === 'POST'),
    page.click('#folder_filter_apply'),
  ]);
  assert(refreshResponse.ok());
  const refreshStart = await refreshResponse.json();
  await page.waitForFunction(name => folderFilterWatched.has(name), {}, albumName);
  await page.evaluate(() => { clearTimeout(folderFilterTimer); folderFilterTimer = null; });
  const refreshJob = await completed(refreshStart.set_id);
  const refreshed = await page.evaluate(async name => {
    await loadAlbums();
    const record = albumsCache.find(value => value.name === name);
    const state = {hasPending: !!record.filter_job, watched: folderFilterWatched.has(name)};
    await pollFolderFilter();
    return state;
  }, albumName);
  assert.equal(refreshed.hasPending, false);
  assert.equal(refreshed.watched, true);
  await gridContains(refreshJob.output.items.map(value => value.sha1));
  passed('完了直前にフォルダ一覧を再取得しても現在の表示を加工結果へ切替');

  // HTTP応答だけを留め、サーバでは開始済みの状態を作る。リセットとの競合を確実に再現する。
  await api(albumURL + '/filter/reset', {});
  await openFolder();
  await gridContains(originalShas);
  await page.evaluate(target => {
    const realFetch = window.fetch.bind(window);
    window.__filterResponseGate = {received: false, release: null, result: null, realFetch};
    window.fetch = async (input, init) => {
      const response = await realFetch(input, init);
      const url = new URL(typeof input === 'string' ? input : input.url, location.href);
      if (url.pathname === target && init?.method === 'POST') {
        window.__filterResponseGate.result = await response.clone().json();
        window.__filterResponseGate.received = true;
        await new Promise(resolve => { window.__filterResponseGate.release = resolve; });
      }
      return response;
    };
  }, albumURL + '/filter');
  await setPrompt('鮮やかにして');
  await page.click('#folder_filter_apply');
  await page.waitForFunction(() => window.__filterResponseGate.received);
  const raceStart = await page.evaluate(() => window.__filterResponseGate.result);
  assert.equal(typeof raceStart.set_id, 'string', JSON.stringify(raceStart));
  createdSets.add(raceStart.set_id);
  const resetArrives = page.waitForResponse(response => new URL(response.url()).pathname === albumURL + '/filter/reset' &&
    response.request().method() === 'POST');
  await page.click('#folder_filter_reset');
  await page.evaluate(() => {
    window.__filterResponseGate.release();
    window.fetch = window.__filterResponseGate.realFetch;
  });
  assert((await resetArrives).ok());
  await until(async () => {
    const status = await api('/api/filters/status');
    return !status.running && !status.committing;
  }, 'reset race worker completion');
  await page.evaluate(() => pollFolderFilter());
  await gridContains(originalShas);
  const afterRace = await album();
  assert(!afterRace.display_criteria?.filter_set && !afterRace.filter && !afterRace.filter_job, JSON.stringify(afterRace));
  passed('開始済み要求の応答が遅れてもリセット後に加工表示が復活しない');

  // 条件なしフォルダでも、焼き込みを始める時点で原本の集合を固定する。
  const wideOriginals = await list({exclude_filtered: 'true'});
  assert(wideOriginals.total > 0 && wideOriginals.total <= 100, '専用テストルートには100枚以下の素材を配置してください');
  await api('/api/albums', {name: wideName, criteria: {}, folder: '', agent: {}, goal: ''});
  const wideURL = '/api/albums/' + encodeURIComponent(wideName);
  const wideStart = await api(wideURL + '/filter', {edit: plan.edit});
  const wideJob = await completed(wideStart.set_id, wideName);
  assert.equal(wideJob.status.total, wideOriginals.total);
  assert.equal(wideJob.current.criteria.exclude_filtered, true);
  await openFolder(wideName);
  await gridContains(wideJob.output.items.map(value => value.sha1));
  const [wideResetResponse] = await Promise.all([
    page.waitForResponse(response => new URL(response.url()).pathname === wideURL + '/filter/reset' && response.request().method() === 'POST'),
    page.click('#folder_filter_reset'),
  ]);
  assert(wideResetResponse.ok());
  const wideReset = await album(wideName);
  assert.equal(wideReset.criteria.exclude_filtered, true);
  assert(!wideReset.display_criteria?.filter_set);
  const wideRestored = await list(wideReset.criteria);
  assert.deepEqual(wideRestored.items.map(value => value.sha1).sort(), wideOriginals.items.map(value => value.sha1).sort());
  assert(wideRestored.items.every(value => !value.source?.startsWith('filter:')));
  await gridContains(wideOriginals.items.map(value => value.sha1));
  passed('条件なしフォルダのリセットでも加工済み画像を混ぜず原本だけ表示');

  // ジョブがない時も停止要求は成功する。全件処理の失敗や後続起動を引き起こさない。
  await api('/api/filters/stop', {});
  const stopped = await api('/api/filters/status');
  assert(!stopped.running && !stopped.committing);
  assert.deepEqual(errors, [], 'browser JavaScript errors');
  passed('停止APIのアイドル時動作・ブラウザ例外なし');
  console.log(`\n${checks} checks passed`);
})().catch(error => {
  console.error(error.stack || error);
  process.exitCode = 1;
}).finally(async () => {
  if (page) await page.close().catch(() => {});
  // この実行が作った名前/SHAだけを片付ける。失敗途中の結果も集合から回収する。
  for (const setId of createdSets) {
    try { for (const item of (await list({filter_set: setId})).items) createdShas.add(item.sha1); } catch (_) {}
  }
  for (const name of [albumName, controlName, wideName]) {
    await fetch(BASE + '/api/albums/' + encodeURIComponent(name), {method: 'DELETE'}).catch(() => {});
  }
  if (createdShas.size) await api('/api/trash', {shas: [...createdShas]}).catch(error => console.error('cleanup:', error.message));
  if (browser) await browser.close();
});
