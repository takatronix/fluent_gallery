'use strict';
// 固定値の置き場 = crawler/config.json(このファイルを書き換えるだけで変わる。読むのは起動時 1 回 → 変更後はクローラー再起動)
// 優先順: 環境変数(一時上書き) > config.json > 既定
const fs = require('fs');
const path = require('path');
let file = {};
try { file = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'config.json'), 'utf8')) || {}; } catch {}

const str = (env, key, def) => process.env[env] || (typeof file[key] === 'string' ? file[key] : def); // ファイルに書いた空文字は「明示的に無し」として尊重
const flag = (env, key, def = false) => {
  const e = process.env[env];
  if (e != null && e !== '') return /^(1|true|yes)$/i.test(e);
  return typeof file[key] === 'boolean' ? file[key] : def;
};

module.exports = { file, str, flag };
