'use strict';
// 小道具: 時間・乱数・ハッシュ・URL・語の切り出し。依存ゼロ
const crypto = require('crypto');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const now = () => Date.now() / 1000;
const jitter = ([lo, hi]) => lo + Math.random() * Math.max(0, hi - lo);
const sha1 = (buf) => crypto.createHash('sha1').update(buf).digest('hex');
const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));

/** 64bit dHash 同士のハミング距離(16 桁 hex) */
function hamming(a, b) {
  if (!a || !b || a.length !== b.length) return 64;
  let d = 0;
  for (let i = 0; i < a.length; i++) {
    let x = parseInt(a[i], 16) ^ parseInt(b[i], 16);
    while (x) { d += x & 1; x >>= 1; }
  }
  return d;
}

// ---- URL ----
const NON_HTML_EXT = /\.(svg|ico|webmanifest|pdf|zip|rar|7z|gz|tar|exe|dmg|pkg|mp4|mp3|m4a|mov|avi|mkv|webm|wav|flac|ogg|css|js|json|xml|rss|atom|txt|csv|doc|docx|xls|xlsx|ppt|pptx|apk|ipa|woff2?|ttf|otf)(\?|#|$)/i;
const IMAGE_EXT = /\.(jpe?g|png|webp|gif|avif|bmp|tiff?)(\?|#|$)/i;

/** 正規化: fragment 除去、末尾スラッシュ揺れ、追跡パラメータ除去、ホスト小文字 */
function normalizeUrl(u, base) {
  let x;
  try { x = new URL(u, base); } catch { return null; }
  if (x.protocol !== 'http:' && x.protocol !== 'https:') return null;
  x.hash = '';
  x.hostname = x.hostname.toLowerCase();
  for (const k of [...x.searchParams.keys()]) {
    if (/^(utm_|fbclid|gclid|ref$|ref_|_ga|mc_cid|mc_eid|yclid|igshid)/i.test(k)) x.searchParams.delete(k);
  }
  if (x.pathname.length > 1 && x.pathname.endsWith('/') && !x.search) x.pathname = x.pathname.slice(0, -1);
  return x.toString();
}

/** eTLD+1 のごく簡易版(co.jp / co.uk / com.au 等の 2 段 TLD を考慮) */
function site(hostname) {
  const h = (hostname || '').toLowerCase().split('.');
  if (h.length <= 2) return h.join('.');
  const two = new Set(['co', 'ne', 'or', 'ac', 'go', 'com', 'net', 'org', 'gov', 'edu', 'ltd', 'plc']);
  if (two.has(h[h.length - 2]) && h[h.length - 1].length === 2) return h.slice(-3).join('.');
  return h.slice(-2).join('.');
}
function sameSite(a, b) {
  try { return site(new URL(a).hostname) === site(new URL(b).hostname); } catch { return false; }
}

const MEDIA_HOSTS = ['youtube.com', 'youtu.be', 'x.com', 'twitter.com', 'twimg.com', 'instagram.com', 'cdninstagram.com',
  'facebook.com', 'fbcdn.net', 'tiktok.com', 'threads.net', 'pinterest.com'];
function isMediaHost(hostname) {
  const h = (hostname || '').toLowerCase();
  return MEDIA_HOSTS.some((m) => h === m || h.endsWith('.' + m));
}

/** 内部アドレス宛ては行かない(SSRF/事故防止)。DNS は引かない(ホスト名の形だけ) */
function isPrivateHost(hostname) {
  const h = (hostname || '').toLowerCase();
  if (!h || h === 'localhost' || h.endsWith('.local') || h.endsWith('.internal')) return true;
  const m = h.match(/^(\d+)\.(\d+)\.(\d+)\.(\d+)$/);
  if (m) {
    const [a, b] = [+m[1], +m[2]];
    return a === 10 || a === 127 || a === 0 || (a === 172 && b >= 16 && b <= 31) || (a === 192 && b === 168) || (a === 169 && b === 254) || (a === 100 && b >= 64 && b < 128);
  }
  return h.startsWith('[') || h === '::1';
}

// ---- 語 ----
const STOP = new Set(['the', 'a', 'an', 'of', 'and', 'or', 'to', 'in', 'on', 'for', 'with', 'by', 'at', 'from', 'is', 'are', 'be', 'this', 'that',
  'photo', 'photos', 'image', 'images', 'picture', 'pictures', 'no', 'not', 'without',
  'の', 'を', 'に', 'は', 'が', 'と', 'で', 'や', 'も', 'な', 'へ', 'から', 'まで', 'より', 'こと', 'もの', 'です', 'ます', 'する', 'した', 'して',
  '写真', '画像', '不可', '以外', '禁止', 'なし', 'ない', 'イラスト', 'ok', 'only', 'real']);
const NEG = /(不可|以外|禁止|なし|ではない|除く|no |not |without |except )/i;

/** goal → 検索語(小文字)。CJK は 2 文字 n-gram も足す(分かち書き無しでも当たるように)。否定節の語は除く */
function terms(text) {
  if (!text) return [];
  const out = new Set();
  // 否定の節(「イラスト不可」「no drawings」)を落とす: 句読点/読点で区切り、否定語を含む節は捨てる
  const clauses = String(text).split(/[。．.,、,;；\n]+/).filter((c) => c.trim() && !NEG.test(c));
  const src = (clauses.length ? clauses : [String(text)]).join(' ').toLowerCase();
  // 日本語は助詞で切ってから(「柴犬の写真」→「柴犬」「写真」)。分かち書きが無くても主語が残る
  const jp = src.replace(/([\p{Script=Han}\p{Script=Katakana}ー]+)([のをにはがとでやもへ])(?=[\p{Script=Han}\p{Script=Katakana}ー]|\s|$)/gu, '$1 ');
  for (const w of jp.split(/[^\p{L}\p{N}ー]+/u)) {
    if (!w || STOP.has(w)) continue;
    if (/^[\p{Script=Han}\p{Script=Hiragana}\p{Script=Katakana}ー]+$/u.test(w)) {
      if (w.length <= 4) { out.add(w); continue; }
      out.add(w);
      for (let i = 0; i + 2 <= w.length; i++) { const g = w.slice(i, i + 2); if (!STOP.has(g)) out.add(g); }
    } else if (w.length >= 2) {
      out.add(w);
      if (w.endsWith('s') && w.length > 3) out.add(w.slice(0, -1)); // 単純な複数形
    }
  }
  return [...out];
}

/** テキストが語群にどれだけ当たるか 0..1(当たった語の割合、長い語は少し重い) */
function textScore(text, termList) {
  if (!termList.length || !text) return 0;
  const t = String(text).toLowerCase();
  let hit = 0, tot = 0;
  for (const w of termList) {
    const wgt = Math.min(2, 0.5 + w.length / 4);
    tot += wgt;
    if (t.includes(w)) hit += wgt;
  }
  return tot ? hit / tot : 0;
}

/** 固有名詞っぽいタグ: goal と見出しから、英数字 3 文字以上/CJK 2 文字以上の語。最大 n */
function tagsFrom(goal, title, n = 5) {
  const out = [];
  for (const w of terms(goal).concat(terms(title || ''))) {
    if (out.length >= n) break;
    if (/^\d+$/.test(w)) continue;
    if (w.length >= 3 || /[\p{Script=Han}\p{Script=Katakana}]/u.test(w)) if (!out.includes(w)) out.push(w);
  }
  return out;
}

function fmtBytes(b) { return b > 1e9 ? (b / 1e9).toFixed(2) + 'GB' : b > 1e6 ? (b / 1e6).toFixed(1) + 'MB' : Math.round(b / 1e3) + 'KB'; }

module.exports = { sleep, now, jitter, sha1, clamp, hamming, normalizeUrl, site, sameSite, isMediaHost, isPrivateHost,
  NON_HTML_EXT, IMAGE_EXT, terms, textScore, tagsFrom, fmtBytes };
