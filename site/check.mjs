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
// 历史地址只转向仓库，不在官网呈现开发记录。
// Historical routes only redirect to the repository without publishing development records.
const historicalRoutes = ['testing', 'roadmap', 'decisions'];
for (const route of historicalRoutes) {
  const file = resolve(root, `docs/${route}/index.html`);
  const html = pages.get(file);
  const destination = `https://github.com/921108257/ling/blob/main/docs/${route}.md`;
  assert(html, `Missing historical redirect: ${route}`);
  assert(html.includes('<meta name="robots" content="noindex">'), `Historical route must not be indexed: ${route}`);
  assert(html.includes(`<meta http-equiv="refresh" content="0; url=${destination}">`), `Invalid historical redirect: ${route}`);
  assert(html.includes(`<a href="${destination}"`), `Missing redirect fallback: ${route}`);
  assert(!html.includes('<article') && !html.includes('class="sidebar"'), `Historical content must stay on GitHub: ${route}`);
  for (const [otherFile, otherHtml] of pages) {
    if (otherFile !== file) assert(!otherHtml.includes(`href="${base}docs/${route}/`), `Product link uses historical route: ${route}`);
  }
}
const search = JSON.parse(await readFile(resolve(root, 'search.json'), 'utf8'));
assert(search.length > 100, 'Expected sections and commands in search');
for (const entry of search) { const [path, hash] = entry.href.split('#'); assert(!historicalRoutes.some(route => path === `${base}docs/${route}/`), `Historical route is searchable: ${entry.href}`); const html = pages.get(resolve(root, path.slice(base.length), 'index.html')); assert(html, `Invalid search route: ${entry.href}`); if(hash) assert(html.includes(`id="${hash}"`), `Invalid search anchor: ${entry.href}`); }
assert(files.some(path => path.endsWith('sitemap.xml')));
const sitemap = await readFile(resolve(root, 'sitemap.xml'), 'utf8');
for (const route of historicalRoutes) assert(!sitemap.includes(`/docs/${route}/`), `Historical route is in sitemap: ${route}`);
console.log(`Checked ${pages.size} HTML pages, ${links} local links and ${search.length} search entries.`);
