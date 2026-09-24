import type { CollectionEntry } from 'astro:content';
import { faq } from '../data/faq';

const url = (path: string) => new URL(path, import.meta.env.SITE).href;

const inOrder = (docs: CollectionEntry<'docs'>[]) => [...docs].sort((a, b) => a.data.order - b.data.order);

/** The markdown twin of a page: its raw body, which carries its own `# ` heading. */
export function markdownOf(title: string, body: string | undefined): string {
  const md = (body ?? '').trim();
  return (md.startsWith('# ') ? md : `# ${title}\n\n${md}`) + '\n';
}

/** The home page's FAQ, rendered from the same array the page and its JSON-LD read, so the two can't drift. */
export function faqMarkdown(): string {
  return ['## Questions', '', ...faq.flatMap((f) => [`### ${f.q}`, '', f.a, ''])].join('\n').trimEnd() + '\n';
}

/** Exit codes and their slugs, read from the exit-codes page so this summary cannot drift from the table `tests/site_docs.rs` checks. */
function exitSummary(docs: CollectionEntry<'docs'>[]) {
  const table = docs.find((e) => e.id === 'exit-codes')?.body ?? '';
  const slugs = new Map<string, string[]>();
  for (const [, code, slug, means] of table.matchAll(/^\| (\d+) \| ([^|]+) \| ([^|]+) \|/gm)) {
    slugs.set(code, [...(slugs.get(code) ?? []), slug.trim() === '—' ? means.trim() : slug.trim()]);
  }
  if (!slugs.has('0') || !slugs.has('64')) throw new Error('src/content/docs/exit-codes.md: the exit-code table did not parse');
  return [...slugs].map(([code, s]) => `${code} ${s.join(', ')}`).join('; ');
}

/** An llmstxt.org index: title, blockquote summary, detail paragraphs, then the link sections. */
export function llmsTxt(docs: CollectionEntry<'docs'>[]): string {
  const links = inOrder(docs).map((e) => `- [${e.data.title}](${url(`/docs/${e.id}.md`)}): ${e.data.description}`);
  return [
    '# lets',
    '',
    '> `lets` is a file-operations CLI for coding agents: Locate, Edit, Transform, Show. One Bash call reads, searches, edits or transforms several files and returns bounded, numbered output whose last line (the footer) names everything it left out. One static Rust binary; no index, no daemon, no config file.',
    '',
    'Install on Linux or macOS with `curl -fsSL https://raw.githubusercontent.com/mayberuk/lets/main/install.sh | sh`, then `lets hooks install claude-code` (or `lets hooks install codex`).',
    '',
    "Run it through Bash the way you already run `cat` or `grep`: `lets show`, `lets find`, `lets edit`, `lets transform`, `lets write`. Pass several files or ranges to one call instead of one call per file; every verb accepts a list. If the footer doesn't name a cut, nothing was cut, so trust the output and skip the follow-up read. In particular, never `cat` or `sed -n` a file right after a `lets edit` on it: the changed region and a syntax-check result are already in that edit's own output. Run `lets` without `| head`: output is already bounded (200 lines per file, 50 search hits by default), and a call over budget exits non-zero and says so rather than truncating silently.",
    '',
    `Branch on the exit code or the last stderr line, \`ERROR_CODE=<slug>\`, not on the message text: ${exitSummary(docs)}. Full table: ${url('/docs/exit-codes.md')}.`,
    '',
    "The target grammar every verb speaks: `path`, `path:40`, `path:40-80`, `path@'regex'`, `path#symbolName`. `find` needs only a path or directory, not a target, but every hit it prints is itself a valid target (`path:line`) for a following `show` or `edit`.",
    '',
    '## Docs',
    '',
    ...links,
    '',
    '## Optional',
    '',
    `- [Product overview](${url('/index.md')}): what \`lets\` replaces, the two ideas behind it, five before/after pairs, measured results and install.`,
    `- [Full reference in one file](${url('/llms-full.txt')}): the product overview and every page above concatenated, for a single fetch that teaches the whole API.`,
    '',
  ].join('\n');
}

export function llmsFull(home: CollectionEntry<'home'>, docs: CollectionEntry<'docs'>[]): string {
  const pages = inOrder(docs);
  const header = [
    '# lets — the complete API reference',
    '',
    '> Locate · Edit · Transform · Show, a file-operations CLI for coding agents. The product overview, then every verb, flag, exit code and JSON shape in one file, so a single fetch teaches the whole tool.',
    `> Individual pages: ${[url('/index.md'), ...pages.map((e) => url(`/docs/${e.id}.md`))].join(', ')}.`,
    '',
  ].join('\n');
  return [header, markdownOf('lets', home.body) + '\n' + faqMarkdown(), ...pages.map((e) => markdownOf(e.data.title, e.body))].join(
    '\n---\n\n',
  );
}
