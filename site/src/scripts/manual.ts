import { d, $, $$, deskQ, say, showToast } from './desktop';

const INSTALL = 'curl -fsSL https://raw.githubusercontent.com/mayberuk/lets/main/install.sh | sh\nlets hooks install claude-code';

/** The commands a `console` block runs: its `$ ` lines, plus each heredoc body up to its terminator. */
function commandsOf(text: string) {
  const out: string[] = [];
  let until = '';
  for (const line of text.split('\n')) {
    if (until) {
      out.push(line);
      if (line === until) until = '';
    } else if (line.startsWith('$ ')) {
      const cmd = line.slice(2);
      out.push(cmd);
      until = /<<-?\s*['"]?(\w+)['"]?\s*$/.exec(cmd)?.[1] ?? '';
    }
  }
  return out.join('\n');
}

function copyKey(label: string) {
  const b = d.createElement('button');
  b.type = 'button'; b.className = 'key code-copy';
  b.innerHTML = '<svg class="glyph" viewBox="0 0 16 16" aria-hidden="true"><use href="#g-copy"/></svg><svg class="tick" viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 8.5l3 3 6-6.5"/></svg><span class="k-label"></span>';
  $('.k-label', b).textContent = label;
  return b;
}

function onCopy(btn: HTMLElement, text: () => string, told: [string, string]) {
  const label = $('.k-label', btn), text0 = label.textContent;
  btn.addEventListener('click', async () => {
    btn.classList.add('is-pressed');
    setTimeout(() => btn.classList.remove('is-pressed'), 120);
    try {
      await navigator.clipboard.writeText(text());
      btn.classList.add('is-done'); label.textContent = 'Copied';
      $('#toast-b').textContent = told[0];
      $('#toast-s').textContent = told[1];
      showToast();
      say(told[0] + '. ' + told[1]);
      setTimeout(() => { btn.classList.remove('is-done'); label.textContent = text0; }, 2400);
    } catch {
      label.textContent = 'Copy failed: select the text';
      setTimeout(() => { label.textContent = text0; }, 3000);
    }
  });
}

function initCopy() {
  if (!(navigator.clipboard && window.isSecureContext)) return;
  $$('[data-copy-install]').forEach((btn) => {
    btn.hidden = false;
    onCopy(btn, () => INSTALL, ['Copied 2 commands', 'Paste them in a terminal.']);
  });
  $$<HTMLPreElement>('.prose pre:not(.cmd)').forEach((pre) => {
    const shell = pre.dataset.language === 'console' && /^\$ /m.test(pre.textContent!);
    const btn = copyKey(shell ? 'Copy command' : 'Copy');
    const wrap = d.createElement('div');
    wrap.className = 'code';
    pre.before(wrap); wrap.append(btn, pre);
    onCopy(btn, () => (shell ? commandsOf(pre.textContent!) : pre.textContent!.replace(/\n$/, '')),
      shell ? ['Copied the command', 'Run it in a project with lets installed.'] : ['Copied', 'The block is on your clipboard.']);
  });
}

function initProse() {
  $$<HTMLTableElement>('.prose table').forEach((t) => {
    const wrap = d.createElement('div');
    wrap.className = 'tbl-wrap';
    t.before(wrap); wrap.append(t);
    const cols = $$('thead th', t).map((th) => th.textContent!);
    $$<HTMLTableRowElement>('tbody tr', t).forEach((tr) => Array.from(tr.cells).forEach((td, i) => { td.dataset.label = cols[i]; }));
  });
  $$('.prose h2[id]').forEach((h) => {
    const a = d.createElement('a');
    a.className = 'anchor'; a.href = '#' + h.id; a.textContent = '#';
    a.setAttribute('aria-label', 'Link to ' + h.textContent);
    h.append(a);
  });
  $$<HTMLPreElement>('.prose pre[data-language="console"] .line').forEach((l) => {
    if (l.textContent!.startsWith('$ ')) l.classList.add('cmdl');
  });
}

function initToc() {
  const toc = $<HTMLDetailsElement>('#toc');
  const sync = () => { toc.open = deskQ.matches; };
  sync(); deskQ.addEventListener('change', sync);
  $$('a[href^="#"]', toc).forEach((a) => a.addEventListener('click', () => { if (!deskQ.matches) toc.open = false; }));

  const links = $$<HTMLAnchorElement>('.tree .sub a');
  const heads = links.map((a) => d.getElementById(a.hash.slice(1))).filter((h): h is HTMLElement => !!h);
  if (!('IntersectionObserver' in window) || !heads.length) return;
  const seen = new Set<string>();
  const io = new IntersectionObserver((es) => {
    es.forEach((e) => { if (e.isIntersecting) seen.add(e.target.id); else seen.delete(e.target.id); });
    const first = heads.find((h) => seen.has(h.id));
    if (first) links.forEach((a) => a.classList.toggle('here', a.hash === '#' + first.id));
  }, { rootMargin: '-80px 0px -55% 0px' });
  heads.forEach((h) => io.observe(h));
}

export function initManual(): void {
  initProse();
  initCopy();
  initToc();
}
