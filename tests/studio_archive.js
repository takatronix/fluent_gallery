// A ZIP carries private editing data without importing those files as gallery images.
// Run only against a disposable /tmp/fg-edit-regression-* root and server.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const zlib = require('node:zlib');
const {execFileSync} = require('node:child_process');
const BASE = process.env.FG_URL, ROOT = process.env.FG_TEST_ROOT;
assert(BASE && ROOT && /^\/tmp\/fg-edit-regression-[^/]+$/.test(ROOT));
assert.equal(fs.realpathSync(ROOT), ROOT);
assert(['localhost', '127.0.0.1'].includes(new URL(BASE).hostname) && new URL(BASE).port !== '8790');
const nonce = crypto.randomBytes(8).toString('hex'), source = 'upload:_archive_' + nonce;
const digest = b => crypto.createHash('sha1').update(b).digest('hex');
function chunk(type, data) {
  const body = Buffer.concat([Buffer.from(type), data]), out = Buffer.alloc(body.length + 8);
  out.writeUInt32BE(data.length); body.copy(out, 4);
  let crc = 0xffffffff;
  for (const b of body) { crc ^= b; for (let k=0;k<8;k++) crc=(crc>>>1) ^ (0xedb88320 & -(crc&1)); }
  out.writeUInt32BE((~crc)>>>0, out.length-4); return out;
}
function png(red) {
  const header = Buffer.alloc(13); header.writeUInt32BE(320);header.writeUInt32BE(200,4);header[8]=8;header[9]=6;
  const rows = Buffer.alloc(200 * (320*4+1));
  for (let y=0;y<200;y++) for(let x=0;x<320;x++) {
    const i=y*(320*4+1)+1+x*4; rows[i]=red;rows[i+1]=x%255;rows[i+2]=y;rows[i+3]=255;
  }
  return Buffer.concat([Buffer.from([137,80,78,71,13,10,26,10]), chunk('IHDR',header),
    chunk('tEXt',Buffer.from('test\0'+nonce)),chunk('IDAT',zlib.deflateSync(rows)),chunk('IEND',Buffer.alloc(0))]);
}
async function api(p, body, method=body===undefined?'GET':'POST') {
  const r=await fetch(BASE+p,{method,headers:body===undefined?{}:{'Content-Type':'application/json'},body:body===undefined?undefined:JSON.stringify(body)});
  const text=await r.text();assert(r.ok, p+': '+r.status+' '+text);return JSON.parse(text);
}
async function getBytes(p) { const r=await fetch(BASE+p);assert(r.ok,p+': '+r.status);return Buffer.from(await r.arrayBuffer()); }
async function upload(bytes,name,src) {
  const form=new FormData();if(src)form.append('source',src);form.append('file',new Blob([bytes]),name);
  const r=await fetch(BASE+'/api/upload',{method:'POST',body:form});assert(r.ok);return r.json();
}
const original=png(21), sha=digest(original), archiveFile='/tmp/fg-archive-test-'+nonce+'.zip';
(async()=>{
  const before=(await api('/api/images?limit=1')).total;
  assert.equal((await upload(original,'archive.png',source)).added,1);
  const meta=await api('/api/meta/'+sha);
  const recipe={version:1,save_mode:'in_place',input_sha:sha,source_edits_rev:meta.edits_rev,target_edits_rev:meta.edits_rev,
    photo_edits:[],graph:{n:[{i:'s',t:'src',k:'smp'},{i:'o',t:'out'}],e:[['s',0,'o',0]]},sceneYaml:'scene: {}'};
  const form=new FormData();form.append('image',new Blob([png(220)],{type:'image/png'}),'output.png');form.append('recipe',JSON.stringify(recipe));
  const response=await fetch(BASE+'/api/studio/'+sha+'/save',{method:'POST',body:form});assert(response.ok);
  const saved=await response.json();assert.equal(saved.sha1,sha);
  const renderSha=saved.meta.studio_edit.render_sha, renderUrl='/render/'+sha+'?w=0&v='+saved.meta.edits_rev;
  const rendered=await getBytes(renderUrl);
  const job=await api('/api/export',{shas:[sha],name:'archive_'+nonce});
  const deadline=Date.now()+30000;let status;
  do { status=await api('/api/export/'+job.id);assert(!status.error,status.error);if(!status.ready)await new Promise(r=>setTimeout(r,100)); }
  while(!status.ready && Date.now()<deadline);
  assert(status.ready);fs.writeFileSync(archiveFile,await getBytes('/export/'+job.id+'/archive.zip'));
  const entries=JSON.parse(execFileSync('python3',['-c','import zipfile,json,sys; print(json.dumps(zipfile.ZipFile(sys.argv[1]).namelist()))',archiveFile],{encoding:'utf8'}));
  assert(entries.some(n=>n.endsWith('/.fluent_gallery/studio_renders/'+renderSha.slice(0,2)+'/'+renderSha+'.png')));
  assert(entries.some(n=>n.endsWith('/.fluent_gallery/studio_renders/'+renderSha.slice(0,2)+'/'+renderSha+'.json')));
  assert.equal(entries.filter(n=>/\.(png|jpg|webp)$/.test(n)&&!n.includes('/.fluent_gallery/')).length,1);
  await api('/api/trash',{shas:[sha]});
  const dir=path.join(ROOT,'store/studio_renders',renderSha.slice(0,2));
  for(const name of fs.readdirSync(dir))if(name.startsWith(renderSha+'.'))fs.unlinkSync(path.join(dir,name));
  assert.equal((await api('/api/images?limit=1')).total,before);
  const restored=await upload(fs.readFileSync(archiveFile),'archive.zip');assert.equal(restored.added,1);assert.equal(restored.bad,0);
  assert.equal((await api('/api/images?limit=1')).total,before+1);
  assert.equal(digest(await getBytes('/img/'+sha)),digest(original));
  const after=await api('/api/meta/'+sha);assert.equal(after.studio_edit.render_sha,renderSha);
  assert.deepEqual(await getBytes(renderUrl),rendered);
  const thumb=await getBytes('/thumb/'+sha+'?v='+after.edits_rev);assert(thumb.length>100);
  console.log('PASS archive export/import: one logical image, private render+recipe restored from ZIP, original and final PNG exact, cold thumbnail works');
})().catch(e=>{console.error(e.stack);process.exitCode=1;}).finally(async()=>{
  await api('/api/trash',{shas:[sha]}).catch(()=>{});fs.rmSync(archiveFile,{force:true});
});
