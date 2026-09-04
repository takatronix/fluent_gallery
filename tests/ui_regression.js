// fluent_gallery UI回帰テスト — デプロイ前に必ず流す(2026-09-03、「直すたび別が壊れる」対策)
// 実行: node tests/ui_regression.js  (要: サーバ稼働中 :8790 / 初回 npm i puppeteer-core)
// 検証: フォルダ切替の追い越し / ライトボックス開閉・送り / アスペクト属性 / クロップ /
//       超小型サムネ / 連続削除 / ⭐トグル / 顔IDパネル。テストデータは fixtures を _uitest へ収蔵→最後に掃除。
const puppeteer = require('puppeteer-core');
const path = require('path');

const BASE = process.env.FG_URL || 'http://localhost:8790';
const results = [];
const check = (name, ok, detail = '') => {
  results.push({name, ok, detail});
  console.log(`${ok ? '✅' : '❌'} ${name}${detail ? '  — ' + detail : ''}`);
};

(async () => {
  // テストデータ収蔵
  // 前回が途中で落ちていると掃除が走っていないので、まず残骸を消す(次の実行を巻き添えにしない)
  for (const al of ['_uitest', '_uitest2', '_uitest_b']) await fetch(BASE + '/api/albums/' + al, {method: 'DELETE'});
  for (const src of ['crawl:_uitest', 'crawl:_uitest2', 'crawl:_uitest_b']) { // 前回の画像が残っていると枚数の検算が狂う
    await fetch(BASE + '/api/source/trash', {method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({source: src})});
  }
  for (const ds of ['_uitest_ds', '_uitest_ds2', '_uitest_ds_b']) await fetch(BASE + '/api/datasets/' + ds, {method: 'DELETE'});
  const fx = path.join(__dirname, 'fixtures');
  await fetch(BASE + '/api/ingest', {method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({path: fx, source: 'crawl:_uitest', move: false})});
  await new Promise(r => setTimeout(r, 4000));
  const total = (await (await fetch(BASE + '/api/images?limit=1&source=crawl%3A_uitest')).json()).total;
  check('テストデータ収蔵', total >= 6, `${total}枚`);

  const b = await puppeteer.launch({executablePath: process.env.CHROME || '/usr/bin/google-chrome', headless: 'new',
    args: ['--no-sandbox'], defaultViewport: {width: 1600, height: 900}});

  // 36pxでは200件が数行にしかならない。旧実装は初期pump後もsentinelが700px帯内に残り、
  // IntersectionObserverが再発火せず約1400〜1600件で永久停止した。専用page+合成APIでDBを汚さず再現する。
  const scrollPage = await b.newPage();
  await scrollPage.setViewport({width: 2400, height: 900});
  await scrollPage.evaluateOnNewDocument(() => localStorage.setItem('fg_cell', '340')); // boot中の実API pumpを最小化
  await scrollPage.goto(BASE + '/#t=lib&k=all', {waitUntil: 'networkidle2', timeout: 60000});
  await scrollPage.waitForFunction(() => typeof reload === 'function' && document.querySelector('.cell') &&
    !loadMore._busy && !fillViewport._promise, {timeout: 30000});
  const infiniteScroll = await scrollPage.evaluate(async () => {
    const realFetch = window.fetch.bind(window), realCellHtml = cellHtml;
    const fakeTotal = 10000;
    window.fetch = (input, init) => {
      const u = new URL(typeof input === 'string' ? input : input.url, location.href);
      if (u.pathname === '/api/images' && u.searchParams.get('_scrolltest') === '1') {
        const offset = Math.max(0, +(u.searchParams.get('offset') || 0));
        const limit = Math.max(1, +(u.searchParams.get('limit') || 200));
        const end = Math.min(fakeTotal, offset + limit);
        const fake = [];
        for (let i = offset; i < end; i++) fake.push({
          sha1: i.toString(16).padStart(40, '0'), w: 120, h: 120,
          source: '_scrolltest', keep: 0, erev: null, attrs: 0,
        });
        return Promise.resolve(new Response(JSON.stringify({total: fakeTotal, items: fake}), {
          status: 200, headers: {'Content-Type': 'application/json'},
        }));
      }
      return realFetch(input, init);
    };
    // 合成画像のHTTP requestは不要。セルの高さ・仮想window・ページングだけ実コードで検査する。
    cellHtml = it => `<div class="cell lite in" id="c_${it.sha1}"></div>`;
    applyCellLayout(36);
    // 実サーバで収集中の新着がliveDropされない場所にし、合成pageだけを測る。
    loc = {type: 'source', key: '_scrolltest', criteria: {_scrolltest: '1'}};
    await reload();
    const wait = async (fn, ms = 10000) => {
      const start = performance.now();
      while (performance.now() - start < ms) {
        if (fn()) return true;
        await new Promise(resolve => setTimeout(resolve, 25));
      }
      return false;
    };
    await wait(() => !loadMore._busy && !fillViewport._promise);
    const wrap = $('gridwrap'), more = $('more');
    const covered = () => items.length >= total ||
      more.getBoundingClientRect().top > wrap.getBoundingClientRect().bottom + LOAD_MORE_MARGIN;
    const snapshotRange = async () => {
      // 仮想窓の非同期growが落ち着いてから、テスト側だけ実DOM矩形で表示範囲を独立検算する。
      for (let i = 0; i < 8; i++) await new Promise(resolve => requestAnimationFrame(resolve));
      const wr = wrap.getBoundingClientRect(), visible = [];
      for (const el of $('grid').children) {
        const r = el.getBoundingClientRect();
        if (r.bottom > wr.top && r.top < wr.bottom)
          visible.push(parseInt(el.id.slice(2), 16) + 1);
      }
      const match = $('resultcount').textContent.match(/^表示中 ([\d,]+)–([\d,]+) \/ ([\d,]+)枚$/);
      const pageMatch = $('pagecount').textContent.match(/^([\d,]+) \/ ([\d,]+)$/);
      const pageSize = gridCols() * Math.max(1, Math.ceil(wrap.clientHeight / cellPitch()));
      return {
        text: $('resultcount').textContent,
        first: match ? +match[1].replaceAll(',', '') : 0,
        last: match ? +match[2].replaceAll(',', '') : 0,
        total: match ? +match[3].replaceAll(',', '') : 0,
        domFirst: visible.length ? Math.min(...visible) : 0,
        domLast: visible.length ? Math.max(...visible) : 0,
        loaded: items.length,
        pageText: $('pagecount').textContent,
        page: pageMatch ? +pageMatch[1].replaceAll(',', '') : 0,
        pages: pageMatch ? +pageMatch[2].replaceAll(',', '') : 0,
        expectedPage: visible.length ? Math.floor(((Math.min(...visible) - 1) +
          Math.floor((Math.max(...visible) - Math.min(...visible)) / 2)) / pageSize) + 1 : 0,
        expectedPages: Math.ceil(fakeTotal / pageSize),
      };
    };
    const initial = items.length, initiallyCovered = covered();
    const ranges = [await snapshotRange()];
    const growth = [];
    for (let pass = 0; pass < 3; pass++) {
      const before = items.length;
      wrap.scrollTop = wrap.scrollHeight;
      const grew = await wait(() => items.length > before);
      await wait(() => !loadMore._busy && !fillViewport._promise);
      growth.push(grew && items.length > before && covered());
      ranges.push(await snapshotRange());
    }
    const unique = new Set(items.map(it => it.sha1)).size === items.length;
    const end = items.length;
    window.fetch = realFetch; cellHtml = realCellHtml; localStorage.removeItem('fg_cell');
    return {initial, end, initiallyCovered, growth, unique, ranges};
  });
  await scrollPage.close();
  const rangeOk = infiniteScroll.ranges.every(r => r.first === r.domFirst && r.last === r.domLast &&
    r.total === 10000 && r.last <= r.loaded && r.page === r.expectedPage && r.pages === r.expectedPages) &&
    infiniteScroll.ranges.slice(1).every((r, i) => r.first > infiniteScroll.ranges[i].first);
  check('最小サムネ無限スクロール継続', infiniteScroll.initiallyCovered &&
    infiniteScroll.growth.every(Boolean) && infiniteScroll.end >= infiniteScroll.initial + 600 &&
    infiniteScroll.unique,
    `${infiniteScroll.initial}→${infiniteScroll.end} (${infiniteScroll.growth.join('/')})`);
  check('現在の表示位置/総数', rangeOk,
    infiniteScroll.ranges.map(r => `${r.pageText} ${r.text}`).join(' → '));

  const p = await b.newPage();
  const jsErrors = [];
  p.on('pageerror', e => jsErrors.push(e.message));
  await p.goto(BASE + '/', {waitUntil: 'networkidle2', timeout: 60000});
  await p.waitForSelector('.cell', {timeout: 30000});

  // 条件が満たされるまで待つ(固定sleepはサーバ負荷でブレて偽の失敗を出す=門番が狼少年になる)
  await p.evaluate(() => {
    window.until = async (fn, ms = 6000) => {
      const t0 = Date.now();
      while (Date.now() - t0 < ms) { if (fn()) return true; await new Promise(r => setTimeout(r, 50)); }
      return false;
    };
  });

  // 1) フォルダ切替の追い越し: 高速切替後、最終的に指定フォルダの内容になっている
  const sw = await p.evaluate(async () => {
    go({type: 'lib', key: 'all', criteria: {}});
    go({type: 'source', key: 'crawl:_uitest', criteria: {source: 'crawl:_uitest'}}); // 待たず連打
    await until(() => items.length >= 6 && items.every(x => x.source === 'crawl:_uitest'));
    await new Promise(r => setTimeout(r, 300)); // 追い越し応答が後から来ないか見届ける
    return {n: items.length, allUitest: items.every(x => x.source === 'crawl:_uitest')};
  });
  check('フォルダ切替(追い越しなし)', sw.allUitest && sw.n >= 6, `items=${sw.n}`);

  // 2) ライトボックス開く: 正しい画像が実寸で表示される
  const open = await p.evaluate(async () => {
    openLb(0);
    // 中身が入り、開くモーフの受け渡し(opacity復帰)まで完了するのを待つ
    await until(() => { const i = $('lbimg');
      return i.dataset.tier && i.getBoundingClientRect().width > 100 && getComputedStyle(i).opacity === '1'; });
    const i = $('lbimg'); const r = i.getBoundingClientRect();
    return {w: r.width, op: getComputedStyle(i).opacity, sha: items[lbIdx].sha1, srcOk: i.src.includes(items[lbIdx].sha1)};
  });
  check('ライトボックス表示', open.w > 100 && open.op === '1' && open.srcOk, `w=${open.w.toFixed(0)}`);

  // 3) 送り: 直後(仮サムネ段階)から最後まで「ビューポートへアスペクトフィット」の同一サイズ
  //    (小原本も拡大して見せる統一ルール 2026-09-03。途中でサイズが揺れたら退行)
  const nav = await p.evaluate(async () => {
    lbGo(1);
    await until(() => $('lbimg').dataset.tier); // 何かが表示された時点(サムネ即時のはず)
    const it = items[lbIdx];
    const ar = it.w / it.h;
    let bw = Math.min(innerWidth * .94, innerHeight * .74 * ar);
    if (bw / ar > innerHeight * .74) bw = innerHeight * .74 * ar;
    const early = $('lbimg').getBoundingClientRect().width;
    await until(() => $('lbimg').dataset.tier === 'pv' || $('lbimg').dataset.tier === 'full');
    return {early, expect: bw, w: $('lbimg').getBoundingClientRect().width};
  });
  const fitOk = w => Math.abs(w - nav.expect) < nav.expect * 0.1;
  check('送り(常にフィットサイズ・揺れなし)', fitOk(nav.early) && fitOk(nav.w),
    `直後${nav.early.toFixed(0)}/最終${nav.w.toFixed(0)}/期待${nav.expect.toFixed(0)}`);

  // 4) クロップ: 確定してもライトボックスが閉じず、編集が保存され、画像が表示され続ける
  await p.evaluate(() => { edToggle(); cropToggle(); });
  const rc = await p.evaluate(() => { const r = $('lbimg').getBoundingClientRect(); return {l: r.left, t: r.top, w: r.width, h: r.height}; });
  await p.mouse.move(rc.l + rc.w * .25, rc.t + rc.h * .25);
  await p.mouse.down();
  await p.mouse.move(rc.l + rc.w * .75, rc.t + rc.h * .75, {steps: 5});
  await p.mouse.up();
  const crop = await p.evaluate(async () => {
    const sha = items[lbIdx].sha1;
    let ops = [];
    await until(async () => false, 300); // 確定処理が走り出すのを一拍待つ
    for (let k = 0; k < 40; k++) { // 保存完了をポーリング(サーバが混んでいても待つ)
      const m = await (await fetch('/api/edits/' + sha)).json();
      ops = m.edits.map(e => e.op);
      if (ops.includes('crop')) break;
      await new Promise(r => setTimeout(r, 200));
    }
    return {open: $('lb').classList.contains('show'), ops, w: $('lbimg').getBoundingClientRect().width};
  });
  check('クロップ', crop.open && crop.ops.includes('crop') && crop.w > 50, `w=${crop.w.toFixed(0)} ops=${crop.ops}`);
  await p.evaluate(async () => { edClear(); await new Promise(r => setTimeout(r, 600)); edToggle(); });

  // 5) ⭐トグル: 付けて外して状態が往復する
  const star = await p.evaluate(async () => {
    closeLb(); await new Promise(r => setTimeout(r, 400));
    const el = document.querySelector('.cell .badge'); const sha = items[0].sha1;
    await cellKeep(sha, el); const on = items[0].keep;
    await cellKeep(sha, el); const off = items[0].keep;
    return {ok: on === 1 && off === 0};
  });
  check('⭐トグル往復', star.ok);

  // 5a) 一覧は画像を含めずgrid最小メタだけ。サムネはSHA URL+immutable cacheで再利用
  const leanApi = await p.evaluate(async () => {
    const [gridRes, fullRes] = await Promise.all([
      fetch('/api/images?limit=20&view=grid'), fetch('/api/images?limit=20'),
    ]);
    const [grid, full] = await Promise.all([gridRes.json(), fullRes.json()]);
    const keys = Object.keys(grid.items[0] || {}).sort();
    const sha = grid.items[0]?.sha1;
    const imageRes = await fetch('/micro/' + sha, {cache: 'reload'});
    const atlasRes = await fetch('/atlas/' + grid.atlas?.id, {cache: 'reload'});
    const atlasSize = await new Promise(resolve => {
      const im = new Image();
      im.onload = () => resolve({w: im.naturalWidth, h: im.naturalHeight});
      im.onerror = () => resolve({w: 0, h: 0});
      im.src = '/atlas/' + grid.atlas?.id;
    });
    return {keys, attrs: grid.items.every(it => Number.isInteger(it.attrs)), itemCount: grid.items.length,
      gridBytes: JSON.stringify(grid).length, fullBytes: JSON.stringify(full).length,
      cache: imageRes.headers.get('cache-control') || '', imageType: imageRes.headers.get('content-type') || '',
      atlas: grid.atlas, atlasOk: atlasRes.ok, atlasCache: atlasRes.headers.get('cache-control') || '',
      atlasType: atlasRes.headers.get('content-type') || '', atlasSize};
  });
  check('一覧API軽量化+SHAサムネキャッシュ',
    leanApi.keys.join(',') === 'attrs,erev,h,keep,sha1,source,w' && leanApi.attrs &&
    leanApi.gridBytes < leanApi.fullBytes * .5 && /max-age=31536000/.test(leanApi.cache) &&
    /immutable/.test(leanApi.cache) && leanApi.imageType.startsWith('image/') &&
    leanApi.atlas?.id && leanApi.atlas.cols === 20 && leanApi.atlas.rows === Math.ceil(leanApi.itemCount / 20) &&
    leanApi.atlasOk && /max-age=31536000/.test(leanApi.atlasCache) &&
    /immutable/.test(leanApi.atlasCache) && leanApi.atlasType.startsWith('image/jpeg') &&
    leanApi.atlasSize.w === leanApi.atlas.cols * 120 && leanApi.atlasSize.h === leanApi.atlas.rows * 120,
    `${leanApi.gridBytes}B/${leanApi.fullBytes}B atlas=${leanApi.atlasSize.w}x${leanApi.atlasSize.h}`);

  // 5b) 属性: 権利クリーン/有料/セーフを既存疑似要素の固定記号だけで示す。属性DOMは増やさない
  const attrs = await p.evaluate(() => {
    const probe = {...items[0], sha1: 'f'.repeat(40), cost: 0, rights: 'unknown', nsfw: null,
      caption: '', source: '', ingested: 0, keep: 0};
    delete probe.attrs; // 個別ビット組合せはfull API互換経路で検査する
    const unquote = value => value === 'none' ? '' : value.replace(/^['"]|['"]$/g, '');
    const make = (patch, px, dense = false) => {
      const box = document.createElement('div');
      box.innerHTML = cellHtml({...probe, ...patch}, px);
      const cell = box.firstElementChild;
      cell.querySelector('img')?.removeAttribute('src');
      const hadDense = document.body.classList.contains('dense');
      const hadSelmode = document.body.classList.contains('selmode');
      document.body.classList.toggle('dense', dense);
      document.body.classList.remove('selmode');
      document.body.appendChild(cell);
      const pseudo = getComputedStyle(cell, '::after');
      const out = {mask: [...cell.classList].find(name => /^m[1-7]$/.test(name)) || '',
        marker: unquote(pseudo.content), frame: pseudo.boxShadow,
        legacy: cell.querySelectorAll('.costb,.rclean,.safeb,.attrs').length,
        textOverlays: cell.querySelectorAll('.cap,.newb').length,
        text: cell.textContent.trim(), children: cell.childElementCount, svgs: cell.querySelectorAll('svg').length};
      cell.remove();
      document.body.classList.toggle('dense', hadDense);
      document.body.classList.toggle('selmode', hadSelmode);
      return out;
    };
    const cases = [
      [{}, 0, ''],
      [{rights: 'CC BY 4.0'}, 1, '✓'],
      [{cost: 0.123}, 2, '$'],
      [{rights: 'CC BY 4.0', cost: 0.123}, 3, '✓ $'],
      [{nsfw: 0}, 4, '○'],
      [{rights: 'CC BY 4.0', nsfw: 0}, 5, '✓ ○'],
      [{cost: 0.123, nsfw: 0}, 6, '$ ○'],
      [{rights: 'CC BY 4.0', cost: 0.123, nsfw: 0}, 7, '✓ $ ○'],
    ];
    const deferred = (() => {
      const wasScrolling = gridScrolling;
      gridScrolling = true;
      const box = document.createElement('div');
      box.innerHTML = cellHtml({...probe, rights: 'clean', cost: 0.123, nsfw: 0}, 92);
      gridScrolling = wasScrolling;
      const cell = box.firstElementChild, img = cell.firstElementChild;
      document.body.appendChild(cell);
      const before = {src: img.hasAttribute('src'), deferred: !!img.dataset.src,
        visibility: getComputedStyle(img).visibility};
      const src = img.dataset.src;
      img.dataset.src = '';
      img.setAttribute('src', src);
      const loading = getComputedStyle(img).visibility;
      img.removeAttribute('data-src');
      const after = getComputedStyle(img).visibility;
      cell.remove();
      return {before, loading, after};
    })();
    return {
      normal: cases.map(([patch, mask, expected]) => ({...make(patch, 172), expectedMask: mask ? `m${mask}` : '', expected})),
      cleanLiteral: make({rights: 'clean'}, 172),
      negative: make({rights: '', cost: 0, nsfw: 1}, 172),
      small: make({rights: 'clean', cost: 0.123, nsfw: 0}, 92),
      dense: make({rights: 'clean', cost: 0.123, nsfw: 0}, 172, true),
      deferred,
    };
  });
  const baseShape = `${attrs.normal[0].children}/${attrs.normal[0].svgs}`;
  const attrsOk = attrs.normal.every(x => x.mask === x.expectedMask && x.marker === x.expected && x.frame === 'none' && !x.legacy &&
    !x.textOverlays && !x.text && `${x.children}/${x.svgs}` === baseShape);
  check('サムネ属性(記号のみ/追加DOMなし/小型は無し)', attrsOk &&
    attrs.cleanLiteral.mask === 'm1' && attrs.cleanLiteral.marker === '✓' &&
    !attrs.negative.mask && !attrs.negative.marker && !attrs.small.mask &&
    !/[✓$○]/.test(attrs.small.marker) && attrs.small.children === 1 &&
    attrs.dense.mask === 'm7' && !attrs.dense.marker && !attrs.deferred.before.src &&
    attrs.deferred.before.deferred && attrs.deferred.before.visibility === 'hidden' &&
    attrs.deferred.loading === 'hidden' && attrs.deferred.after === 'visible',
    `all=${attrs.normal.at(-1).marker} small=${attrs.small.marker} dense=${attrs.dense.marker}`);

  // 5c) 110px以下の画像専用セル: 文字DOMなし。最小でも左上選択・右上お気に入りを失わない
  await p.evaluate(() => {
    $('cellsize').value = 92;
    setCell(92);
    commitCell();
  });
  await p.waitForFunction(() => {
    const cells = [...document.querySelectorAll('.cell')];
    return cells.length > 0 && cells.every(cell => cell.classList.contains('lite') &&
      cell.childElementCount === 1 && cell.firstElementChild?.tagName === 'IMG' && !cell.textContent.trim());
  });
  const overview = await p.evaluate(() => ({
    cells: document.querySelectorAll('.cell.lite').length,
    attrs: document.querySelectorAll('.cell:is(.m1,.m2,.m3,.m4,.m5,.m6,.m7)').length,
    overlays: document.querySelectorAll('.cell .cap,.cell .costb,.cell .rclean,.cell .safeb,.cell .attrs,.cell .newb,.cell .ck,.cell .badge').length,
    marks: [...document.querySelectorAll('.cell')]
      .filter(cell => /[✓$○]/.test(getComputedStyle(cell, '::after').content)).length,
  }));
  check('小型サムネ(92px/画像のみ)', overview.cells > 0 && overview.attrs === 0 &&
    overview.overlays === 0 && overview.marks === 0,
    `cells=${overview.cells} attrs=${overview.attrs} overlays=${overview.overlays} marks=${overview.marks}`);
  await p.evaluate(async () => {
    $('cellsize').value = 36;
    setCell(36);
    commitCell();
    // 編集テストで単体microへ退避したセルも含め、現ページの新しい不変atlas情報を取り直す。
    await reload(true);
  });
  await p.waitForFunction(() => {
    const imgs = [...document.querySelectorAll('.cell.lite img')];
    return imgs.length > 0 && imgs.every(img => img.dataset.tier === 'atlas' && img.naturalWidth > 0);
  }, {timeout: 30000});
  const atlasCompact = await p.evaluate(() => {
    const imgs = [...document.querySelectorAll('.cell.lite img')];
    return {cells: imgs.length, sources: new Set(imgs.map(im => im.src)).size,
      atlas: imgs.filter(im => im.classList.contains('atlas')).length,
      maxNatural: Math.max(...imgs.map(im => im.naturalWidth || 0))};
  });
  const litePoint = await p.evaluate(() => {
    const r = document.querySelector('.cell.lite').getBoundingClientRect();
    return {x: r.left + 6, y: r.top + 6};
  });
  await p.mouse.click(litePoint.x, litePoint.y);
  const liteStarPoint = await p.evaluate(() => {
    const r = document.querySelector('.cell.lite').getBoundingClientRect();
    return {x: r.right - 6, y: r.top + 6, sha: items[0].sha1};
  });
  await p.mouse.click(liteStarPoint.x, liteStarPoint.y);
  await p.waitForFunction(sha => items.find(it => it.sha1 === sha)?.keep === 1 &&
    document.getElementById('c_' + sha)?.classList.contains('keep'), {}, liteStarPoint.sha);
  await p.mouse.click(liteStarPoint.x, liteStarPoint.y);
  await p.waitForFunction(sha => items.find(it => it.sha1 === sha)?.keep === 0 &&
    !document.getElementById('c_' + sha)?.classList.contains('keep'), {}, liteStarPoint.sha);
  // remote側の画像右クリック機能とlite(img 1子)を接合した箇所。メニュー操作でも状態を二重反転しない。
  const tinyCtx = await p.evaluate(async () => {
    const sha = items[0].sha1, cell = document.getElementById('c_' + sha);
    const open = () => cell.dispatchEvent(new MouseEvent('contextmenu',
      {bubbles: true, cancelable: true, clientX: 220, clientY: 120}));
    open();
    const labels = [...document.querySelectorAll('#ctxm .it')].map(x => x.textContent.trim());
    [...document.querySelectorAll('#ctxm .it')].find(x => x.textContent.includes('お気に入り'))?.click();
    const on = await until(() => items[0].keep === 1 && cell.classList.contains('keep'));
    open();
    const offLabel = [...document.querySelectorAll('#ctxm .it')].some(x => x.textContent.includes('お気に入りを外す'));
    [...document.querySelectorAll('#ctxm .it')].find(x => x.textContent.includes('お気に入りを外す'))?.click();
    const off = await until(() => items[0].keep === 0 && !cell.classList.contains('keep'));
    ctxClose();
    return {labels, on, off, offLabel};
  });
  const lite = await p.evaluate(() => {
    const cells = [...document.querySelectorAll('.cell.lite')];
    return {selected: sel.size === 1, cells: cells.length, unique: new Set(cells.map(cell => cell.id)).size,
      windowCount: vEnd - vStart};
  });
  check('超小型サムネ(atlas/軽量セル/選択/⭐)', lite.selected && lite.cells === lite.unique &&
    lite.cells === lite.windowCount && atlasCompact.atlas === atlasCompact.cells &&
    atlasCompact.sources <= Math.ceil(atlasCompact.cells / 200) && atlasCompact.maxNatural > 120,
    `cells=${lite.cells} atlasURL=${atlasCompact.sources}`);
  check('超小型サムネ右クリック(保存/zip/⭐往復)', tinyCtx.labels.some(x => x.includes('原本を保存')) &&
    tinyCtx.labels.some(x => x.includes('zipで保存')) && tinyCtx.on && tinyCtx.off && tinyCtx.offLabel,
    tinyCtx.labels.join(' | '));
  const patternSwitch = await p.evaluate(async () => {
    clearSel();
    applyPattern(0); // 俯瞰で開始
    await until(() => [...document.querySelectorAll('.cell.lite img')].every(im => im.complete && im.naturalWidth));
    const shas = items.slice(0, 6).map(it => it.sha1);
    const watch = i => new Promise(resolve => {
      const refs = new Map(shas.map(sha => [sha, document.querySelector(`#c_${sha} img`)]));
      let frames = 0, blankFrames = 0;
      applyPattern(i);
      const tick = () => {
        const blank = shas.some(sha => {
          const im = document.querySelector(`#c_${sha} img`);
          return !im || !im.complete || !im.naturalWidth || getComputedStyle(im).visibility !== 'visible';
        });
        if (blank) blankFrames++;
        if (++frames < 40) requestAnimationFrame(tick);
        else resolve({blankFrames, sameImage:shas.every(sha => document.querySelector(`#c_${sha} img`) === refs.get(sha))});
      };
      requestAnimationFrame(tick);
    });
    const standard = await watch(1);
    const normalChildren = [...document.querySelectorAll('.cell')].every(c => !c.classList.contains('lite') && c.childElementCount === 3);
    const overview = await watch(0);
    const liteChildren = [...document.querySelectorAll('.cell')].every(c => c.classList.contains('lite') && c.childElementCount === 1);
    return {standard, overview, normalChildren, liteChildren};
  });
  check('俯瞰⇄標準(画像DOM保持/ちらつきなし)', patternSwitch.standard.sameImage &&
    patternSwitch.overview.sameImage && patternSwitch.standard.blankFrames === 0 &&
    patternSwitch.overview.blankFrames === 0 && patternSwitch.normalChildren && patternSwitch.liteChildren,
    `blank=${patternSwitch.standard.blankFrames}/${patternSwitch.overview.blankFrames}`);
  await p.evaluate(() => applyPattern(1));
  await p.waitForFunction(() => document.querySelector('.cell:not(.lite)'));
  const normalCtx = await p.evaluate(() => {
    const cell = document.querySelector('.cell:not(.lite)');
    cell.dispatchEvent(new MouseEvent('contextmenu', {bubbles: true, cancelable: true, clientX: 300, clientY: 160}));
    const labels = [...document.querySelectorAll('#ctxm .it')].map(x => x.textContent.trim());
    ctxClose();
    return labels;
  });
  check('標準サムネ右クリック保持', normalCtx.some(x => x.includes('原本を保存')) &&
    normalCtx.some(x => x.includes('zipで保存')), normalCtx.join(' | '));

  // 6) 顔IDパネル: 開閉と枠掃除(feature "faceid" 無効ビルドではUIが無いのでskip)
  const caps = await (await fetch(BASE + '/api/caps')).json().catch(() => ({faceid: true}));
  const face = !caps.faceid ? {shown: true, cleaned: true, skipped: true} : await p.evaluate(async () => {
    openLb(0); await new Promise(r => setTimeout(r, 800));
    await lbFacePanel(); await new Promise(r => setTimeout(r, 400));
    const shown = $('lbfacebox').style.display !== 'none';
    closeLb();
    return {shown, cleaned: document.querySelectorAll('.facebox').length === 0};
  });
  if (face.skipped) console.log('  - 顔IDパネル開閉: このビルドは顔ID無効(skip)');
  else check('顔IDパネル開閉', face.shown && face.cleaned);

  // 7) 押しっぱなし送り: 高速連打で流しても、静止したら正しい画像がすぐ出る
  //    (中間画像の原寸DLが回線を塞ぎ「最初の画像が出続ける」退行の再発検知 2026-09-03)
  const hold = await p.evaluate(async () => {
    openLb(0); await new Promise(r => setTimeout(r, 600));
    for (let k = 0; k < 12; k++) { lbGo(1); await new Promise(r => setTimeout(r, 40)); }
    const want = items[lbIdx].sha1;
    await new Promise(r => setTimeout(r, 900));
    const i = $('lbimg');
    const ok = i.src.includes(want) && !!i.dataset.tier;
    closeLb();
    return {ok, src: i.src.slice(-46)};
  });
  check('押しっぱなし送り(最終画像が出る)', hold.ok, hold.src);

  // 8) キーボードカーソル: 矢印移動→Spaceで開く→Spaceで閉じる→Shift+矢印で範囲選択
  const kb = await p.evaluate(() => new Promise(async done => {
    // 前項のcloseLb()は閉じアニメ完了(240ms)後に'show'を外す。閉じ切る前にキーを打つとライトボックス側に食われる
    for (let t = 0; t < 30 && $('lb').classList.contains('show'); t++) await new Promise(r => setTimeout(r, 100));
    const key = (k, opts) => document.dispatchEvent(new KeyboardEvent('keydown', {key: k, bubbles: true, ...opts}));
    key('ArrowRight'); key('ArrowRight');
    await new Promise(r => setTimeout(r, 200));
    const curOk = curSha === items[Math.max(0, vStart) + 1]?.sha1 || curIndex() >= 0;
    key(' ');
    await new Promise(r => setTimeout(r, 800));
    const opened = $('lb').classList.contains('show');
    key(' ');
    await new Promise(r => setTimeout(r, 500));
    const closed = !$('lb').classList.contains('show');
    key('ArrowRight', {shiftKey: true}); key('ArrowRight', {shiftKey: true});
    await new Promise(r => setTimeout(r, 200));
    const selN = sel.size;
    sel.clear(); document.querySelectorAll('.cell.sel').forEach(x => x.classList.remove('sel'));
    done({curOk, opened, closed, selN});
  }));
  check('キーボード(矢印/Space開閉/Shift範囲選択)', kb.curOk && kb.opened && kb.closed && kb.selN >= 2,
    `cur=${kb.curOk} open=${kb.opened} close=${kb.closed} sel=${kb.selN}`);

  // 9) 連続削除: 5連打で5枚消え、詰まらない
  const del = await p.evaluate(async () => {
    openLb(0); await until(() => $('lbimg').dataset.tier);
    const start = items.length;
    for (let i = 0; i < 5; i++) { lbDel(); await new Promise(r => setTimeout(r, 120)); }
    await new Promise(r => setTimeout(r, 2500));
    return {start, end: items.length};
  });
  check('連続削除(5連打)', del.start - del.end === 5, `${del.start}→${del.end}`);

  // 10) フォルダ改名: 名前を変えると中身(画像のsource)も一緒に引っ越す
  const ren = await p.evaluate(async () => {
    const mk = (name, folder) => fetch('/api/albums', {method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({name, folder, goal: '', criteria: {source: 'crawl:' + name}, agent: {}, keywords: [], engines: []})});
    await mk('_uitest', '');
    await mk('_uitest_b', '_uitest棚');
    await loadAlbums();
    const before = (await (await fetch('/api/images?limit=1&source=crawl%3A_uitest')).json()).total;
    const row = document.querySelector('.nav[data-album="_uitest"]');
    row.querySelector('.nm').dispatchEvent(new MouseEvent('dblclick', {bubbles: true}));
    const inp = row.querySelector('input.rn');
    const shown = !!inp;
    inp.value = '_uitest2';
    inp.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
    await until(() => albumsCache.some(a => a.name === '_uitest2'), 15000);
    const after = (await (await fetch('/api/images?limit=1&source=crawl%3A_uitest2')).json()).total;
    return {shown, before, after, gone: !albumsCache.some(a => a.name === '_uitest')};
  });
  check('フォルダ改名(中身ごと引っ越す)', ren.shown && ren.gone && ren.before > 0 && ren.after === ren.before,
    `${ren.before}枚→${ren.after}枚`);

  // 10b) 改名を開いたまま別の行の✎を押せる(取消時にサイドバーを描き直すと押せなくなっていた)
  // 前の検査(連続削除)がライトボックスを開いたままなので閉じる。
  // 開いていると全面オーバーレイがサイドバーを覆い、実マウスのクリックが届かない
  await p.evaluate(() => closeLb());
  await new Promise(r => setTimeout(r, 500));
  // 操作アイコンはホバーで出て、その瞬間に行の間隔も詰まる(枚数が隠れる)。
  // だから「ホバーしてから座標を測って押す」順でないと、ズレた場所を押して何も起きない
  const penClick = async sel => {
    const home = await p.evaluate(s => {
      const row = document.querySelector(s);
      if (!row) return null;
      row.scrollIntoView({block: 'center'});
      const r = row.getBoundingClientRect();
      return {x: r.x + 20, y: r.y + r.height / 2};
    }, sel);
    if (!home) return false;
    await p.mouse.move(home.x, home.y); // ホバー
    await new Promise(r => setTimeout(r, 150));
    const pt = await p.evaluate(s => {
      const d = [...document.querySelector(s).querySelectorAll('.del')].find(x => x.title.startsWith('名前を変える'));
      const r = d?.getBoundingClientRect();
      return r && r.width ? {x: r.x + r.width / 2, y: r.y + r.height / 2} : null;
    }, sel);
    if (!pt) return false;
    await p.mouse.click(pt.x, pt.y); // 実マウスで押す(編集中の入力のblurとの順番を本番どおりに再現)
    return true;
  };
  await penClick('.nav[data-album="_uitest_b"]');
  await new Promise(r => setTimeout(r, 300));
  const open1 = await p.evaluate(() => document.querySelector('input.rn')?.closest('.nav')?.dataset.album);
  await penClick('.nav[data-album="_uitest2"]'); // 開いたまま別の✎を押す
  await new Promise(r => setTimeout(r, 400));
  const open2 = await p.evaluate(() => document.querySelector('input.rn')?.closest('.nav')?.dataset.album);
  await p.keyboard.press('Escape');
  await new Promise(r => setTimeout(r, 300));
  const closed = await p.evaluate(() => !document.querySelector('input.rn') && !renaming);
  check('✎連打(開いたまま別の行も押せる)', open1 === '_uitest_b' && open2 === '_uitest2' && closed,
    `${open1} → ${open2}`);

  // 11) D&Dでグループへ移動
  const dnd = await p.evaluate(async () => {
    const drag = (fromSel, toSel) => {
      const dt = new DataTransfer();
      const a = document.querySelector(fromSel), b = document.querySelector(toSel);
      a.dispatchEvent(new DragEvent('dragstart', {bubbles: true, dataTransfer: dt}));
      b.dispatchEvent(new DragEvent('dragover', {bubbles: true, dataTransfer: dt}));
      const lit = b.classList.contains('dropon') || b.classList.contains('mergeon');
      b.dispatchEvent(new DragEvent('drop', {bubbles: true, dataTransfer: dt}));
      a.dispatchEvent(new DragEvent('dragend', {bubbles: true, dataTransfer: dt}));
      return lit;
    };
    const lit = drag('.nav[data-album="_uitest2"]', '.nav[data-grp="_uitest棚"]');
    await until(() => albumsCache.find(a => a.name === '_uitest2')?.folder === '_uitest棚', 8000);
    const moved = albumsCache.find(a => a.name === '_uitest2')?.folder;
    // 12) グループ改名: 中のフォルダのパスが全部ついてくる
    const g = document.querySelector('.nav[data-grp="_uitest棚"]');
    g.querySelector('.nm').dispatchEvent(new MouseEvent('dblclick', {bubbles: true}));
    const gi = g.querySelector('input.rn');
    gi.value = '_uitest棚2';
    gi.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
    await until(() => albumsCache.filter(a => a.folder === '_uitest棚2').length === 2, 8000);
    return {lit, moved, renamed: albumsCache.filter(a => a.folder === '_uitest棚2').map(a => a.name).sort()};
  });
  check('D&Dでグループへ移動', dnd.lit && dnd.moved === '_uitest棚', `folder=${dnd.moved}`);
  check('グループ改名(中のフォルダごと)', dnd.renamed.length === 2, dnd.renamed.join(','));

  // 13) 合流: 確認ポップアップ→やめるで何も起きない→もう一度で合流(画像は消えない)
  const mg = await p.evaluate(async () => {
    const dropAlbum = () => {
      const dt = new DataTransfer();
      const a = document.querySelector('.nav[data-album="_uitest2"]');
      const b = document.querySelector('.nav[data-album="_uitest_b"]');
      a.dispatchEvent(new DragEvent('dragstart', {bubbles: true, dataTransfer: dt}));
      b.dispatchEvent(new DragEvent('dragover', {bubbles: true, dataTransfer: dt}));
      const warn = b.classList.contains('mergeon');
      b.dispatchEvent(new DragEvent('drop', {bubbles: true, dataTransfer: dt}));
      a.dispatchEvent(new DragEvent('dragend', {bubbles: true, dataTransfer: dt}));
      return warn;
    };
    const warn = dropAlbum();
    await until(() => !!$('cfovl'), 4000);
    const popup = !!$('cfovl');
    $('cfovl').querySelector('[data-no]').click();          // やめる
    await new Promise(r => setTimeout(r, 400));
    const cancelled = !$('cfovl') && albumsCache.some(a => a.name === '_uitest2');
    // 前段の連続削除がまだ落ちきっていないと枚数が動く(偽の失敗になる)ので、止まるまで待つ
    const count = async src => (await (await fetch('/api/images?limit=1&source=' + encodeURIComponent(src))).json()).total;
    let before = await count('crawl:_uitest2');
    for (let k = 0; k < 20; k++) {
      await new Promise(r => setTimeout(r, 400));
      const now = await count('crawl:_uitest2');
      if (now === before) break;
      before = now;
    }
    dropAlbum();
    await until(() => !!$('cfovl'), 4000);
    $('cfovl').querySelector('[data-yes]').click();         // 合流する
    await until(() => !albumsCache.some(a => a.name === '_uitest2'), 15000);
    const after = await count('crawl:_uitest_b');
    const leftover = await count('crawl:_uitest2'); // 元のバケツに置き去りが無いこと
    return {warn, popup, cancelled, before, after, leftover, gone: !albumsCache.some(a => a.name === '_uitest2')};
  });
  check('合流の確認ポップアップ(やめる=無変更)', mg.warn && mg.popup && mg.cancelled);
  check('合流(画像は消えず移る)', mg.gone && mg.before > 0 && mg.after === mg.before && mg.leftover === 0,
    `${mg.before}枚→${mg.after}枚 置き去り${mg.leftover}`);

  // 14) 出荷(データセット)の棚も同じように整理できる+木をまたぐD&Dは無効
  const dsT = await p.evaluate(async () => {
    const imgs = (await (await fetch('/api/images?limit=2')).json()).items.map(i => i.sha1);
    const mk = (name, folder) => fetch('/api/datasets', {method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({name, folder, shas: imgs})}); // q は flatten なので送らない(送ると422)
    await mk('_uitest_ds', '');
    await mk('_uitest_ds_b', '_uitest棚d');
    await loadDatasets();
    const row = document.querySelector('.nav[data-ds="_uitest_ds"]');
    row.querySelector('.nm').dispatchEvent(new MouseEvent('dblclick', {bubbles: true}));
    const inp = row.querySelector('input.rn');
    inp.value = '_uitest_ds2';
    inp.dispatchEvent(new KeyboardEvent('keydown', {key: 'Enter', bubbles: true}));
    await until(() => datasetsCache.some(d => d.name === '_uitest_ds2'), 12000);
    const renamed = datasetsCache.some(d => d.name === '_uitest_ds2') && !datasetsCache.some(d => d.name === '_uitest_ds');
    // 棚へD&D
    const drag = (from, to) => {
      const dt = new DataTransfer();
      from.dispatchEvent(new DragEvent('dragstart', {bubbles: true, dataTransfer: dt}));
      to?.dispatchEvent(new DragEvent('dragover', {bubbles: true, dataTransfer: dt}));
      const lit = !!to?.classList.contains('dropon');
      if (lit) to.dispatchEvent(new DragEvent('drop', {bubbles: true, dataTransfer: dt}));
      from.dispatchEvent(new DragEvent('dragend', {bubbles: true, dataTransfer: dt}));
      return lit;
    };
    const lit = drag(document.querySelector('.nav[data-ds="_uitest_ds2"]'),
                     document.querySelector('#nav_datasets .nav[data-grp="_uitest棚d"]'));
    await until(() => datasetsCache.find(d => d.name === '_uitest_ds2')?.folder === '_uitest棚d', 8000);
    const moved = datasetsCache.find(d => d.name === '_uitest_ds2')?.folder;
    // 木をまたぐ(データセット→フォルダ側のグループ)は受け付けない
    const crossLit = drag(document.querySelector('.nav[data-ds="_uitest_ds2"]'),
                          document.querySelector('#nav_folders .nav[data-grp]'));
    return {renamed, lit, moved, crossLit};
  });
  check('データセットの改名/棚へD&D', dsT.renamed && dsT.lit && dsT.moved === '_uitest棚d', `folder=${dsT.moved}`);
  check('木をまたぐD&Dは無効', dsT.crossLit === false);

  // 15) 右クリックメニュー: 出て、名前を変えるが効いて、Escで閉じる
  const ctx = await p.evaluate(async () => {
    const fire = sel => {
      const el = document.querySelector(sel);
      if (!el) return false;
      const r = el.getBoundingClientRect();
      el.dispatchEvent(new MouseEvent('contextmenu', {bubbles: true, clientX: r.x + 30, clientY: r.y + 8}));
      return true;
    };
    fire('.nav[data-ds="_uitest_ds2"]');
    const shown = !!$('ctxm');
    const labels = [...($('ctxm')?.querySelectorAll('.it') || [])].map(e => e.textContent.trim());
    const inside = $('ctxm') && $('ctxm').getBoundingClientRect().right <= innerWidth; // 画面外に出ない
    document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}));
    const closed = !$('ctxm');
    // フォルダ行の「名前を変える」を実際に押す
    fire('.nav[data-album="_uitest_b"]');
    const it = [...$('ctxm').querySelectorAll('.it')].find(e => e.textContent.includes('名前を変える'));
    it.click();
    await new Promise(r => setTimeout(r, 300));
    const editing = document.querySelector('input.rn')?.closest('.nav')?.dataset.album;
    const inp = document.querySelector('input.rn');
    if (inp) { inp.dataset.cancel = '1'; inp.blur(); }
    return {shown, labels, inside, closed, editing, menuGone: !$('ctxm')};
  });
  check('右クリックメニュー(表示/Escで閉じる/画面内)', ctx.shown && ctx.closed && ctx.inside && ctx.labels.length >= 3,
    ctx.labels.join(' | '));
  check('右クリック→名前を変える', ctx.editing === '_uitest_b' && ctx.menuGone, `編集中=${ctx.editing}`);

  // 16) iPhone/iPad: ナビとツールバーが画像領域を押し出さず、全UIが到達可能
  const deviceSpecs = [
    {name: 'iPhone縦', width: 390, height: 844, dpr: 3, compact: true},
    {name: 'iPhone横', width: 844, height: 390, dpr: 3, compact: true},
    {name: 'iPad縦', width: 768, height: 1024, dpr: 2, compact: true},
    {name: 'iPad横', width: 1024, height: 768, dpr: 2, compact: false},
  ];
  for (const spec of deviceSpecs) {
    const mp = await b.newPage();
    mp.on('pageerror', e => jsErrors.push(`${spec.name}: ${e.message}`));
    await mp.setViewport({width: spec.width, height: spec.height, deviceScaleFactor: spec.dpr,
      isMobile: true, hasTouch: true});
    await mp.evaluateOnNewDocument(() => {
      localStorage.setItem('fg_cell', '92');
      localStorage.setItem('fg_vp', '0');
    });
    await mp.goto(BASE + '/', {waitUntil: 'networkidle2', timeout: 60000});
    await mp.waitForSelector('.cell', {timeout: 30000});
    const layout = await mp.evaluate(compact => {
      const rect = sel => {
        const el = document.querySelector(sel), r = el.getBoundingClientRect();
        return {x:r.x, y:r.y, width:r.width, height:r.height, right:r.right, bottom:r.bottom,
          scrollWidth:el.scrollWidth, scrollHeight:el.scrollHeight, clientWidth:el.clientWidth, clientHeight:el.clientHeight};
      };
      const side = rect('aside'), main = rect('main'), top = rect('.top'), grid = rect('.gridwrap');
      const first = document.querySelector('.cell').getBoundingClientRect();
      const topEl = document.querySelector('.top');
      topEl.scrollLeft = topEl.scrollWidth;
      const lastControl = [...topEl.children].filter(el => getComputedStyle(el).display !== 'none').at(-1)?.getBoundingClientRect();
      return {compact, side, main, top, grid, first:{top:first.top,bottom:first.bottom},
        docWidth:document.documentElement.scrollWidth, viewport:[innerWidth, innerHeight],
        lastReachable:!lastControl || (lastControl.left >= -1 && lastControl.right <= innerWidth + 1),
        coarse:matchMedia('(pointer:coarse)').matches, filtersReady:$('chipsbtn').textContent.includes('タグ')};
    }, spec.compact);
    const shellOk = layout.compact
      ? layout.side.height <= 70 && Math.abs(layout.main.y - layout.side.bottom) <= 2 &&
        layout.top.height <= 66 && layout.side.scrollHeight <= layout.side.clientHeight + 2
      : layout.side.width >= 160 && layout.side.width <= 280 && layout.main.height >= spec.height - 2 &&
        layout.top.height <= 112;
    check(`${spec.name}(画像領域/横ナビ/タッチ)`, shellOk && layout.coarse && layout.filtersReady &&
      layout.docWidth <= spec.width + 1 && layout.grid.height >= spec.height * .60 &&
      layout.grid.bottom <= spec.height + 1 && layout.first.top >= layout.grid.y - 1 && layout.lastReachable,
      `side=${layout.side.height.toFixed(0)} top=${layout.top.height.toFixed(0)} grid=${layout.grid.height.toFixed(0)}`);

    // タッチの左上選択と、1段の選択バー(実タップイベント)
    if (spec.name === 'iPhone縦') {
      const point = await mp.evaluate(() => {
        const r = document.querySelector('.cell.lite').getBoundingClientRect();
        return {x:r.left + 10, y:r.top + 10};
      });
      await mp.touchscreen.tap(point.x, point.y);
      await mp.waitForFunction(() => sel.size === 1 && $('selbar').classList.contains('show'));
      const selectBar = await mp.evaluate(() => {
        const r = $('selbar').getBoundingClientRect();
        return {height:r.height, bottom:r.bottom, scrollable:$('selbar').scrollWidth > $('selbar').clientWidth};
      });
      check('iPhone選択バー(1段/到達可能)', selectBar.height <= 66 &&
        selectBar.bottom <= spec.height + 1 && selectBar.scrollable,
        `h=${selectBar.height.toFixed(0)}`);
      await mp.evaluate(() => clearSel());
    }

    // フォルダの操作列も1段で横へ逃がし、設定を開いても画像領域を残す。
    await mp.evaluate(() => {
      openFolder('_uitest_b');
      $('viewhead').classList.remove('open');
    });
    await mp.waitForFunction(() => loc.type === 'folder' && getComputedStyle($('viewhead')).display !== 'none');
    const folderHead = await mp.evaluate(async () => {
      const measure = () => {
        const h = $('viewhead'), row = h.querySelector('.row1'), g = $('gridwrap');
        row.scrollLeft = row.scrollWidth;
        const last = row.lastElementChild?.getBoundingClientRect(), rr = row.getBoundingClientRect();
        const hr = h.getBoundingClientRect(), gr = g.getBoundingClientRect();
        return {head:{top:hr.top,bottom:hr.bottom,height:hr.height,scrollHeight:h.scrollHeight,clientHeight:h.clientHeight,
          scrollWidth:h.scrollWidth,clientWidth:h.clientWidth},
          grid:{top:gr.top,bottom:gr.bottom,height:gr.height}, rowVertical:row.scrollHeight > row.clientHeight + 2,
          lastReachable:!last || (last.left >= rr.left - 1 && last.right <= rr.right + 1),
          overflowY:getComputedStyle(h).overflowY};
      };
      await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
      const closed = measure();
      $('viewhead').classList.add('open');
      await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
      return {closed, open:measure()};
    });
    const maxOpen = Math.min(spec.height * .55, 420) + 2;
    check(`${spec.name}フォルダ見出し(閉/展開/到達可能)`, folderHead.closed.head.height <= 72 &&
      !folderHead.closed.rowVertical && folderHead.closed.lastReachable &&
      folderHead.open.head.height <= maxOpen && folderHead.open.head.bottom <= spec.height + 1 &&
      folderHead.open.head.scrollWidth <= folderHead.open.head.clientWidth + 2 &&
      folderHead.open.overflowY === 'auto' && folderHead.open.grid.height >= Math.min(120, spec.height * .20),
      `閉=${folderHead.closed.head.height.toFixed(0)} 開=${folderHead.open.head.height.toFixed(0)} grid=${folderHead.open.grid.height.toFixed(0)}`);
    await mp.evaluate(() => go({type:'lib', key:'all', criteria:{}}, {keepChips:true}));
    await mp.waitForSelector('.cell', {timeout: 30000});

    await mp.evaluate(() => openLb(0));
    await mp.waitForFunction(() => $('lb').classList.contains('show') && !!$('lbimg').dataset.tier, {timeout: 10000});
    await new Promise(r => setTimeout(r, 500));
    const lightbox = await mp.evaluate(() => {
      const lb = $('lb'); lb.scrollTop = 0;
      const imageTop = $('lbimg').getBoundingClientRect().top;
      const scrollable = getComputedStyle(lb).overflowY !== 'hidden';
      lb.scrollTop = lb.scrollHeight;
      const metaBottom = $('lbmeta').getBoundingClientRect().bottom;
      return {imageTop, metaBottom, scrollable, height:lb.clientHeight, scrollHeight:lb.scrollHeight};
    });
    check(`${spec.name}ライトボックス(上下切れなし)`, lightbox.imageTop >= -1 &&
      lightbox.metaBottom <= spec.height + 1 && (spec.compact ? lightbox.scrollable : true),
      `imgY=${lightbox.imageTop.toFixed(0)} metaB=${lightbox.metaBottom.toFixed(0)}`);

    if (spec.name === 'iPhone縦') {
      await mp.evaluate(() => { $('lb').classList.remove('show'); openIngest(); });
      const panels = await mp.evaluate(async () => {
        await new Promise(r => setTimeout(r, 50));
        const ir = $('ingestpanel').getBoundingClientRect();
        closeIngest(); await facesOpen();
        const fr = document.querySelector('#facesovl > div').getBoundingClientRect();
        facesClose();
        return {ingest:{top:ir.top,bottom:ir.bottom,scroll:getComputedStyle($('ingestpanel')).overflowY},
          faces:{top:fr.top,bottom:fr.bottom}};
      });
      check('iPhoneパネル(取込/顔IDが画面内)', panels.ingest.top >= 0 && panels.ingest.bottom <= spec.height + 1 &&
        panels.ingest.scroll === 'auto' && panels.faces.top >= 0 && panels.faces.bottom <= spec.height + 1,
        `ingest=${panels.ingest.top.toFixed(0)}-${panels.ingest.bottom.toFixed(0)}`);

      // 縦→横回転で旧列数と新幅を混ぜず、同じ画像付近に残る
      const beforeRotate = await mp.evaluate(async () => {
        const wrap = $('gridwrap');
        wrap.scrollTop = Math.min(2500, wrap.scrollHeight - wrap.clientHeight - 1);
        await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
        const row = Math.floor(Math.max(0, wrap.scrollTop - contentTop()) / cellPitch());
        const index = row * gridCols();
        dispatchEvent(new Event('orientationchange'));
        return {index, cols:gridCols(), sha:items[index]?.sha1};
      });
      await mp.setViewport({width: 844, height: 390, deviceScaleFactor: 3, isMobile: true, hasTouch: true});
      await mp.waitForFunction(() => setCell._anchor === undefined && !setCell._raf, {timeout: 3000});
      const afterRotate = await mp.evaluate(() => {
        const wrap = $('gridwrap');
        const row = Math.floor(Math.max(0, wrap.scrollTop - contentTop()) / cellPitch());
        const index = row * gridCols();
        return {index, cols:gridCols(), shaVisible:!!document.getElementById('c_' + items[index]?.sha1)};
      });
      check('iPhone縦横回転(閲覧位置を維持)', Math.abs(afterRotate.index - beforeRotate.index) <=
        Math.max(beforeRotate.cols, afterRotate.cols) && afterRotate.shaVisible,
        `index=${beforeRotate.index}→${afterRotate.index}`);
    }
    await mp.close();
  }

  check('JSエラーなし', jsErrors.length === 0, jsErrors.slice(0, 3).join(' | '));
  await b.close();

  // 掃除(テストソースごとゴミ箱へ)
  for (const src of ['crawl:_uitest', 'crawl:_uitest2', 'crawl:_uitest_b']) {
    await fetch(BASE + '/api/source/trash', {method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({source: src})});
  }
  for (const al of ['_uitest', '_uitest2', '_uitest_b']) {
    await fetch(BASE + '/api/albums/' + al, {method: 'DELETE'});
  }
  for (const ds of ['_uitest_ds', '_uitest_ds2', '_uitest_ds_b']) {
    await fetch(BASE + '/api/datasets/' + ds, {method: 'DELETE'});
  }

  const ng = results.filter(r => !r.ok);
  console.log(ng.length ? `\n❌ ${ng.length}件失敗 — デプロイ禁止` : '\n✅ 全部通過 — デプロイOK');
  process.exit(ng.length ? 1 : 0);
})().catch(e => { console.error('ERR', e.message); process.exit(1); });
