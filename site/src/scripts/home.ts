import {
  d, $, $$, deskQ, still, EASE, say, esc, center, byId,
  focusWin, restore, toward, syncTasks, addTask, showToast, initDesktop,
} from './desktop';

const TASKS: [id: string, icon: string][] = [
  ['readme', 'i-readme'], ['anat', 'i-folder'], ['two', 'i-readme'], ['term', 'i-terminal'], ['cmp', 'i-folder'],
  ['sheet', 'i-sheet'], ['hook', 'i-shield'], ['fm', 'i-folder'], ['txt', 'i-txt'], ['trash', 'i-trash'],
];
const BOTH = 'curl -fsSL https://raw.githubusercontent.com/mayberuk/lets/main/install.sh | sh\nlets hooks install claude-code';

function initTrash() {
  const trash = byId.trash, trashIcon = $('#trash-icon');
  let trashFrom: HTMLElement | null = null;
  function openTrash(from: HTMLElement | null) {
    trashFrom = from || trashIcon;
    if (!trash.classList.contains('floating')) {
      trash.classList.add('floating');
      trashIcon.setAttribute('aria-expanded', 'true');
      toward(trash, $('svg', trashIcon).getBoundingClientRect(), false, () => {});
    }
    focusWin(trash);
    trash.focus({ preventScroll: true });
    syncTasks();
  }
  function closeTrash() {
    const fin = () => { trash.classList.remove('floating'); trashIcon.setAttribute('aria-expanded', 'false'); syncTasks(); if (trashFrom) trashFrom.focus(); };
    toward(trash, $('svg', trashIcon).getBoundingClientRect(), true, fin);
  }
  trashIcon.setAttribute('aria-expanded', 'false');
  $('#open-trash').addEventListener('click', (e) => openTrash(e.currentTarget as HTMLElement));
  $('[data-close-trash]').addEventListener('click', closeTrash);
  trash.addEventListener('keydown', (e) => { if (e.key === 'Escape' && trash.classList.contains('floating')) { e.stopPropagation(); closeTrash(); } });
  const items = $$('#trash-files li');
  $('[data-empty]', trash).addEventListener('click', () => {
    const fin = () => {
      trash.classList.add('is-empty'); trashIcon.classList.add('is-empty');
      $('#trash-count').textContent = 'Empty';
      say('Trash emptied. The hook catches these for you.');
      $('[data-putback]', trash).focus();
    };
    if (still()) { fin(); return; }
    let left = items.length;
    items.forEach((li, i) => {
      li.animate([
        { transform: 'none', borderRadius: '8px', opacity: 1 },
        { transform: 'scale(.72, .5) rotate(-5deg) skewX(8deg)', borderRadius: '30%', opacity: 1, offset: 0.45 },
        { transform: 'translate(-40px, 30px) scale(.06) rotate(-40deg)', borderRadius: '50%', opacity: 0 },
      ], { duration: 460, delay: i * 90, easing: 'cubic-bezier(0.5, 0, 0.75, 0)', fill: 'forwards' }).onfinish = () => {
        if (--left === 0) { items.forEach((x) => x.getAnimations().forEach((a) => a.cancel())); fin(); }
      };
    });
  });
  $('[data-putback]', trash).addEventListener('click', () => {
    trash.classList.remove('is-empty'); trashIcon.classList.remove('is-empty');
    $('#trash-count').textContent = items.length + ' items';
    if (!still()) items.forEach((li, i) => li.animate([{ opacity: 0, transform: 'scale(.8)' }, { opacity: 1, transform: 'none' }], { duration: 260, delay: i * 60, easing: EASE, fill: 'backwards' }));
    say('Three habits put back in the Trash.');
    $('[data-empty]', trash).focus();
  });
  return { openTrash, closeTrash };
}

function initTasks(openTrash: (from: HTMLElement | null) => void, closeTrash: () => void) {
  const trash = byId.trash;
  TASKS.forEach(([id, icon]) => addTask(id, icon, id === 'trash' ? () => {
    if (trash.classList.contains('floating')) { if (trash.classList.contains('is-focus')) closeTrash(); else focusWin(trash); }
  } : undefined));
  const copyTask = d.createElement('button');
  copyTask.type = 'button'; copyTask.className = 'key key-blue bar-copy'; copyTask.dataset.copy = ''; copyTask.hidden = true;
  copyTask.innerHTML = '<svg class="glyph" viewBox="0 0 16 16" aria-hidden="true"><use href="#g-copy"/></svg><svg class="tick" viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 8.5l3 3 6-6.5"/></svg><span class="k-label">Copy install commands</span>';
  $('#taskbar').appendChild(copyTask);

  // Desktop icons restore their window before the link scrolls to it.
  $$<HTMLAnchorElement>('.icons [data-for]').forEach((a) => a.addEventListener('click', (e) => {
    const id = a.dataset.for!;
    if (id === 'trash') { e.preventDefault(); openTrash(a); return; }
    const w = byId[id];
    if (w && (w.classList.contains('is-min') || w.classList.contains('is-shut'))) restore(w);
    if (w) focusWin(w);
  }));
  return copyTask;
}

function initCopy(copyTask: HTMLButtonElement) {
  const canCopy = !!(navigator.clipboard && window.isSecureContext);
  $$<HTMLButtonElement>('[data-copy]').forEach((btn) => {
    if (!canCopy) return;
    if (!btn.classList.contains('bar-copy')) btn.hidden = false;
    const label = $('.k-label', btn), text0 = label.textContent;
    btn.addEventListener('click', async () => {
      btn.classList.add('is-pressed');
      setTimeout(() => btn.classList.remove('is-pressed'), 120);
      try {
        await navigator.clipboard.writeText(BOTH);
        btn.classList.add('is-done'); label.textContent = 'Copied';
        showToast();
        say('Copied 2 commands. Paste them in a terminal.');
        setTimeout(() => { btn.classList.remove('is-done'); label.textContent = text0; }, 2400);
      } catch (err) {
        label.textContent = 'Copy failed: select the commands above';
        setTimeout(() => { label.textContent = text0; }, 3000);
      }
    });
  });
  const firstInstall = $('#install');
  if (canCopy && 'IntersectionObserver' in window) {
    new IntersectionObserver((es) => { es.forEach((en) => { copyTask.hidden = en.isIntersecting || $('#end').getBoundingClientRect().top < innerHeight; }); }).observe(firstInstall);
    addEventListener('scroll', () => { if (!copyTask.hidden && $('#end').getBoundingClientRect().top < innerHeight) copyTask.hidden = true; }, { passive: true });
  }
}

function initAnatomy() {
  const keyed = $$('[data-n]', byId.anat);
  const mark = (n: string | null) => keyed.forEach((el) => el.classList.toggle('on', n !== null && el.dataset.n === n));
  keyed.forEach((el) => {
    const n = el.dataset.n!;
    // Tapping an <li> fires a ghost mouseenter before its click, so a hover binding here
    // would double-toggle it; the legend items rely on click alone.
    if (el.tagName === 'LI') { el.addEventListener('click', () => mark(el.classList.contains('on') ? null : n)); return; }
    el.addEventListener('mouseenter', () => mark(n));
    el.addEventListener('mouseleave', () => mark(null));
    el.addEventListener('focus', () => mark(n));
    el.addEventListener('blur', () => mark(null));
  });
}

function initSwitch() {
  const sw = $('#sw'), swbox = $('#swbox'), swi = $('#swi'), tx = $('#tx');
  const strip = $('#roll'), rollL = $('#roll-l');
  strip.innerHTML = [7, 6, 5, 4, 3, 2].map((n) => '<span>' + n + '</span>').join('');
  function setSwitch(on: boolean) {
    if ((swbox.dataset.on === 'true') === on) return;
    sw.setAttribute('aria-checked', String(on));
    swi.dataset.on = String(on);
    strip.style.transform = on ? 'translateY(-220px)' : 'none';
    rollL.textContent = on ? 'lets on' : 'lets off';
    const stock = $$('#stock .turn');
    if (still()) { swbox.dataset.on = String(on); say(on ? 'lets on: 2 turns.' : 'lets off: 7 turns.'); return; }
    const tr = tx.getBoundingClientRect();
    if (on) {
      const first = stock.map((li) => li.getBoundingClientRect());
      const ghosts = stock.map((li, i) => {
        const g = li.cloneNode(true) as HTMLElement;
        g.classList.add('ghost');
        g.setAttribute('aria-hidden', 'true');
        Object.assign(g.style, { left: (first[i].left - tr.left) + 'px', top: (first[i].top - tr.top) + 'px', width: first[i].width + 'px', transformOrigin: '0 0' });
        return g;
      });
      swbox.dataset.on = 'true';
      const targets = $$('#withlets .turn');
      const tr2 = tx.getBoundingClientRect();
      ghosts.forEach((g) => tx.appendChild(g));
      ghosts.forEach((g, i) => {
        const t = targets[+stock[i].dataset.to!].getBoundingClientRect();
        const f = first[i];
        const dx = t.left - f.left, dy = (t.top - tr2.top) - (f.top - tr.top) + 4;
        g.animate([{ transform: 'none', opacity: 1 }, { transform: 'translate(' + dx + 'px,' + dy + 'px) scaleY(.35)', opacity: 0 }], { duration: 380, delay: i * 28, easing: EASE, fill: 'forwards' }).onfinish = () => g.remove();
      });
      targets.forEach((t, i) => t.animate([{ opacity: 0, transform: 'translateY(8px)' }, { opacity: 1, transform: 'none' }], { duration: 280, delay: 200 + i * 90, easing: EASE, fill: 'backwards' }));
      say('lets on: 2 turns.');
    } else {
      const targets = $$('#withlets .turn').map((t) => t.getBoundingClientRect());
      swbox.dataset.on = 'false';
      stock.forEach((li, i) => {
        const r = li.getBoundingClientRect(), t = targets[+li.dataset.to!];
        li.animate([{ transformOrigin: '0 0', transform: 'translate(' + (t.left - r.left) + 'px,' + (t.top - r.top) + 'px) scaleY(.35)', opacity: 0 }, { transformOrigin: '0 0', transform: 'none', opacity: 1 }], { duration: 340, delay: i * 28, easing: EASE, fill: 'backwards' });
      });
      say('lets off: 7 turns.');
    }
  }
  sw.addEventListener('click', () => setSwitch(sw.getAttribute('aria-checked') !== 'true'));
  $$('.lab', swi).forEach((l) => l.addEventListener('click', () => setSwitch(l.dataset.set === 'true')));
}

function initTerminal() {
  const REC: Record<string, { out: string; code?: number; next: string }> = JSON.parse($('#rec').textContent!);
  const HOOK: Record<string, string> = {
    'cat src/usage.ts': 'lets show reads several files and ranges in one call.\nrun: lets show src/usage.ts',
    "sed -n '3,9p' src/usage.ts": 'lets show reads several files and ranges in one call.\nrun: lets show src/usage.ts:3-9',
    'grep -rn usageCap src': "lets find returns every hit numbered and grouped by file.\nrun: lets find 'usageCap' src",
  };
  const HELP = [
    'lets show src/usage.ts#usage', 'lets show src/usage.ts src/config.ts', 'lets find usageCap', 'lets find cap src',
    "lets edit src/usage.ts --old 'const cap = 10' --new 'const cap = 20'",
    "lets edit src/usage.ts --old 'const cap = 15' --new 'const cap = 20'",
    "lets edit src/usage.ts --old 'return total(id, now)' --new 'return total(id, now))'",
    "lets edit src/usage.ts --old 'return' --new 'return undefined'",
    "cat src/usage.ts   sed -n '3,9p' src/usage.ts   grep -rn usageCap src", 'ls   reset   clear',
  ];
  const out = $('#term-out'), form = $<HTMLFormElement>('#term-form'), inp = $<HTMLInputElement>('#term-in');
  const caret = $('.caret', form), meas = $('.measure', form), line = form;
  let state = 'A', busy = false;
  const hist: string[] = []; let hi = 0;
  out.innerHTML = '<span class="note">Recorded output from lets 0.0.1 on a demo project with two files, src/usage.ts and src/config.ts. Type help for the list.</span>\n';
  function fmt(t: string) {
    return t.replace(/\n$/, '').split('\n').map((l) => {
      const h = esc(l).replace(/«([^»]*)»/g, '<mark>«$1»</mark>');
      if (/^── /.test(l)) return '<span class="rl">' + h.replace('REVERTED', '<span class="err">REVERTED</span>') + '</span>';
      if (/← parse error/.test(l)) return '<span class="bad">' + h + '</span>';
      if (/^\s*\d+~\t/.test(l)) return '<span class="chg">' + h + '</span>';
      if (/^ERROR_CODE=/.test(l)) return '<span class="err">' + h + '</span>';
      return h;
    });
  }
  const tick = (ms: number) => new Promise((r) => setTimeout(r, ms));
  async function print(lines: string[], fast: boolean) {
    for (const l of lines) {
      out.insertAdjacentHTML('beforeend', l + '\n');
      out.scrollTop = out.scrollHeight;
      if (!fast) await tick(14);
    }
  }
  function norm(c: string) { return c.trim().replace(/\s+/g, ' ').replace(/"/g, "'"); }
  async function run(cmd: string, init = false) {
    const fast = still() || init;
    out.insertAdjacentHTML('beforeend', '<span class="ps"><b>demo</b> $</span> ' + esc(cmd) + '\n');
    const n = norm(cmd);
    if (n) { hist.push(cmd); hi = hist.length; }
    const note = (t: string) => '<span class="note">' + esc(t) + '</span>';
    if (!n) { out.scrollTop = out.scrollHeight; return; }
    if (n === 'clear') { out.innerHTML = ''; return; }
    if (n === 'help' || n === '?' || n === 'lets' || n === 'lets --help' || n === 'lets guide') {
      await print([note('This simulation replays these commands:')].concat(HELP.map(esc)), fast); return;
    }
    if (n === 'reset') { state = 'A'; await print([note('The demo project is back to its first state: const cap = 10.')], fast); return; }
    if (n === 'ls') { await print(['src'], fast); return; }
    if (n === 'ls src') { await print(['config.ts  usage.ts'], fast); return; }
    const r = REC[state + '|' + n];
    if (r) {
      await print(fmt(r.out), fast);
      if (r.code) await print([note('exit ' + r.code)], fast);
      state = r.next;
      return;
    }
    if (HOOK[n]) {
      const [msg, run1] = HOOK[n].split('\n');
      await print([note('In an agent session, the lets hook blocks this and hands back:'), '<span class="hookmsg">' + esc(msg) + '\n' + esc(run1) + '</span>' + note('In your own shell it still runs; the hook only answers your agent.')], fast);
      return;
    }
    const w = n.split(' ')[0];
    const head = w === 'lets' ? 'That lets command is not recorded in this simulation. Try one of these:' : w + ': not part of this simulation. Try one of these:';
    await print([note(head)].concat(HELP.slice(0, 5).map(esc)), fast);
  }
  async function submit(cmd: string) {
    if (busy) return;
    busy = true; inp.value = ''; moveCaret();
    await run(cmd);
    busy = false;
  }
  form.addEventListener('submit', (e) => { e.preventDefault(); submit(inp.value); });
  inp.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowUp' && hi > 0) { hi--; inp.value = hist[hi]; e.preventDefault(); }
    else if (e.key === 'ArrowDown') { if (hi < hist.length - 1) { hi++; inp.value = hist[hi]; } else { hi = hist.length; inp.value = ''; } e.preventDefault(); }
    else if (e.key === 'l' && e.ctrlKey) { e.preventDefault(); out.innerHTML = ''; }
    requestAnimationFrame(moveCaret);
  });
  function moveCaret() {
    meas.textContent = inp.value.slice(0, inp.selectionStart == null ? inp.value.length : inp.selectionStart);
    caret.style.left = Math.min(meas.offsetWidth - inp.scrollLeft, inp.clientWidth - 8) + 'px';
    caret.style.animation = 'none'; void caret.offsetWidth; caret.style.animation = '';
  }
  ['input', 'click', 'keyup', 'select', 'focus'].forEach((ev) => inp.addEventListener(ev, moveCaret));
  inp.addEventListener('focus', () => line.classList.add('has-focus'));
  inp.addEventListener('blur', () => line.classList.remove('has-focus'));
  inp.addEventListener('scroll', moveCaret);
  moveCaret();
  if ('IntersectionObserver' in window) new IntersectionObserver((es) => es.forEach((en) => caret.classList.toggle('paused', !en.isIntersecting))).observe(form);
  run('lets show src/usage.ts#usage', true).then(() => { out.scrollTop = 0; });
  $$('.try-list code[data-cmd]').forEach((c) => {
    const b = d.createElement('button');
    b.type = 'button'; b.className = 'cmdlink'; b.dataset.cmd = c.dataset.cmd;
    b.setAttribute('aria-label', 'Run in the terminal: ' + c.dataset.cmd);
    c.replaceWith(b); b.appendChild(c);
  });
  $$('.cmdlink').forEach((b) => b.addEventListener('click', async () => {
    if (busy) return;
    const cmd = b.dataset.cmd!;
    busy = true;
    if (!still()) {
      const step = Math.min(14, 520 / cmd.length);
      for (let i = 1; i <= cmd.length; i++) { inp.value = cmd.slice(0, i); inp.scrollLeft = inp.scrollWidth; moveCaret(); await tick(step); }
      await tick(120);
    }
    busy = false;
    submit(cmd);
  }));
}

function initCompare() {
  const cmp = byId.cmp;
  const tabs = $$('[role="tab"]', cmp), panels = $$('.cmp-panel', cmp);
  let curPanel = panels[0];
  function draw(panel: HTMLElement, animate: boolean) {
    const band = $('.bands', panel), svg = $<SVGSVGElement>('svg', band);
    if (!band.offsetWidth || panel.hasAttribute('data-hide')) return;
    const b = band.getBoundingClientRect(), w = b.width, m = w / 2;
    const chunks: Record<string, DOMRect> = {};
    $$('.chunk', panel).forEach((c) => { chunks[c.dataset.chunk!] = c.getBoundingClientRect(); });
    let paths = '';
    $$('.trow', panel).forEach((r) => {
      const a = r.getBoundingClientRect(), c = chunks[r.dataset.link!];
      if (!c) return;
      const y1 = a.top - b.top, y2 = a.bottom - b.top, y3 = c.top - b.top, y4 = c.bottom - b.top;
      paths += '<path data-link="' + r.dataset.link + '" d="M0 ' + y1 + ' C' + m + ' ' + y1 + ' ' + m + ' ' + y3 + ' ' + w + ' ' + y3 +
        ' L' + w + ' ' + y4 + ' C' + m + ' ' + y4 + ' ' + m + ' ' + y2 + ' 0 ' + y2 + ' Z"/>';
    });
    svg.setAttribute('viewBox', '0 0 ' + w + ' ' + b.height);
    svg.innerHTML = paths;
    if (animate && !still()) $$<SVGPathElement>('path', svg).forEach((p, i) => p.animate([{ transform: 'scaleX(0)', opacity: 0 }, { transform: 'none', opacity: 1 }], { duration: 380, delay: 60 + i * 70, easing: EASE, fill: 'backwards' }));
  }
  function selectTab(i: number, focus: boolean) {
    tabs.forEach((t, j) => { t.setAttribute('aria-selected', String(i === j)); t.tabIndex = i === j ? 0 : -1; });
    panels.forEach((p, j) => { if (i === j) p.removeAttribute('data-hide'); else p.setAttribute('data-hide', ''); });
    curPanel = panels[i];
    draw(curPanel, true);
    if (focus) tabs[i].focus();
  }
  tabs.forEach((t, i) => {
    t.addEventListener('click', () => selectTab(i, false));
    t.addEventListener('keydown', (e) => {
      const n = tabs.length;
      const to = ({ ArrowRight: (i + 1) % n, ArrowLeft: (i - 1 + n) % n, Home: 0, End: n - 1 } as Record<string, number>)[e.key];
      if (to === undefined) return;
      e.preventDefault(); selectTab(to, true);
    });
  });
  function heat(panel: HTMLElement, link: string | null) {
    $$<HTMLElement | SVGElement>('.trow, .chunk, .bands path', panel).forEach((el) => el.classList.toggle('hot', link !== null && (el.dataset.link || el.dataset.chunk) === link));
  }
  panels.forEach((p) => {
    p.addEventListener('pointerover', (e) => { const el = (e.target as Element).closest<HTMLElement>('.trow, .chunk'); heat(p, el ? (el.dataset.link || el.dataset.chunk)! : null); });
    p.addEventListener('pointerleave', () => heat(p, null));
  });
  if ('ResizeObserver' in window) new ResizeObserver(() => draw(curPanel, false)).observe($('.wb', cmp));
  else addEventListener('resize', () => draw(curPanel, false));
  if (d.fonts && d.fonts.ready) d.fonts.ready.then(() => draw(curPanel, false));
}

function initHookDialog() {
  const blocks = $$('#blocks li');
  let bi = 0;
  $('#block-next').addEventListener('click', () => {
    blocks[bi].classList.remove('cur');
    bi = (bi + 1) % blocks.length;
    blocks[bi].classList.add('cur');
    $('#block-count').textContent = (bi + 1) + ' of ' + blocks.length;
    if (!still()) blocks[bi].animate([{ opacity: 0, transform: 'translateY(4px)' }, { opacity: 1, transform: 'none' }], { duration: 200, easing: EASE });
  });
}

function initSheet() {
  const cells = $$<HTMLTableCellElement>('#calc td');
  const fxRef = $('#fx-ref'), fxVal = $('#fx-val');
  const COLS = 'ABC';
  cells.forEach((c, i) => {
    c.tabIndex = i === 2 ? 0 : -1;
    c.addEventListener('focus', () => {
      const tr = c.parentElement!;
      const row = $('.rn', tr).textContent;
      const col = c.colSpan > 1 ? 'A' : COLS[c.cellIndex - 1];
      fxRef.textContent = col + row;
      fxVal.textContent = c.dataset.fx || c.textContent!.trim();
      cells.forEach((x) => { x.tabIndex = x === c ? 0 : -1; });
    });
    c.addEventListener('keydown', (e) => {
      const tr = c.parentElement as HTMLTableRowElement, rows = $$<HTMLTableRowElement>('#calc tbody tr');
      const ri = rows.indexOf(tr);
      let t: Element | null = null;
      if (e.key === 'ArrowRight') t = c.nextElementSibling;
      else if (e.key === 'ArrowLeft') t = c.previousElementSibling;
      else if (e.key === 'ArrowDown' && rows[ri + 1]) t = rows[ri + 1].cells[Math.min(c.cellIndex, rows[ri + 1].cells.length - 1)];
      else if (e.key === 'ArrowUp' && rows[ri - 1]) t = rows[ri - 1].cells[Math.min(c.cellIndex, rows[ri - 1].cells.length - 1)];
      if (t && t.tagName === 'TD') { (t as HTMLElement).focus(); e.preventDefault(); }
    });
  });
}

// The one entrance: the README and anatomy grow out of their desktop icons.
function entrance() {
  if (still() || !deskQ.matches || scrollY > 200) return;
  const grow = (els: HTMLElement[], icon: HTMLElement, delay: number) => {
    const [px, py] = center($('svg', icon).getBoundingClientRect());
    els.forEach((el) => {
      const r = el.getBoundingClientRect();
      el.animate([
        { transformOrigin: (px - r.left) + 'px ' + (py - r.top) + 'px', transform: 'scale(.18)', opacity: 0 },
        { transformOrigin: (px - r.left) + 'px ' + (py - r.top) + 'px', transform: 'none', opacity: 1 },
      ], { duration: 400, delay, easing: EASE, fill: 'backwards' });
    });
  };
  const firstRow = $('.row');
  grow([byId.readme, $('.rd', firstRow)].concat($$('.row:not(:first-child) .rd'), [$('.foot')]), $('.icons [data-for="readme"]'), 0);
  grow([byId.anat], $('.icons [data-for="anat"]'), 80);
}

export function initHome(): void {
  initDesktop();
  const { openTrash, closeTrash } = initTrash();
  const copyTask = initTasks(openTrash, closeTrash);
  initCopy(copyTask);
  initAnatomy();
  initSwitch();
  initTerminal();
  initCompare();
  initHookDialog();
  initSheet();
  focusWin(byId.readme);
  entrance();
  syncTasks();
}
