'use strict';
// robots.txt(User-agent: * と自分の名前)の Disallow/Allow を守る。origin ごとにキャッシュ。取れなければ許可扱い(標準の慣行)
const UA_TOKEN = 'fluent_crawler';

function parse(text) {
  const groups = []; // {agents:[], rules:[{allow, path}], delay}
  let cur = null;
  for (let raw of text.split(/\r?\n/)) {
    const line = raw.replace(/#.*$/, '').trim();
    if (!line) continue;
    const m = line.match(/^([A-Za-z-]+)\s*:\s*(.*)$/);
    if (!m) continue;
    const key = m[1].toLowerCase(), val = m[2].trim();
    if (key === 'user-agent') {
      if (!cur || cur.rules.length || cur.delay != null) { cur = { agents: [], rules: [], delay: null }; groups.push(cur); }
      cur.agents.push(val.toLowerCase());
    } else if (cur && (key === 'disallow' || key === 'allow')) {
      if (key === 'disallow' && !val) continue; // 空 Disallow = 全許可
      cur.rules.push({ allow: key === 'allow', path: val });
    } else if (cur && key === 'crawl-delay') {
      const d = parseFloat(val); if (!isNaN(d)) cur.delay = d;
    }
  }
  return groups;
}

function toRegex(path) {
  let re = '';
  for (const ch of path) {
    if (ch === '*') re += '.*';
    else if (ch === '$') re += '$';
    else re += ch.replace(/[.+?^${}()|[\]\\]/g, '\\$&');
  }
  return new RegExp('^' + re);
}

class Robots {
  constructor(fetchText) { this.fetchText = fetchText; this.cache = new Map(); }
  async group(origin) {
    if (this.cache.has(origin)) return this.cache.get(origin);
    let g = null;
    try {
      const txt = await this.fetchText(origin + '/robots.txt');
      if (typeof txt === 'string') {
        const groups = parse(txt);
        g = groups.find((x) => x.agents.some((a) => a.includes(UA_TOKEN))) || groups.find((x) => x.agents.includes('*')) || null;
      }
    } catch { g = null; }
    const rules = g ? g.rules.map((r) => ({ allow: r.allow, path: r.path, re: toRegex(r.path) })) : [];
    const v = { rules, delay: g ? g.delay : null };
    this.cache.set(origin, v);
    return v;
  }
  /** 許可か。最長一致のルールが勝つ(Google 流) */
  async allowed(url) {
    let u; try { u = new URL(url); } catch { return false; }
    const g = await this.group(u.origin);
    const path = u.pathname + u.search;
    let best = null;
    for (const r of g.rules) {
      if (r.re.test(path) && (!best || r.path.length > best.path.length)) best = r;
    }
    return best ? best.allow : true;
  }
  async crawlDelay(url) {
    try { return (await this.group(new URL(url).origin)).delay; } catch { return null; }
  }
}

module.exports = { Robots, parse, toRegex, UA_TOKEN };
