// 从仓库规范生成静态页面，所有页面均可直接访问。
// Generate directly addressable static pages from repository specifications.
import { readFile, writeFile, mkdir, cp } from 'node:fs/promises';
import { resolve, relative, dirname, extname } from 'node:path';
import { Marked } from 'marked';
import { createHighlighter } from 'shiki';

const root = resolve('..');
const out = resolve('dist');
const basePath = (process.env.BASE_PATH || '/ling/').replace(/^\/+|\/+$/g, '');
const base = basePath ? `/${basePath}/` : '/';
const origin = (process.env.SITE_URL || 'https://921108257.github.io/ling').replace(/\/$/, '');
const cargo = await readFile(resolve(root, 'Cargo.toml'), 'utf8');
const version = cargo.match(/^version\s*=\s*"([^"]+)"/m)[1];
const github = 'https://github.com/921108257/ling';
const url = (path = '') => `${base}${path.replace(/^\//, '')}`;
const esc = (value) => String(value).replace(/[&<>"']/g, ch => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]));
const slug = text => text.toLowerCase().replace(/<[^>]*>/g, '').replace(/[\u2000-\u206f\u2e00-\u2e7f\\!'"#$%&()*+,./:;<=>?@[\]^`{|}~]/g, '').replace(/ /g, '-');
const pages = [
  ['quickstart', '快速上手', '开始使用', 'site/src/quickstart.md', '安装、连接并存取你的第一条数据。'],
  ['install', '安装与服务', '开始使用', 'docs/install.md', '选择安装包，管理系统服务与升级。'],
  ['cli', 'CLI 使用指南', '开始使用', 'docs/cli.md', '类型语法、执行方式与终端帮助。'],
  ['commands', '命令参考', '使用与参考', 'docs/commands.md', '每条命令的返回值、原子性与错误边界。'],
  ['data-model', '数据类型', '使用与参考', 'docs/data-model.md', '十种 Value 与严格的类型语义。'],
  ['ttl', '过期与生命周期', '使用与参考', 'docs/ttl.md', '毫秒 TTL、逻辑过期和回收。'],
  ['events', '事件与订阅', '使用与参考', 'docs/events.md', 'Fast、Reliable、消费组和 ACK。'],
  ['config', '配置参考', '使用与参考', 'docs/config.md', '配置默认值、资源预算与运行时边界。'],
  ['sdk', '客户端 SDK', '使用与参考', 'docs/sdk.md', 'Rust、Java 和 Go 的连接与集成。'],
  ['client-examples', '客户端实现示例', '使用与参考', 'docs/client-examples.md', '逐帧握手、强类型读写、订阅 ACK 与断线排错。'],
  ['persistence', '持久化边界', '深入了解', 'docs/persistence.md', 'Journal 提交、恢复与故障语义。'],
  ['protocol', '二进制协议', '深入了解', 'docs/protocol.md', '帧、编码、请求与连接屏障。'],
  ['architecture', '架构与范围', '深入了解', 'docs/architecture.md', '独立单机服务、分片与并发设计。'],
  ['overview', '产品概览', '深入了解', 'README.md', '安装、命令行入门和功能说明。'],
  ['testing', '测试与性能', '贡献者参考', 'docs/testing.md', '测试方法、性能数据与测量条件。'],
  ['roadmap', '开发路线图', '贡献者参考', 'docs/roadmap.md', '开发阶段与发布条件。'],
  ['decisions', '设计决策', '贡献者参考', 'docs/decisions.md', '设计取舍和兼容性说明。'],
].map(([id, title, group, file, description]) => ({ id, title, group, file, description, route: `docs/${id}/`, reference: group === '贡献者参考' }));
const readerPages = pages.filter(page => !page.reference);
const sourceRoutes = new Map(pages.map(page => [resolve(root, page.file), page.reference ? `${github}/blob/main/${page.file}` : url(page.route)]));
const highlighter = await createHighlighter({ themes: ['github-light', 'github-dark'], langs: ['bash', 'rust', 'go', 'java', 'json', 'toml', 'typescript', 'powershell', 'xml', 'yaml', 'python'] });
const search = [];

function rewriteLink(href, source) {
  if (!href || /^(https?:|mailto:|#)/.test(href)) return href;
  const [path, hash] = href.split('#');
  const absolute = resolve(dirname(source), decodeURIComponent(path));
  if (sourceRoutes.has(absolute)) return sourceRoutes.get(absolute) + (hash ? `#${hash}` : '');
  const repoPath = relative(root, absolute).replaceAll('\\', '/');
  if (repoPath.startsWith('../')) throw new Error(`Link leaves repository: ${href}`);
  return `${github}/${extname(repoPath) ? 'blob' : 'tree'}/main/${repoPath}${hash ? `#${hash}` : ''}`;
}

function renderMarkdown(source, file) {
  const headings = [];
  const seen = new Map();
  const marked = new Marked({ gfm: true });
  marked.use({ renderer: {
    heading({ tokens, depth }) {
      const html = this.parser.parseInline(tokens);
      const text = tokens.map(token => token.text ?? token.raw).join('').replace(/<[^>]*>/g, '');
      const stem = slug(text); const count = seen.get(stem) || 0; seen.set(stem, count + 1);
      const id = stem + (count ? `-${count}` : '');
      if (depth === 2 || depth === 3) headings.push({ id, text, depth });
      return `<h${depth} id="${esc(id)}">${html}<a class="heading-link" href="#${esc(id)}" aria-label="链接到此章节">#</a></h${depth}>`;
    },
    link({ href, title, tokens }) {
      const target = rewriteLink(href, file);
      const external = /^https?:/.test(target);
      return `<a href="${esc(target)}"${title ? ` title="${esc(title)}"` : ''}${external ? ' rel="noreferrer"' : ''}>${this.parser.parseInline(tokens)}</a>`;
    },
    code({ text, lang }) {
      const language = (lang || 'text').split(/\s/)[0];
      const aliases = { sh: 'bash', shell: 'bash', ps1: 'powershell', ts: 'typescript' };
      const chosen = aliases[language] || language;
      const highlighted = highlighter.codeToHtml(text, { lang: highlighter.getLoadedLanguages().includes(chosen) ? chosen : 'text', themes: { light: 'github-light', dark: 'github-dark' } });
      return `<div class="code-block"><div class="code-bar"><span>${esc(language)}</span><button class="copy-code" type="button" aria-label="复制代码">复制</button></div>${highlighted}</div>`;
    },
    table(token) { return `<div class="table-scroll"><table><thead>${this.tablerow({ text: token.header.map(cell => this.tablecell({ ...cell, header: true })).join('') })}</thead><tbody>${token.rows.map(row => this.tablerow({ text: row.map(cell => this.tablecell({ ...cell, header: false })).join('') })).join('')}</tbody></table></div>`; },
  } });
  return { html: marked.parse(source), headings };
}

const mark = `<svg class="brand-symbol" viewBox="0 0 32 32" fill="none" aria-hidden="true"><path d="M9 5v15a7 7 0 0 0 14 0v-9h-7v9" stroke="currentColor" stroke-width="3.5" stroke-linecap="round" stroke-linejoin="round"/><path d="M16 5h7" stroke="currentColor" stroke-width="3.5" stroke-linecap="round"/></svg>`;
function header(active) {
  return `<header class="site-header"><div class="header-inner"><a class="brand" href="${url()}">${mark}<span>ling</span></a><span class="version">${esc(version)} pre</span><nav class="top-nav" aria-label="主导航"><a${active === 'docs' ? ' aria-current="page"' : ''} href="${url('docs/quickstart/')}">文档</a><a${active === 'commands' ? ' aria-current="page"' : ''} href="${url('commands/')}">命令速查</a><a href="${github}/releases">下载</a></nav><div class="header-tools"><button type="button" class="search-trigger" aria-label="搜索文档"><svg viewBox="0 0 20 20" fill="none" aria-hidden="true"><circle cx="8.5" cy="8.5" r="5.5" stroke="currentColor" stroke-width="1.5"/><path d="m13 13 4 4" stroke="currentColor" stroke-width="1.5"/></svg><span>搜索文档</span><kbd>/</kbd></button><button type="button" class="theme-toggle icon-button" aria-label="切换深浅主题">◐</button><a class="github-link" href="${github}" aria-label="GitHub 源码"><svg viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="M12 .8a11.3 11.3 0 0 0-3.57 22c.56.1.77-.24.77-.54v-2.1c-3.14.68-3.8-1.34-3.8-1.34-.52-1.3-1.26-1.66-1.26-1.66-1.03-.7.08-.69.08-.69 1.14.08 1.74 1.17 1.74 1.17 1.01 1.72 2.64 1.22 3.29.94.1-.73.4-1.22.72-1.5-2.51-.28-5.15-1.26-5.15-5.6 0-1.24.44-2.25 1.16-3.04-.12-.29-.5-1.44.11-3 0 0 .95-.3 3.1 1.16a10.8 10.8 0 0 1 5.64 0c2.16-1.46 3.1-1.16 3.1-1.16.62 1.56.23 2.71.12 3 .72.8 1.16 1.8 1.16 3.04 0 4.35-2.65 5.32-5.17 5.6.41.36.77 1.04.77 2.1v3.08c0 .3.2.65.78.54A11.3 11.3 0 0 0 12 .8Z"/></svg></a></div><button type="button" class="menu-toggle icon-button" aria-label="打开文档导航" aria-expanded="false">☰</button></div></header>`;
}
function sidebar(current) {
  const navigation = readerPages;
  return `<aside class="sidebar" id="docs-sidebar" aria-label="文档目录"><a class="sidebar-home" href="${url()}">ling 文档</a>${[...new Set(navigation.map(page => page.group))].map(group => `<section class="nav-group"><h2>${group}</h2>${navigation.filter(page => page.group === group).map(page => `<a href="${url(page.route)}"${page.id === current ? ' aria-current="page"' : ''}>${page.title}</a>`).join('')}</section>`).join('')}<a class="sidebar-release" href="${github}/releases/tag/v${version}"><span class="status-dot"></span> ${esc(version)} 预发布说明</a></aside>`;
}
function footer() { return `<footer class="site-footer"><a class="brand" href="${url()}">${mark}<span>ling</span></a><p>类型明确，边界清晰。</p><div><a href="${github}">GitHub</a><a href="${url('docs/install/')}">安装与服务</a><a href="${github}/issues">反馈问题</a></div><span>ling ${esc(version)} pre</span></footer>`; }
function shell({ title, description, route, content, active = 'docs', classes = '' }) {
  return `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="description" content="${esc(description)}"><meta name="theme-color" content="#2356d8"><meta property="og:title" content="${esc(title)} · ling"><meta property="og:description" content="${esc(description)}"><link rel="canonical" href="${esc(origin + '/' + route)}"><link rel="icon" type="image/svg+xml" href="${url('assets/favicon.svg')}"><link rel="stylesheet" href="${url('assets/style.css')}"><title>${esc(title)} · ling</title><script>try{if(localStorage.getItem('ling-theme')==='dark'||(!localStorage.getItem('ling-theme')&&matchMedia('(prefers-color-scheme: dark)').matches))document.documentElement.dataset.theme='dark'}catch{}</script></head><body class="${classes}" data-base="${base}"><a class="skip-link" href="#main">跳到正文</a>${header(active)}${content}<dialog class="search-dialog" aria-label="搜索文档"><div class="search-head"><label class="sr-only" for="doc-search">搜索文档与命令</label><input id="doc-search" type="search" placeholder="搜索命令、配置、安装…" autocomplete="off"><button type="button" class="search-close" aria-label="关闭搜索">Esc</button></div><div id="search-results" aria-live="polite"><p class="search-empty">输入关键词，查找文档与命令。</p></div><div class="search-foot">↑ ↓ 选择结果 <span>Enter 打开</span></div></dialog><div class="toast" role="status"></div><script type="module" src="${url('assets/app.js')}"></script></body></html>`;
}
async function save(route, html) { const file = resolve(out, route, 'index.html'); await mkdir(dirname(file), { recursive: true }); await writeFile(file, html); }
await mkdir(resolve(out, 'assets'), { recursive: true });
await cp('public', resolve(out, 'assets'), { recursive: true });

for (const page of pages) {
  if (page.reference) {
    const destination = sourceRoutes.get(resolve(root, page.file));
    await save(page.route, `<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="description" content="在 GitHub 查看${esc(page.title)}。"><meta name="robots" content="noindex"><meta http-equiv="refresh" content="0; url=${esc(destination)}"><link rel="canonical" href="${esc(destination)}"><title>正在前往 GitHub · ling</title></head><body><main id="main"><p>此文档位于 GitHub。<a href="${esc(destination)}" rel="noreferrer">继续查看${esc(page.title)}</a>。</p></main></body></html>`);
    continue;
  }
  const fileSource = await readFile(resolve(root, page.file), 'utf8');
  const source = page.id === 'overview' ? fileSource.replace(/<details>[\s\S]*?<\/details>/g, '') : fileSource;
  const { html, headings } = renderMarkdown(source, resolve(root, page.file));
  const sequence = readerPages;
  const index = sequence.indexOf(page);
  const next = sequence[index + 1]; const previous = sequence[index - 1];
  const content = `<div class="docs-layout">${sidebar(page.id)}<main id="main" class="document"><div class="breadcrumb">${esc(page.group)} <span>/</span> ${esc(page.title)}</div><article class="prose">${html}</article><nav class="doc-pagination" aria-label="文档翻页">${previous ? `<a href="${url(previous.route)}"><small>上一篇</small>${previous.title}</a>` : '<span></span>'}${next ? `<a href="${url(next.route)}"><small>下一篇</small>${next.title}</a>` : ''}</nav></main><aside class="toc" aria-label="本页目录"><span>本页内容</span>${headings.map(h => `<a class="depth-${h.depth}" href="#${esc(h.id)}">${esc(h.text)}</a>`).join('')}<a class="back-top" href="#main">返回顶部 ↑</a></aside></div>`;
  await save(page.route, shell({ ...page, content, classes: 'docs-page' }));
  const plain = source.replace(/```[^\n]*\n[\s\S]*?```/g, ' ').replace(/[#*`>|\[\]]/g, ' ').replace(/\s+/g, ' ');
  search.push({ title: page.title, group: page.group, text: plain, href: url(page.route) });
  // 章节也加入索引，定位到正文而非仅找到长页面。
  // Index sections to land within long reference pages.
  for (const heading of headings.filter(h => h.depth === 2)) search.push({ title: heading.text, group: page.title, text: heading.text, href: url(page.route) + `#${heading.id}` });
}

const commands = (await readFile(resolve(root, 'crates/cli-help/src/commands.tsv'), 'utf8')).trim().split(/\r?\n/).map(line => {
  const [names, syntax, english, chinese] = line.split('\t');
  return { names: names.split(','), syntax, english, description: chinese };
});
const groupRules = [
  ['Key 与读写', ['HELP', 'SET', 'GET', 'GETSET', 'MGET', 'MSET', 'DEL', 'EXISTS', 'TYPE', 'RENAME', 'RENAMENX', 'COPY', 'MOVE', 'KEYS', 'SCAN', 'RANDOMKEY']],
  ['过期与 TTL', ['EXPIRE', 'PEXPIRE', 'EXPIREAT', 'PEXPIREAT', 'PERSIST', 'TTL', 'PTTL', 'EXPIRETIME', 'PEXPIRETIME']],
  ['数值与字节', ['INCR', 'DECR', 'INCRBY', 'DECRBY', 'INCRFLOAT', 'APPEND', 'STRLEN', 'BLEN']],
  ['List', ['LPUSH', 'RPUSH', 'LPOP', 'RPOP', 'LLEN', 'LINDEX', 'LRANGE', 'LSET', 'LREM', 'LTRIM']],
  ['Map', ['FGET', 'FMGET', 'FSET', 'FSETNX', 'FDEL', 'FEXISTS', 'FLEN', 'FKEYS', 'FVALS', 'FGETALL', 'FMERGE']],
  ['Set', ['SADD', 'SREM', 'SISMEMBER', 'SMISMEMBER', 'SCARD', 'SMEMBERS', 'SPOP', 'SRANDMEMBER', 'SUNION', 'SINTER', 'SDIFF', 'SUNIONSTORE', 'SINTERSTORE', 'SDIFFSTORE']],
  ['服务与订阅', []],
];
for (const command of commands) command.group = (groupRules.find(([, names]) => command.names.some(name => names.includes(name))) || groupRules.at(-1))[0];
const cards = commands.map((command, index) => {
  const id = command.names[0].toLowerCase();
  search.push({ title: command.names.join(' / '), group: '命令速查', text: `${command.syntax} ${command.description} ${command.english}`, href: url('commands/') + `#${id}` });
  return `<article class="command-card" id="${id}" data-group="${esc(command.group)}" data-search="${esc(`${command.names.join(' ')} ${command.description} ${command.english}`.toLowerCase())}"><div class="command-heading"><h2>${esc(command.names.join(' / '))}</h2><span>${command.group}</span></div><p>${esc(command.description)}</p><code>${esc(command.syntax)}</code><details><summary>在终端查看完整帮助</summary><div class="code-block"><div class="code-bar"><span>shell</span><button class="copy-code" type="button">复制</button></div><pre><code>ling-cli -c 'HELP ${esc(command.names[0])}'</code></pre></div><a href="${url('docs/commands/')}">阅读命令参考 ↗</a></details></article>`;
}).join('');
await save('commands/', shell({ title: '命令速查', description: '按类别和关键词查找 ling 命令、语法与使用说明。', route: 'commands/', active: 'commands', content: `<main id="main" class="command-page"><div class="page-intro"><p class="breadcrumb">使用与参考 / 命令速查</p><h1>找到要用的命令。</h1><p>按类别或关键词查找语法、返回值与用法说明。</p></div><label class="command-search"><svg viewBox="0 0 20 20" fill="none" aria-hidden="true"><circle cx="8.5" cy="8.5" r="5.5" stroke="currentColor" stroke-width="1.5"/><path d="m13 13 4 4" stroke="currentColor" stroke-width="1.5"/></svg><span class="sr-only">筛选命令</span><input type="search" id="command-filter" placeholder="搜索命令或用途，例如：SET、过期、订阅"></label><div class="command-filters" role="group" aria-label="命令分类"><button type="button" data-filter="全部" aria-pressed="true">全部</button>${groupRules.map(([group]) => `<button type="button" data-filter="${group}" aria-pressed="false">${group}</button>`).join('')}</div><p class="command-count" aria-live="polite">${commands.length} 组命令</p><div class="command-list">${cards}</div><p class="command-empty" hidden>没有匹配的命令。试试命令名或更简短的关键词。</p></main>${footer()}` }));

const home = `<main id="main"><section class="hero"><div class="hero-copy"><a class="release-pill" href="${github}/releases/tag/v${version}"><span class="status-dot"></span>${esc(version)} 预发布 <span>查看更新 ↗</span></a><h1>为数据保留类型。<br>为缓存明确边界。</h1><p class="hero-description">一个用 Rust 构建的独立内存缓存数据库。<br>十种值类型、毫秒 TTL 与可选可靠事件，<br>从第一条命令开始，就有清晰的规则。</p><div class="hero-actions"><a class="button primary" href="${url('docs/quickstart/')}">开始使用 <span>↗</span></a><a class="button secondary" href="${url('commands/')}">浏览命令</a></div><p class="hero-footnote">Windows · macOS · Linux <span>独立服务 / 原生 SDK</span></p></div><div class="terminal-demo"><div class="terminal-title"><span class="terminal-dots"><i></i><i></i><i></i></span><span>ling-cli</span><span class="terminal-caption">命令示例</span></div><div class="demo-tabs" role="tablist" aria-label="命令示例"><button role="tab" id="tab-values" aria-controls="demo-values" aria-selected="true" tabindex="0">强类型读写</button><button role="tab" id="tab-ttl" aria-controls="demo-ttl" aria-selected="false" tabindex="-1">毫秒过期</button><button role="tab" id="tab-map" aria-controls="demo-map" aria-selected="false" tabindex="-1">结构化数据</button></div><div id="demo-values" class="demo-panel" role="tabpanel" aria-labelledby="tab-values"><div><span class="prompt">ling&gt;</span> SET counter <b>int(0)</b></div><p>true</p><div><span class="prompt">ling&gt;</span> INCRBY counter <b>2</b></div><p>int(2)</p><div><span class="prompt">ling&gt;</span> GET counter</div><p>int(2)</p><div class="terminal-note">类型始终保留，不做隐式转换。</div></div><div id="demo-ttl" class="demo-panel" role="tabpanel" aria-labelledby="tab-ttl" hidden><div><span class="prompt">ling&gt;</span> SET session null PX <b>5000</b> NX</div><p>true</p><div><span class="prompt">ling&gt;</span> PTTL session</div><p>int(4982)</p><div><span class="prompt">ling&gt;</span> PERSIST session</div><p>true</p><div class="terminal-note">按毫秒设置期限，剩余时间随读取时刻变化。</div></div><div id="demo-map" class="demo-panel" role="tabpanel" aria-labelledby="tab-map" hidden><div><span class="prompt">ling&gt;</span> SET profile <b>map({"visits": uint(1)})</b></div><p>true</p><div><span class="prompt">ling&gt;</span> FGETALL profile</div><p>map({"visits": uint(1)})</p><div><span class="prompt">ling&gt;</span> TYPE profile</div><p>"map"</p><div class="terminal-note">原生容器，与标量共享同一套类型规则。</div></div><div class="terminal-bottom"><span class="status-dot"></span>127.0.0.1:6380 <span>db 0</span></div></div></section><section class="type-strip" aria-label="十种数据类型"><span>十种值，清楚表达。</span><div>${['Null','Bool','Int','UInt','Float','String','Bytes','List','Map','Set'].map(type => `<a href="${url('docs/data-model/')}">${type}</a>`).join('')}</div></section><section class="home-guides"><div class="section-lead"><h2>从能运行，到用得明白。</h2><p>先完成一次读写，再按你的任务深入。</p></div><div class="guide-grid"><a class="guide-feature" href="${url('docs/quickstart/')}"><svg viewBox="0 0 40 40" fill="none" aria-hidden="true"><path d="M10 14l8 6-8 6m13 0h8" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"/></svg><h3>快速上手</h3><p>安装、连接、写入第一条数据。<br>用一组可运行的命令认识 ling。</p><span>开始第一次读写 ↗</span></a><div class="guide-links"><a href="${url('docs/install/')}"><span class="guide-icon">↓</span><div><h3>安装与服务</h3><p>选择平台，管理启动、升级和配置。</p></div><span>↗</span></a><a href="${url('commands/')}"><span class="guide-icon">⌘</span><div><h3>命令速查</h3><p>按用途查找语法，快速定位相关命令。</p></div><span>↗</span></a><a href="${url('docs/sdk/')}"><span class="guide-icon">{ }</span><div><h3>接入你的应用</h3><p>Rust、Java 和 Go 原生客户端。</p></div><span>↗</span></a></div></div></section><section class="principles"><div class="section-lead"><h2>知道它做什么，也知道边界在哪。</h2><p>帮助你选择正确的使用方式。</p></div><div class="principle-grid"><article><span class="principle-line"></span><h3>数据有自己的类型</h3><p>Null 与不存在分开表达。整数、浮点数和字节不会被悄悄转换；服务端原子修改单个 Key。</p><a href="${url('docs/data-model/')}">了解数据模型 ↗</a></article><article><span class="principle-line"></span><h3>过期是明确的规则</h3><p>支持相对与绝对时间，精确到毫秒。逻辑过期立即对读取生效，后台回收独立进行。</p><a href="${url('docs/ttl/')}">了解生命周期 ↗</a></article><article><span class="principle-line"></span><h3>事件按需求选择</h3><p>Fast 使用有界内存队列；Reliable 持久化已提交事件及消费进度。缓存值与 TTL 重启后不恢复。</p><a href="${url('docs/events/')}">了解事件语义 ↗</a></article></div></section><section class="preview-note"><span class="status-dot"></span><div><h2>版本与平台支持</h2><p>当前为 ${esc(version)} 预发布版。Windows 支持 Fast；Reliable 适用于 Linux / macOS。缓存值与 TTL 重启后不恢复。</p></div><a href="${url('docs/install/')}">安装与服务 ↗</a></section></main>${footer()}`;
await save('', shell({ title: '有类型、有边界的内存缓存', description: 'ling 是用 Rust 构建的独立内存缓存数据库，支持十种强类型值、毫秒 TTL、Fast 与 Reliable 事件。', route: '', content: home, active: '', classes: 'home-page' }));
await writeFile(resolve(out, 'search.json'), JSON.stringify(search));
await writeFile(resolve(out, '.nojekyll'), '');
await writeFile(resolve(out, '404.html'), shell({ title: '页面未找到', description: '返回 ling 文档继续浏览。', route: '404.html', content: `<main id="main" class="not-found"><span>404</span><h1>这条路径没有内容。</h1><p>可以返回快速上手，或使用搜索查找文档。</p><a class="button primary" href="${url('docs/quickstart/')}">打开文档</a></main>${footer()}` }));
await writeFile(resolve(out, 'sitemap.xml'), `<?xml version="1.0" encoding="UTF-8"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${['', 'commands/', ...readerPages.map(page => page.route)].map(route => `<url><loc>${origin}/${route}</loc></url>`).join('')}</urlset>`);
highlighter.dispose();
console.log(`Built ${pages.length + 2} pages at ${base}, using ling ${version}.`);
