'use strict';
// ブラウザ無しで確かめられる芯: 語の切り出し / robots / 部品判定 / 順位付け / ハミング
const assert = require('assert');
const u = require('./../lib/util');
const { parse, Robots } = require('./../lib/robots');
const s = require('./../lib/score');

// terms
assert.deepStrictEqual(u.terms('柴犬の写真。イラスト不可'), ['柴犬']);
assert.ok(u.terms('Impressionist oil paintings, no sketches').includes('impressionist'));
assert.ok(!u.terms('Impressionist oil paintings, no sketches').includes('sketches'));
assert.strictEqual(u.site('upload.wikimedia.org'), 'wikimedia.org');
assert.strictEqual(u.site('www.example.co.jp'), 'example.co.jp');
assert.ok(u.sameSite('https://commons.wikimedia.org/x', 'https://upload.wikimedia.org/y'));
assert.ok(u.isPrivateHost('127.0.0.1') && u.isPrivateHost('10.1.2.3') && u.isPrivateHost('localhost') && !u.isPrivateHost('example.org'));
assert.ok(u.isMediaHost('www.youtube.com') && u.isMediaHost('x.com') && !u.isMediaHost('flickr.com'));
assert.strictEqual(u.normalizeUrl('https://Example.org/a/?utm_source=x#frag'), 'https://example.org/a');
assert.strictEqual(u.hamming('0000000000000000', 'ffffffffffffffff'), 64);
assert.strictEqual(u.hamming('00000000000000f0', '0000000000000000'), 4);

// robots
const g = parse('User-agent: *\nDisallow: /private/\nAllow: /private/ok\nCrawl-delay: 2\n\nUser-agent: fluent_crawler\nDisallow: /nope\n');
assert.strictEqual(g.length, 2);
(async () => {
  const r = new Robots(async () => 'User-agent: *\nDisallow: /private/\nAllow: /private/ok\nDisallow: /*.pdf$\n');
  assert.strictEqual(await r.allowed('https://a.org/public'), true);
  assert.strictEqual(await r.allowed('https://a.org/private/x'), false);
  assert.strictEqual(await r.allowed('https://a.org/private/ok/1'), true);
  assert.strictEqual(await r.allowed('https://a.org/doc.pdf'), false);
  const r2 = new Robots(async () => { throw new Error('net'); });
  assert.strictEqual(await r2.allowed('https://b.org/x'), true);

  // junk image
  const base = { src: 'https://a.org/img/photo.jpg', currentSrc: 'https://a.org/img/photo.jpg', srcset: [], lazy: [], link: '', w: 800, h: 600, nw: 1600, nh: 1200, alt: '', caption: '', context: '', head: '', chrome: false, inMain: true, inFigure: false, cls: '', kind: 'img', hidden: false };
  const opts = { minSide: 300, seenPages: new Map() };
  assert.strictEqual(s.junkImage(base, opts), null);
  assert.strictEqual(s.junkImage({ ...base, src: 'https://a.org/logo.svg', currentSrc: 'https://a.org/logo.svg' }, opts), 'icon');
  assert.strictEqual(s.junkImage({ ...base, cls: 'site-logo' }, opts), 'icon');
  assert.strictEqual(s.junkImage({ ...base, chrome: true }, opts), 'nav');
  assert.strictEqual(s.junkImage({ ...base, nw: 120, nh: 120, w: 120, h: 120 }, opts), 'small');
  assert.strictEqual(s.junkImage({ ...base, nw: 120, nh: 120, w: 120, h: 120, srcset: [{ url: 'https://a.org/big.jpg', w: 1200, x: 0 }] }, opts), null, 'srcset に大きい版がある小サムネは候補');
  assert.strictEqual(s.junkImage({ ...base, nw: 1200, nh: 90 }, opts), 'banner');
  const seen = new Map([[base.src, 3]]);
  assert.strictEqual(s.junkImage(base, { minSide: 300, seenPages: seen }), 'repeat');

  // image scoring: goal 一致・本文・大きい > 本文外・小さい
  const t = u.terms('柴犬の写真');
  const a = s.scoreImage({ ...base, alt: '柴犬の子犬', inFigure: true, caption: '庭で遊ぶ柴犬' }, t, opts);
  const b = s.scoreImage({ ...base, inMain: false, nw: 400, nh: 300, w: 200, h: 150 }, t, opts);
  assert.ok(a.score > b.score, `${a.score} > ${b.score}`);

  // candidates: Commons thumb → original first
  const c = s.imageCandidates({ ...base, src: 'https://upload.wikimedia.org/wikipedia/commons/thumb/a/ab/Shiba.jpg/220px-Shiba.jpg', currentSrc: 'https://upload.wikimedia.org/wikipedia/commons/thumb/a/ab/Shiba.jpg/220px-Shiba.jpg', link: 'https://commons.wikimedia.org/wiki/File:Shiba.jpg' });
  assert.strictEqual(c[0], 'https://upload.wikimedia.org/wikipedia/commons/a/ab/Shiba.jpg');

  // links
  const lo = { sameSite: true, startUrl: 'https://a.org/gallery', pageOffsite: false };
  assert.strictEqual(s.junkLink('https://a.org/login', 'https://a.org/', lo), 'skip');
  assert.strictEqual(s.junkLink('https://a.org/x.pdf', 'https://a.org/', lo), 'nonhtml');
  assert.strictEqual(s.junkLink('https://b.org/x', 'https://a.org/', lo), 'offsite');
  assert.strictEqual(s.junkLink('https://a.org/photos/2', 'https://a.org/', lo), null);
  const L = (x) => ({ href: 'https://a.org/p', text: '', title: '', context: '', chrome: false, inMain: true, wrapsImage: false, imgAlt: '', relNext: false, generic: false, y: 0, ...x });
  const so = { startUrl: 'https://a.org/gallery' };
  const next = s.scoreLink(L({ href: 'https://a.org/gallery?page=2', text: '次へ', relNext: true }), t, 1, so);
  const nav = s.scoreLink(L({ href: 'https://a.org/about', text: 'About', chrome: true, generic: true }), t, 1, so);
  const thumb = s.scoreLink(L({ href: 'https://a.org/photo/12', wrapsImage: true, imgAlt: '柴犬' }), t, 1, so);
  assert.ok(next.score > nav.score && thumb.score > nav.score, `${next.score} ${thumb.score} > ${nav.score}`);
  console.log('unit: ok');
})().catch((e) => { console.error(e); process.exit(1); });
