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
  const fx = path.join(__dirname, 'fixtures');
  await fetch(BASE + '/api/ingest', {method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({path: fx, source: 'crawl:_uitest', move: false})});
  await new Promise(r => setTimeout(r, 4000));
  const total = (await (await fetch(BASE + '/api/images?limit=1&source=crawl%3A_uitest')).json()).total;
  check('テストデータ収蔵', total >= 6, `${total}枚`);

  const b = await puppeteer.launch({executablePath: '/usr/bin/google-chrome', headless: 'new',
    args: ['--no-sandbox'], defaultViewport: {width: 1600, height: 900}});
  const p = await b.newPage();
  const jsErrors = [];
  p.on('pageerror', e => jsErrors.push(e.message));
  await p.goto(BASE + '/', {waitUntil: 'networkidle2', timeout: 60000});
  await p.waitForSelector('.cell', {timeout: 30000});

  // 1) フォルダ切替の追い越し: 高速切替後、最終的に指定フォルダの内容になっている
  const sw = await p.evaluate(async () => {
    go({type: 'lib', key: 'all', criteria: {}});
    go({type: 'source', key: 'crawl:_uitest', criteria: {source: 'crawl:_uitest'}}); // 待たず連打
    await new Promise(r => setTimeout(r, 1500));
    return {n: items.length, allUitest: items.every(x => x.source === 'crawl:_uitest')};
  });
  check('フォルダ切替(追い越しなし)', sw.allUitest && sw.n >= 6, `items=${sw.n}`);

  // 2) ライトボックス開く: 正しい画像が実寸で表示される
  const open = await p.evaluate(async () => {
    openLb(0);
    await new Promise(r => setTimeout(r, 1800));
    const i = $('lbimg'); const r = i.getBoundingClientRect();
    return {w: r.width, op: getComputedStyle(i).opacity, sha: items[lbIdx].sha1, srcOk: i.src.includes(items[lbIdx].sha1)};
  });
  check('ライトボックス表示', open.w > 100 && open.op === '1' && open.srcOk, `w=${open.w.toFixed(0)}`);

  // 3) 送り: サイズ属性が残らず、次画像も表示される
  const nav = await p.evaluate(async () => {
    lbGo(1); await new Promise(r => setTimeout(r, 900));
    const i = $('lbimg');
    return {attr: i.hasAttribute('width') || i.hasAttribute('height'), w: i.getBoundingClientRect().width};
  });
  check('送り(アスペクト属性掃除)', !nav.attr && nav.w > 100, `w=${nav.w.toFixed(0)}`);

  // 4) クロップ: 確定してもライトボックスが閉じず、編集が保存され、画像が表示され続ける
  await p.evaluate(() => { edToggle(); cropToggle(); });
  const rc = await p.evaluate(() => { const r = $('lbimg').getBoundingClientRect(); return {l: r.left, t: r.top, w: r.width, h: r.height}; });
  await p.mouse.move(rc.l + rc.w * .25, rc.t + rc.h * .25);
  await p.mouse.down();
  await p.mouse.move(rc.l + rc.w * .75, rc.t + rc.h * .75, {steps: 5});
  await p.mouse.up();
  await new Promise(r => setTimeout(r, 2500));
  const crop = await p.evaluate(async () => {
    const m = await (await fetch('/api/edits/' + items[lbIdx].sha1)).json();
    return {open: $('lb').classList.contains('show'), ops: m.edits.map(e => e.op), w: $('lbimg').getBoundingClientRect().width};
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

  // 6) 顔IDパネル: 開閉と枠掃除
  const face = await p.evaluate(async () => {
    openLb(0); await new Promise(r => setTimeout(r, 800));
    await lbFacePanel(); await new Promise(r => setTimeout(r, 400));
    const shown = $('lbfacebox').style.display !== 'none';
    closeLb();
    return {shown, cleaned: document.querySelectorAll('.facebox').length === 0};
  });
  check('顔IDパネル開閉', face.shown && face.cleaned);

  // 7) 連続削除: 5連打で5枚消え、詰まらない
  const del = await p.evaluate(async () => {
    openLb(0); await new Promise(r => setTimeout(r, 600));
    const start = items.length;
    for (let i = 0; i < 5; i++) { lbDel(); await new Promise(r => setTimeout(r, 120)); }
    await new Promise(r => setTimeout(r, 2500));
    return {start, end: items.length};
  });
  check('連続削除(5連打)', del.start - del.end === 5, `${del.start}→${del.end}`);

  check('JSエラーなし', jsErrors.length === 0, jsErrors.slice(0, 3).join(' | '));
  await b.close();

  // 掃除(テストソースごとゴミ箱へ)
  await fetch(BASE + '/api/source/trash', {method: 'POST', headers: {'Content-Type': 'application/json'},
    body: JSON.stringify({source: 'crawl:_uitest'})});

  const ng = results.filter(r => !r.ok);
  console.log(ng.length ? `\n❌ ${ng.length}件失敗 — デプロイ禁止` : '\n✅ 全部通過 — デプロイOK');
  process.exit(ng.length ? 1 : 0);
})().catch(e => { console.error('ERR', e.message); process.exit(1); });
