export const d = document;
export const root = d.documentElement;
export const $ = <T extends Element = HTMLElement>(s: string, c: ParentNode = d): T => c.querySelector(s) as T;
export const $$ = <T extends Element = HTMLElement>(s: string, c: ParentNode = d): T[] => Array.from(c.querySelectorAll<T>(s));
export const reduceQ = matchMedia('(prefers-reduced-motion: reduce)');
export const deskQ = matchMedia('(min-width: 1100px)');
export const still = () => reduceQ.matches || !('animate' in Element.prototype);
export const EASE = 'cubic-bezier(0.16, 1, 0.3, 1)';
export const say = (t: string) => { const live = $('#live'); live.textContent = ''; setTimeout(() => { live.textContent = t; }, 40); };
export const esc = (t: string) => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
export const center = (r: DOMRect): [number, number] => [r.left + r.width / 2, r.top + r.height / 2];

const WALLS = [
  ['graphite', 'Graphite', '#3A3F45', '#2F3338', '#4F5861', '#7C858E'],
  ['pistachio', 'Pistachio', '#C9D8B4', '#A3B98D', '#7E9A6A', '#F1F4EA'],
  ['apricot', 'Apricot', '#F0C7A0', '#DDA879', '#B7835A', '#FBEBDD'],
  ['lagoon', 'Lagoon', '#8FC4BE', '#72ACA6', '#4F8B86', '#E3F1EF'],
];

export const byId: Record<string, HTMLElement> = {};
let wins: HTMLElement[] = [];
let readme: HTMLElement | null = null;
let doc: HTMLElement | null = null;
let bar: HTMLElement | null = null;
let z = 3;

function sideOf(w: HTMLElement) { return w.closest<HTMLElement>('.side'); }

export function focusWin(w: HTMLElement | null) {
  if (!w) return;
  wins.forEach((x) => x.classList.toggle('is-focus', x === w));
  if (w === readme) doc?.classList.add('readme-top');
  else {
    doc?.classList.remove('readme-top');
    const s = sideOf(w);
    if (s) { if (z > 400) { $$('.side').forEach((x) => { x.style.zIndex = ''; }); z = 3; } s.style.zIndex = String(++z); }
  }
  syncTasks();
}

export function iconFor(id: string) {
  const t = $('#taskbar [data-task="' + id + '"]');
  if (t && t.offsetParent) return t.getBoundingClientRect();
  const i = $('.icons [data-for="' + id + '"]');
  return i ? $('svg', i).getBoundingClientRect() : null;
}

function note(w: HTMLElement) { const n = sideOf(w) && w.nextElementSibling; return n && n.classList.contains('min-note') ? n : null; }
function setNote(w: HTMLElement, verb: string) {
  const n = note(w);
  if (!n) return;
  $('span', n).textContent = w.dataset.title + ' is ' + verb + '.';
  $('button', n).textContent = verb === 'closed' ? 'Reopen' : 'Restore';
}

export function toward(w: HTMLElement, rect: DOMRect | null, out: boolean, done: () => void) {
  if (still() || !rect) {
    if (still() && 'animate' in Element.prototype) {
      w.animate(out ? [{ opacity: 1 }, { opacity: 0 }] : [{ opacity: 0 }, { opacity: 1 }], { duration: 150 }).onfinish = done;
    } else done();
    return;
  }
  const r = w.getBoundingClientRect();
  const [cx, cy] = center(r), [tx, ty] = center(rect);
  const s = Math.max(0.06, Math.min(rect.width / r.width, 0.2));
  const far = { transform: 'translate(' + (tx - cx) + 'px,' + (ty - cy) + 'px) scale(' + s + ')', opacity: 0 };
  const near = { transform: 'none', opacity: 1 };
  const a = w.animate(out ? [near, far] : [far, near], { duration: out ? 280 : 400, easing: out ? 'cubic-bezier(0.7, 0, 0.84, 0)' : EASE });
  a.onfinish = done;
}

export function minimise(w: HTMLElement) {
  if (w.classList.contains('is-min')) return;
  if (w.classList.contains('is-max')) unmax(w, true);
  toward(w, iconFor(w.dataset.win!), true, () => {
    w.classList.add('is-min'); setNote(w, 'minimised'); syncTasks();
    say(w.dataset.title + ' minimised.');
  });
}

function closeWin(w: HTMLElement) {
  if (w.classList.contains('is-max')) unmax(w, true);
  const fin = () => { w.classList.add('is-shut'); setNote(w, 'closed'); syncTasks(); say(w.dataset.title + ' closed. Reopen it from its place in the page.'); const n = note(w); if (n) $('button', n).focus(); };
  if (still()) { fin(); return; }
  w.animate([{ opacity: 1, transform: 'none' }, { opacity: 0, transform: 'scale(.96)' }], { duration: 160, easing: 'ease-in' }).onfinish = fin;
}

export function restore(w: HTMLElement) {
  const wasShut = w.classList.contains('is-shut');
  w.classList.remove('is-min', 'is-shut');
  focusWin(w);
  toward(w, wasShut ? null : iconFor(w.dataset.win!), false, () => {});
  if (wasShut && !still()) w.animate([{ opacity: 0, transform: 'scale(.96)' }, { opacity: 1, transform: 'none' }], { duration: 220, easing: EASE });
  syncTasks();
}

function flipMax(w: HTMLElement, change: () => void) {
  const a = w.getBoundingClientRect();
  change();
  if (still()) return;
  const b = w.getBoundingClientRect();
  w.animate([
    { transformOrigin: '0 0', transform: 'translate(' + (a.left - b.left) + 'px,' + (a.top - b.top) + 'px) scale(' + (a.width / b.width) + ',' + (a.height / b.height) + ')' },
    { transformOrigin: '0 0', transform: 'none' },
  ], { duration: 300, easing: EASE });
}
function maxBtn(w: HTMLElement) { return $('[data-act="max"]', w); }
function max(w: HTMLElement) {
  const s = sideOf(w); if (s) s.style.zIndex = '1000';
  flipMax(w, () => { w.classList.add('is-max'); });
  const b = maxBtn(w); if (b) { b.setAttribute('aria-label', 'Restore ' + w.dataset.title + ' to its size'); b.setAttribute('aria-pressed', 'true'); }
  focusWin(w);
  if (s) s.style.zIndex = '1000';
}
function unmax(w: HTMLElement, quiet = false) {
  const s = sideOf(w);
  const change = () => { w.classList.remove('is-max'); };
  if (quiet) change(); else flipMax(w, change);
  if (s) s.style.zIndex = String(++z);
  const b = maxBtn(w); if (b) { b.setAttribute('aria-label', 'Maximise ' + w.dataset.title); b.setAttribute('aria-pressed', 'false'); }
}

/** Adds a taskbar button for window `id`; without `click`, it restores, focuses or minimises the window. */
export function addTask(id: string, icon: string, click?: () => void) {
  const w = byId[id];
  if (!w || !bar) return;
  const b = d.createElement('button');
  b.type = 'button'; b.dataset.task = id;
  b.innerHTML = '<svg viewBox="0 0 48 48" aria-hidden="true"><use href="#' + icon + '"/></svg><span>' + esc(w.dataset.title!) + '</span>';
  b.addEventListener('click', click || (() => {
    if (w.classList.contains('is-min') || w.classList.contains('is-shut')) { restore(w); return; }
    const r = w.getBoundingClientRect();
    const inView = r.bottom > 60 && r.top < innerHeight - 60;
    if (w !== readme && w.classList.contains('is-focus') && inView) { minimise(w); return; }
    focusWin(w);
    if (!inView) (w === readme ? d.getElementById('top')! : w).scrollIntoView({ behavior: still() ? 'auto' : 'smooth', block: w === readme ? 'start' : 'center' });
  }));
  bar.appendChild(b);
}

export function syncTasks() {
  if (!bar) return;
  $$('button[data-task]', bar).forEach((b) => {
    const w = byId[b.dataset.task!];
    // A shut window and the docked Trash are both display: none, so neither has a box.
    b.hidden = !w.getClientRects().length;
    b.classList.toggle('min', w.classList.contains('is-min'));
    b.setAttribute('aria-pressed', String(w.classList.contains('is-focus') && !w.classList.contains('is-min')));
    b.title = w.classList.contains('is-min') ? 'Restore ' + w.dataset.title : w.dataset.title!;
  });
}

let toastT = 0;
export function hideToast() { const t = $('#toast'); t.classList.remove('show'); t.setAttribute('aria-hidden', 'true'); }
export function showToast() {
  const t = $('#toast');
  t.classList.add('show'); t.setAttribute('aria-hidden', 'false');
  clearTimeout(toastT); toastT = window.setTimeout(hideToast, 3600);
}

function initWallpaper() {
  const appBtn = $<HTMLButtonElement>('#appearance');
  let picker: HTMLElement | null = null, pickerReturn: HTMLElement | null = null;
  const currentWall = () => root.dataset.wall || 'graphite';
  function setWall(name: string) {
    root.dataset.wall = name;
    try { localStorage.setItem('lets-wall', name); } catch (e) {}
    const w = WALLS.find((x) => x[0] === name)!;
    say('Wallpaper: ' + w[1] + '.');
  }
  function closePicker(restoreFocus: boolean) {
    if (!picker) return;
    picker.remove(); picker = null;
    appBtn.setAttribute('aria-expanded', 'false');
    if (restoreFocus && pickerReturn) pickerReturn.focus();
  }
  function openPicker(x: number, y: number, from: HTMLElement | null) {
    closePicker(false);
    pickerReturn = from || appBtn;
    const p = d.createElement('div');
    picker = p;
    p.className = 'picker';
    p.setAttribute('role', 'dialog');
    p.setAttribute('aria-labelledby', 'picker-h');
    const cur = currentWall();
    p.innerHTML = '<h2 id="picker-h">Wallpaper</h2><fieldset><legend class="vh">Wallpaper</legend>' +
      WALLS.map((w) => '<label><input type="radio" name="wall" value="' + w[0] + '"' + (w[0] === cur ? ' checked' : '') + '>' +
        '<span class="sw-prev" style="--s0:' + w[2] + ';--s1:' + w[3] + ';--s2:' + w[4] + ';--s3:' + w[5] + '"><i></i><i></i><i></i></span>' + w[1] + '</label>').join('') +
      '</fieldset>';
    d.body.appendChild(p);
    const pw = p.offsetWidth, ph = p.offsetHeight;
    p.style.left = Math.max(8, Math.min(x, innerWidth - pw - 8)) + 'px';
    p.style.top = Math.max(8, Math.min(y, innerHeight - ph - 8)) + 'px';
    appBtn.setAttribute('aria-expanded', 'true');
    p.addEventListener('change', (e) => setWall((e.target as HTMLInputElement).value));
    p.addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); closePicker(true); } });
    p.addEventListener('focusout', (e) => { const to = e.relatedTarget as Node | null; if (picker && to && !picker.contains(to)) closePicker(false); });
    $('input:checked', p).focus();
  }
  appBtn.addEventListener('click', () => {
    if (picker) { closePicker(true); return; }
    const r = appBtn.getBoundingClientRect();
    openPicker(r.right - 292, r.bottom + 6, appBtn);
  });
  d.addEventListener('pointerdown', (e) => { const t = e.target as Node; if (picker && !picker.contains(t) && !appBtn.contains(t)) closePicker(false); });
  d.addEventListener('contextmenu', (e) => {
    if ((e.target as Element).closest('.win, .panel, .icons, .rd, .foot, .taskbar, .picker, .toast, .min-note, a, button, input, pre, code')) return;
    e.preventDefault();
    openPicker(e.clientX, e.clientY, null);
  });
}

function initWindows() {
  readme = $('.readme-bg');
  doc = $('.doc');
  bar = $('#taskbar');
  wins = $$('[data-win]');
  wins.forEach((w) => { byId[w.dataset.win!] = w; });
  d.addEventListener('pointerdown', (e) => {
    const t = e.target as Element;
    const w = t.closest<HTMLElement>('[data-win]');
    if (w) focusWin(w);
    else if (t.closest('.rd, .foot')) focusWin(readme);
  });
  d.addEventListener('focusin', (e) => {
    const t = e.target as Element;
    const w = t.closest<HTMLElement>('[data-win]');
    if (w) { if (!w.classList.contains('is-focus')) focusWin(w); }
    else if (readme && t.closest('.rd, .foot') && !readme.classList.contains('is-focus')) focusWin(readme);
  });

  $$('.ctl [data-act]').forEach((b) => {
    const w = b.closest<HTMLElement>('[data-win]')!;
    const act = b.dataset.act as 'min' | 'max' | 'close';
    b.setAttribute('aria-label', { min: 'Minimise ', max: 'Maximise ', close: 'Close ' }[act] + w.dataset.title);
    if (act === 'max') b.setAttribute('aria-pressed', 'false');
    b.addEventListener('click', () => {
      if (act === 'min') minimise(w);
      else if (act === 'close') closeWin(w);
      else if (w.classList.contains('is-max')) unmax(w); else max(w);
    });
  });
  $$('[data-restore]').forEach((b) => b.addEventListener('click', () => { const w = byId[b.dataset.restore!]; restore(w); const f = $('.ctl button', w); if (f) f.focus(); }));
  d.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    const m = $('.win.is-max');
    if (m) { unmax(m); const b = maxBtn(m); if (b) b.focus(); }
  });

  let drag: { w: HTMLElement; sx: number; sy: number; x0: number; y0: number; r: DOMRect } | null = null;
  $$('.drag-ok > .hb').forEach((hb) => {
    const w = hb.parentElement!;
    hb.addEventListener('pointerdown', (e) => {
      if (!deskQ.matches || e.pointerType !== 'mouse' || e.button !== 0 || (e.target as Element).closest('button, a') || w.classList.contains('is-max')) return;
      const [x0, y0] = (w.style.translate || '0px 0px').split(' ').map(parseFloat);
      drag = { w, sx: e.clientX, sy: e.clientY, x0: x0 || 0, y0: y0 || 0, r: w.getBoundingClientRect() };
      hb.setPointerCapture(e.pointerId);
      d.body.classList.add('dragging');
      e.preventDefault();
    });
    hb.addEventListener('pointermove', (e) => {
      if (!drag || drag.w !== w) return;
      let dx = e.clientX - drag.sx, dy = e.clientY - drag.sy;
      const top = parseFloat(getComputedStyle(root).getPropertyValue('--panel-h')) || 34;
      dx = Math.max(-drag.r.left + 8, Math.min(dx, innerWidth - drag.r.left - 120));
      dy = Math.max(-drag.r.top + top + 4, Math.min(dy, innerHeight - drag.r.top - 60));
      w.style.translate = (drag.x0 + dx) + 'px ' + (drag.y0 + dy) + 'px';
    });
    const end = () => { if (drag && drag.w === w) { drag = null; d.body.classList.remove('dragging'); } };
    hb.addEventListener('pointerup', end);
    hb.addEventListener('pointercancel', end);
    hb.addEventListener('dblclick', (e) => { if (!deskQ.matches || (e.target as Element).closest('button, a')) return; if (w.classList.contains('is-max')) unmax(w); else max(w); });
  });
}

export function initDesktop(): void {
  initWallpaper();
  initWindows();
  $('#toast-x').addEventListener('click', hideToast);
}
