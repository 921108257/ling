// 检查发布目录内的路由、锚点和真实内容；Validate routes, anchors and published content.
import { readdir, readFile, stat } from 'node:fs/promises';
import { resolve } from 'node:path';
import assert from 'node:assert/strict';
const root = resolve('dist');
const basePath = (process.env.BASE_PATH || '/ling/').replace(/^\/+|\/+$/g, '');
const base = basePath ? `/${basePath}/` : '/';
const decode = text => text.replaceAll('&amp;', '&').replaceAll('&quot;', '"').replaceAll('&#39;', "'");
async function walk(dir) { const entries = await readdir(dir, { withFileTypes: true }); return (await Promise.all(entries.map(entry => entry.isDirectory() ? walk(resolve(dir, entry.name)) : [resolve(dir, entry.name)]))).flat(); }
const files = await walk(root);
const pages = new Map(await Promise.all(files.filter(path => path.endsWith('.html')).map(async path => [path, await readFile(path, 'utf8')])));
let links = 0;
for (const [file, html] of pages) {
  assert.match(html, /<html lang="zh-CN">/);
  assert.match(html, /<main id="main"/);
  assert.match(html, /<meta name="description"/);
  assert(!html.includes('undefined'), `${file}: undefined output`);
  const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map(match => decode(match[1]));
  assert.equal(new Set(ids).size, ids.length, `${file}: duplicate ids`);
  for (const match of html.matchAll(/\b(?:href|src)="([^"]+)"/g)) {
    const href = decode(match[1]);
    if (/^(?:https?:|mailto:|data:)/.test(href)) continue;
    assert(href.startsWith('#') || href.startsWith(base), `${file}: link escapes Pages base: ${href}`);
    const [pathname, hash] = href.split('#');
    let target = pathname ? resolve(root, decodeURIComponent(pathname.slice(base.length))) : file;
    if ((await stat(target)).isDirectory()) target = resolve(target, 'index.html');
    if (hash) { const targetHtml = pages.get(target); assert(targetHtml, `Not an HTML anchor: ${href}`); const targetIds = [...targetHtml.matchAll(/\bid="([^"]+)"/g)].map(item => decode(item[1])); assert(targetIds.includes(decodeURIComponent(hash)), `${file}: missing anchor ${href}`); }
    links++;
  }
}
const home = pages.get(resolve(root, 'index.html'));
assert(home.includes('十种值，清楚表达。'));
const reference = pages.get(resolve(root, 'commands/index.html'));
for (const command of ['SET','SCAN','SUNIONSTORE','SHUTDOWN','ACK']) assert(reference.includes(`>${command}<`) || reference.includes(command), `Missing ${command}`);
const search = JSON.parse(await readFile(resolve(root, 'search.json'), 'utf8'));
assert(search.length > 100, 'Expected sections and commands in search');
for (const entry of search) { const [path, hash] = entry.href.split('#'); const html = pages.get(resolve(root, path.slice(base.length), 'index.html')); assert(html, `Invalid search route: ${entry.href}`); if(hash) assert(html.includes(`id="${hash}"`), `Invalid search anchor: ${entry.href}`); }
assert(files.some(path => path.endsWith('sitemap.xml')));
console.log(`Checked ${pages.size} HTML pages, ${links} local links and ${search.length} search entries.`);
