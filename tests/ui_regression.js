// fluent_gallery UI回帰テスト — デプロイ前に必ず流す(2026-09-03、「直すたび別が壊れる」対策)
// 実行: node tests/ui_regression.js  (要: サーバ稼働中 :8790 / 初回 npm i puppeteer-core)
// 検証: フォルダ切替の追い越し / ライトボックス開閉・送り / アスペクト属性 / クロップ /
//       連続削除 / ⭐トグル / 顔IDパネル。テストデータは fixtures を _uitest へ収蔵→最後に掃除。
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
