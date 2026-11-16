// 本地预览与 Pages 相同的基础路径；Preview using the same base path as Pages.
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { resolve, extname, sep } from 'node:path';
const root = resolve('dist');
const base = process.env.BASE_PATH || '/ling/';
const types = { '.html': 'text/html; charset=utf-8', '.css': 'text/css', '.js': 'text/javascript', '.json': 'application/json', '.svg': 'image/svg+xml' };
createServer(async (req, res) => {
  try {
    const path = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
    if (!path.startsWith(base)) {
      res.writeHead(302, { Location: base }); res.end(); return;
    }
    let file = resolve(root, path.slice(base.length));
    if (file !== root && !file.startsWith(root + sep)) throw new Error('Outside preview');
    if ((await stat(file)).isDirectory()) file = resolve(file, 'index.html');
    const body = await readFile(file);
    res.writeHead(200, { 'Content-Type': types[extname(file)] || 'application/octet-stream' });
    res.end(body);
  } catch { res.writeHead(404); res.end('Not found'); }
}).listen(4173, '127.0.0.1', () => console.log(`Preview: http://127.0.0.1:4173${base}`));
