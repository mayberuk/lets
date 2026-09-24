// Every right-hand output is pasted from a run of lets 0.0.1 on the demo project, never written
// by hand; `link` ties a stock turn to the output chunk that makes it unnecessary.

type Link = 'a' | 'b' | 'c';

export interface Pair {
  id: string;
  tab: string;
  title: string;
  say: string;
  turns: [cmd: string, note: string, link: Link][];
  cmd: string;
  fold?: string;
  out: [link: Link | null, text: string][];
}

export const PAIRS: Pair[] = [
  {
    id: 'two', tab: 'Two files', title: 'Two files, one call.',
    say: 'Every line comes back numbered, and each header says which lines you see out of how many.',
    turns: [['cat src/usage.ts', 'read one file', 'a'],
      ['cat src/config.ts', 'then the next', 'b']],
    cmd: 'lets show src/usage.ts src/config.ts',
    out: [['a', `── src/usage.ts  (1-13 of 13) · sha:6fae9e67700e
 1 \timport { usageCap } from './config'
 2 \t
 3 \texport function usage(id: string) {
 4 \t  const now = Date.now()
 5 \t  const cap = 10
 6 \t  if (!id) return
 7 \t  if (count(id) > cap) return
 8 \t  return total(id, now)
 9 \t}
10 \t
11 \tfunction total(id: string, now: number) {
12 \t  return now - count(id)
13 \t}`], ['b', `── src/config.ts  (1-2 of 2) · sha:4f49d457dfea
 1 \texport const usageCap = 10
 2 \texport const retries = 3`], [null, '── showed 2 targets · 15 lines · ~77 tokens']],
  },
  {
    id: 'fn', tab: 'One function', title: 'Just the function, found by parsing.',
    say: 'No grep for where it starts and no guess at where it ends.',
    turns: [["grep -n 'function usage' src/usage.ts", 'find where it starts', 'a'],
      ["sed -n '3,20p' src/usage.ts", 'guess where it ends', 'b']],
    cmd: 'lets show src/usage.ts#usage',
    out: [['a', '── src/usage.ts#usage  (3-9 of 13 · via tree-sitter) · sha:6fae9e67700e'],
      ['b', `3 \texport function usage(id: string) {
4 \t  const now = Date.now()
5 \t  const cap = 10
6 \t  if (!id) return
7 \t  if (count(id) > cap) return
8 \t  return total(id, now)
9 \t}`], [null, '── showed 1 target · 7 lines · ~38 tokens']],
  },
  {
    id: 'find', tab: 'Find, then read', title: 'Every hit, with the lines around it.',
    say: 'One search returns the hits and their context, grouped by file.',
    turns: [['grep -rn usageCap .', 'find the uses', 'a'],
      ["sed -n '1,4p' src/usage.ts", 'read around a hit', 'b']],
    cmd: 'lets find usageCap -C 3',
    out: [['a', `── src/config.ts
1:\texport const «usageCap» = 10
2-\texport const retries = 3`], ['b', `── src/usage.ts
1:\timport { «usageCap» } from './config'
2-\t
3-\texport function usage(id: string) {
4-\t  const now = Date.now()`], [null, '── 2 hits in 2 files · searched 2 files · ~39 tokens']],
  },
  {
    id: 'miss', tab: 'A miss', title: 'A miss names the nearest line.',
    say: 'Exit 1 shows the closest match, so the agent fixes its text without reading the whole file.',
    turns: [['Edit {"old_string": "const cap = 15", …}', 'String to replace not found', 'a'],
      ['cat src/usage.ts', 're-read to see what is there', 'b']],
    cmd: "lets edit src/usage.ts --old 'const cap = 15' --new 'const cap = 20'",
    out: [['a', '--old not found in src/usage.ts'], ['b', '  nearest: line 5\t  const cap = 10'],
      [null, 'ERROR_CODE=not_found']],
  },
  {
    id: 'broken', tab: 'A broken edit', title: 'A broken edit puts itself back.',
    say: 'The file is parsed before and after; a new parse error reverts the edit in the same call.',
    turns: [["sed -i 's/total(id, now)/total(id, now))/' src/usage.ts", 'lands broken', 'a'],
      ['npm test', 'fails, several turns later', 'b'],
      ['Edit {"old_string": "total(id, now))", …}', 'undo it by hand', 'c']],
    cmd: "lets edit src/usage.ts --old 'return total(id, now)' --new 'return total(id, now))'",
    out: [['a', '── src/usage.ts · 1 replacement · line 8 · REVERTED'],
      [null, ` 6 \t  if (!id) return
 7 \t  if (count(id) > cap) return`],
      ['b', ' 8~\t  return total(id, now))          ← parse error'],
      [null, ` 9 \t}
10 \t`],
      ['c', '── check: failed → reverted · file unchanged · sha:6fae9e67700e · ~40 tokens'],
      [null, `structure check failed for src/usage.ts: failed
ERROR_CODE=check_failed`]],
  },
  {
    id: 'batch', tab: 'A batch', title: 'Two files change together, or neither does.',
    say: 'One batch on stdin; every edit is matched and checked before any file is written.',
    turns: [['Edit {"file_path": "src/config.ts", …}', 'edit one file', 'a'],
      ['Edit {"file_path": "src/usage.ts", …}', 'edit the other', 'b'],
      ['cat src/config.ts', 're-read to confirm', 'c'],
      ['cat src/usage.ts', 're-read to confirm', 'c']],
    cmd: "lets edit --from - <<'LETS'", fold: 'the 12-line batch on stdin is folded on this page',
    out: [['a', `── src/config.ts · 1 replacement · line 1 · exact
1~\texport const usageLimit = 10
2 \texport const retries = 3`], ['b', `── src/usage.ts · 1 replacement · line 1 · exact
1~\timport { usageLimit } from './config'
2 \t
3 \texport function usage(id: string) {`],
      ['c', '── 2 files · 2 edits · all applied · checks: structure ok ×2 · ~60 tokens']],
  },
  {
    id: 'cfg', tab: 'Config values', title: 'Set a value; comments and order stay.',
    say: 'transform edits JSON, YAML, TOML and frontmatter by key, then checks the file still parses.',
    turns: [['cat package.json', 'read it', 'a'],
      ['Edit {"old_string": "\\"version\\": \\"1.3.0\\"", …}', 'edit by exact text', 'b'],
      ['cat package.json', 'confirm it still parses', 'c']],
    cmd: 'lets transform package.json --set version=1.4.0',
    out: [['a', '── package.json · json · set version · line 3'],
      [null, `1 \t{
2 \t  "name": "usage-meter",`],
      ['b', '3~\t  "version": "1.4.0",'],
      [null, `4 \t  "scripts": {
5 \t    "test": "vitest"`],
      ['c', '── check: json ok · sha:cac96a379708→6c53277fd361 · ~21 tokens']],
  },
];

const esc = (t: string) => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

function fmtLine(line: string): string {
  const h = esc(line).replace(/«([^»]*)»/g, '<mark>«$1»</mark>');
  if (line.startsWith('── ')) return '<span class="rl">' + h.replace('REVERTED', '<span class="err">REVERTED</span>') + '</span>';
  if (line.includes('← parse error')) return '<span class="bad">' + h.replace('← parse error', '<em>← parse error</em>') + '</span>';
  if (/^\s*\d+~\t/.test(line)) return '<span class="chg">' + h + '</span>';
  if (line.startsWith('ERROR_CODE=')) return '<span class="err">' + h + '</span>';
  return h;
}

export function paneOut(p: Pair): string {
  return p.out.map(([link, text]) => {
    const body = text.split('\n').map(fmtLine).join('\n');
    return link ? `<span class="chunk" data-chunk="${link}">${body}</span>` : `<span class="plain">${body}</span>`;
  }).join('');
}

/** The first line of each linked chunk, cut to 60 characters: what a screen reader hears as the turn's replacement. */
export function labels(p: Pair): Record<string, string> {
  const names: Record<string, string> = {};
  for (const [link, text] of p.out) {
    if (link && !(link in names)) names[link] = text.split('\n')[0].trim().slice(0, 60);
  }
  return names;
}
