// 本地交互只增强静态内容，关闭脚本后文档仍可阅读。
// Local interactions enhance static content; documents remain readable without scripts.
const base = document.body.dataset.base;
const toast = document.querySelector('.toast');
let toastTimer;
function notify(text) { toast.textContent = text; toast.classList.add('show'); clearTimeout(toastTimer); toastTimer = setTimeout(() => toast.classList.remove('show'), 2400); }
document.querySelector('.theme-toggle').addEventListener('click', () => {
  const dark = document.documentElement.dataset.theme !== 'dark';
  document.documentElement.dataset.theme = dark ? 'dark' : 'light';
  try { localStorage.setItem('ling-theme', dark ? 'dark' : 'light'); } catch { /* 禁用存储不影响当前页面 / Storage is optional. */ }
});
document.querySelectorAll('.copy-code').forEach(button => button.addEventListener('click', async () => {
  const text = button.closest('.code-block').querySelector('pre').textContent;
  try { await navigator.clipboard.writeText(text); button.textContent = '已复制'; notify('代码已复制'); setTimeout(() => button.textContent = '复制', 1800); }
  catch { notify('无法访问剪贴板，请手动选择代码复制。'); }
}));
const menu = document.querySelector('.menu-toggle');
const sidebar = document.querySelector('.sidebar');
menu.addEventListener('click', () => {
  if (sidebar) { const open = sidebar.classList.toggle('open'); menu.setAttribute('aria-expanded', String(open)); menu.setAttribute('aria-label', open ? '关闭文档导航' : '打开文档导航'); }
  else location.href = `${base}docs/quickstart/`;
});
document.addEventListener('keydown', event => {
  if (event.key === 'Escape' && sidebar?.classList.contains('open')) { sidebar.classList.remove('open'); menu.setAttribute('aria-expanded', 'false'); menu.focus(); }
});
const tabs = [...document.querySelectorAll('.demo-tabs button')];
function selectTab(tab) { tabs.forEach(button => { const active = button === tab; button.setAttribute('aria-selected', String(active)); button.tabIndex = active ? 0 : -1; document.getElementById(button.getAttribute('aria-controls')).hidden = !active; }); }
tabs.forEach((tab, index) => {
  tab.addEventListener('click', () => selectTab(tab));
  tab.addEventListener('keydown', event => { if (['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) { event.preventDefault(); const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : tabs.length - 1)) % tabs.length; selectTab(tabs[next]); tabs[next].focus(); } });
});

const dialog = document.querySelector('.search-dialog');
const searchInput = document.querySelector('#doc-search');
const results = document.querySelector('#search-results');
let revealCommand = () => {};
let searchData;
let searchPromise;
async function loadSearch() {
  searchPromise ||= fetch(`${base}search.json`).then(response => { if (!response.ok) throw new Error('Search unavailable'); return response.json(); });
  return searchPromise;
}
async function openSearch() {
  dialog.showModal(); searchInput.focus();
  try { searchData = await loadSearch(); renderSearch(); }
  catch { results.replaceChildren(); const p = document.createElement('p'); p.className = 'search-empty'; p.textContent = '搜索暂时不可用，请使用文档目录浏览。'; results.append(p); searchPromise = null; }
}
function renderSearch() {
  if (!searchData) return;
  const terms = searchInput.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
  results.replaceChildren();
  if (!terms.length) { const p = document.createElement('p'); p.className = 'search-empty'; p.textContent = '输入关键词，查找文档与命令。'; results.append(p); return; }
  const found = searchData.map(entry => ({ entry, score: terms.reduce((score, term) => score + (entry.title.toLowerCase().includes(term) ? 10 : entry.text.toLowerCase().includes(term) ? 1 : 0), 0) })).filter(({ entry }) => terms.every(term => `${entry.title} ${entry.text}`.toLowerCase().includes(term))).sort((a, b) => b.score - a.score).slice(0, 16);
  if (!found.length) { const p = document.createElement('p'); p.className = 'search-empty'; p.textContent = '没有匹配结果，试试命令名或更简短的关键词。'; results.append(p); }
  for (const { entry } of found) { const a = document.createElement('a'); a.className = 'search-result'; a.href = entry.href; a.addEventListener('click', () => { dialog.close(); const target = new URL(a.href); if (target.pathname === location.pathname && target.hash) revealCommand(target.hash); }); const group = document.createElement('span'); group.textContent = entry.group; const title = document.createElement('strong'); title.textContent = entry.title; const excerpt = document.createElement('p'); const lower = entry.text.toLowerCase(); const index = lower.indexOf(terms[0]); excerpt.textContent = entry.text.slice(Math.max(0, index - 20), Math.max(0, index - 20) + 110); a.append(group, title, excerpt); results.append(a); }
}
document.querySelector('.search-trigger').addEventListener('click', openSearch);
document.querySelector('.search-close').addEventListener('click', () => dialog.close());
dialog.addEventListener('click', event => { if (event.target === dialog && (event.offsetX < 0 || event.offsetX > dialog.offsetWidth || event.offsetY < 0 || event.offsetY > dialog.offsetHeight)) dialog.close(); });
searchInput.addEventListener('input', renderSearch);
document.addEventListener('keydown', event => {
  const typing = event.target.matches('input,textarea,[contenteditable=true]');
  if ((event.key === '/' && !typing) || ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'k')) { event.preventDefault(); if (!dialog.open) openSearch(); }
  if (dialog.open && ['ArrowDown', 'ArrowUp'].includes(event.key)) { const links = [...results.querySelectorAll('a')]; if (!links.length) return; event.preventDefault(); const index = links.indexOf(document.activeElement); const next = index < 0 ? (event.key === 'ArrowDown' ? 0 : links.length - 1) : (index + (event.key === 'ArrowDown' ? 1 : links.length - 1)) % links.length; links[next].focus(); }
  if (dialog.open && event.key === 'Enter' && document.activeElement === searchInput) results.querySelector('a')?.click();
});

const commandInput = document.querySelector('#command-filter');
if (commandInput) {
  const filters = [...document.querySelectorAll('[data-filter]')];
  const cards = [...document.querySelectorAll('.command-card')];
  let selected = '全部';
  function filterCommands() { const terms = commandInput.value.toLowerCase().trim().split(/\s+/).filter(Boolean); let count = 0; cards.forEach(card => { const visible = (selected === '全部' || card.dataset.group === selected) && terms.every(term => `${card.dataset.search} ${card.dataset.group}`.toLowerCase().includes(term)); card.hidden = !visible; count += Number(visible); }); document.querySelector('.command-count').textContent = `${count} 组命令`; document.querySelector('.command-empty').hidden = count !== 0; }
  commandInput.addEventListener('input', filterCommands);
  filters.forEach(button => button.addEventListener('click', () => { selected = button.dataset.filter; filters.forEach(item => item.setAttribute('aria-pressed', String(item === button))); filterCommands(); }));
  revealCommand = hash => { selected = '全部'; commandInput.value = ''; filters.forEach(button => button.setAttribute('aria-pressed', String(button.dataset.filter === selected))); filterCommands(); document.getElementById(decodeURIComponent(hash.slice(1)))?.scrollIntoView(); };
  window.addEventListener('hashchange', () => revealCommand(location.hash));
}
if ('IntersectionObserver' in window) {
  const links = [...document.querySelectorAll('.toc a[href^="#"]')];
  const observer = new IntersectionObserver(entries => { for (const entry of entries) if (entry.isIntersecting) { links.forEach(link => link.classList.toggle('active', decodeURIComponent(link.hash.slice(1)) === entry.target.id)); break; } }, { rootMargin: '-10% 0px -65% 0px' });
  document.querySelectorAll('.prose h2,.prose h3').forEach(heading => observer.observe(heading));
}
