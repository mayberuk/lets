#!/usr/bin/env bun
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, relative } from 'node:path';

const SITE = 'https://lets.mayberuk.com';
const MAX_DESCRIPTION = 160;
const DEFAULT_DIST = join(import.meta.dir, '..', 'dist');

function listFiles(dir: string, exts?: string[]): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) out.push(...listFiles(full, exts));
    else if (!exts || exts.some((e) => name.endsWith(e))) out.push(full);
  }
  return out;
}

function urlToDistPath(dist: string, url: string): string {
  const path = url.startsWith(SITE) ? url.slice(SITE.length) : url;
  const clean = path.split('#')[0].split('?')[0];
  if (clean === '' || clean.endsWith('/')) return join(dist, clean, 'index.html');
  return join(dist, clean);
}

function checkDist(dist: string): string[] {
  const errors: string[] = [];
  const htmlFiles = listFiles(dist, ['.html']);

  const sitemapIndexPath = join(dist, 'sitemap-index.xml');
  const sitemapLocs = new Set<string>();
  if (existsSync(sitemapIndexPath)) {
    const idx = readFileSync(sitemapIndexPath, 'utf8');
    const subSitemaps = [...idx.matchAll(/<loc>([^<]+)<\/loc>/g)].map((m) => m[1]);
    for (const sub of subSitemaps) {
      const realPath = sub.startsWith(SITE) ? join(dist, sub.slice(SITE.length)) : sub;
      if (!existsSync(realPath)) {
        errors.push(`sitemap-index.xml: sub-sitemap ${sub} not found at ${realPath}`);
        continue;
      }
      const body = readFileSync(realPath, 'utf8');
      for (const m of body.matchAll(/<loc>([^<]+)<\/loc>/g)) sitemapLocs.add(m[1]);
    }
  } else {
    errors.push('sitemap-index.xml is missing');
  }

  const cnamePath = join(dist, 'CNAME');
  if (!existsSync(cnamePath)) errors.push('CNAME is missing');
  else if (readFileSync(cnamePath, 'utf8').trim() !== 'lets.mayberuk.com') {
    errors.push(`CNAME: expected exactly "lets.mayberuk.com", got "${readFileSync(cnamePath, 'utf8').trim()}"`);
  }

  for (const file of listFiles(dist)) {
    const rel = relative(dist, file);
    if (rel.includes('_shots')) errors.push(`${rel}: matches _shots, must not ship in dist`);
    if (rel.endsWith('.png') && rel !== 'og.png') errors.push(`${rel}: PNG other than og.png must not ship in dist`);
  }

  for (const file of htmlFiles) {
    const rel = relative(dist, file);
    const html = readFileSync(file, 'utf8');
    const is404 = rel === '404.html';

    if (!/<title>[^<]+<\/title>/.test(html)) errors.push(`${rel}: missing <title>`);

    const descMatch = html.match(/<meta name="description" content="([^"]*)"/);
    if (!descMatch) errors.push(`${rel}: missing meta description`);
    else if (descMatch[1].length > MAX_DESCRIPTION) {
      errors.push(`${rel}: meta description is ${descMatch[1].length} chars, over ${MAX_DESCRIPTION}`);
    }

    const canonicalMatch = html.match(/<link rel="canonical" href="([^"]*)"/);
    if (!is404) {
      if (!canonicalMatch) errors.push(`${rel}: missing canonical link`);
      else if (!canonicalMatch[1].startsWith(`${SITE}/`)) {
        errors.push(`${rel}: canonical "${canonicalMatch[1]}" does not start with ${SITE}/`);
      } else if (!sitemapLocs.has(canonicalMatch[1])) {
        errors.push(`${rel}: canonical ${canonicalMatch[1]} is not listed in the sitemap`);
      }
    }

    for (const prop of ['og:title', 'og:description', 'og:image']) {
      if (!new RegExp(`<meta property="${prop}" content="[^"]*"`).test(html)) {
        errors.push(`${rel}: missing ${prop}`);
      }
    }

    const ldBlocks = [...html.matchAll(/<script type="application\/ld\+json">([^<]*)<\/script>/g)];
    if (ldBlocks.length === 0) errors.push(`${rel}: no ld+json block found`);
    for (const [i, block] of ldBlocks.entries()) {
      try {
        JSON.parse(block[1]);
      } catch (e) {
        errors.push(`${rel}: ld+json block ${i} does not parse: ${(e as Error).message}`);
      }
    }

    if (!is404) {
      const altMatch = html.match(/<link rel="alternate" type="text\/markdown" href="([^"]*)"/);
      if (!altMatch) errors.push(`${rel}: missing text/markdown alternate link`);
      else {
        const twinPath = join(dist, altMatch[1]);
        if (!existsSync(twinPath)) errors.push(`${rel}: markdown alternate ${altMatch[1]} does not exist in dist`);
      }
    }

    for (const m of html.matchAll(/(?:href|src)="(\/[^"]*)"/g)) {
      const target = m[1];
      if (target.startsWith('//')) continue;
      const resolved = urlToDistPath(dist, target);
      if (!existsSync(resolved)) errors.push(`${rel}: internal link "${target}" does not resolve to a dist file`);
    }
  }

  const llmsPath = join(dist, 'llms.txt');
  if (!existsSync(llmsPath)) {
    errors.push('llms.txt is missing');
  } else {
    const llms = readFileSync(llmsPath, 'utf8');
    const links = new Set(
      [...llms.matchAll(new RegExp(`${SITE}[^\\s)\\]]*`, 'g'))].map((m) => m[0].replace(/[.,;:!?]+$/, '')),
    );
    for (const link of links) {
      const resolved = urlToDistPath(dist, link);
      if (!existsSync(resolved)) errors.push(`llms.txt: link ${link} does not resolve to a dist file`);
    }
  }

  return errors;
}

function copyDist(dist: string, tmp: string): void {
  Bun.spawnSync(['cp', '-r', `${dist}/.`, tmp]);
}

function mustReplace(path: string, pattern: RegExp | string, replacement: string, label: string): void;
function mustReplace(
  path: string,
  pattern: RegExp,
  replacement: (substring: string, ...args: string[]) => string,
  label: string,
): void;
function mustReplace(
  path: string,
  pattern: RegExp | string,
  replacement: string | ((substring: string, ...args: string[]) => string),
  label: string,
): void {
  const original = readFileSync(path, 'utf8');
  const mutated = (original.replace as (p: RegExp | string, r: typeof replacement) => string)(pattern, replacement);
  if (mutated === original) throw new Error(`self-test setup: ${label} not found in ${path}`);
  writeFileSync(path, mutated);
}

type Expected = { pathFragment: string; messageFragment: string; label: string };

function assertCaught(errors: string[], expected: Expected[]): void {
  const missing = expected.filter((x) => !errors.some((e) => e.includes(x.pathFragment) && e.includes(x.messageFragment)));
  if (missing.length > 0) {
    console.error(`self-test FAILED: ${missing.length} expected error(s) not caught:`);
    for (const m of missing) console.error(`  - ${m.label}`);
    console.error('full error list:');
    for (const e of errors) console.error(`  ${e}`);
    process.exit(1);
  }
  console.log(`self-test: ${expected.length} injected failure(s) caught`);
  for (const x of expected) {
    console.log(`  - ${errors.find((e) => e.includes(x.pathFragment) && e.includes(x.messageFragment))}`);
  }
}

function selfTest(): void {
  const dist = process.argv[3] && process.argv[3] !== '--self-test' ? process.argv[3] : DEFAULT_DIST;
  if (!existsSync(dist)) {
    console.error(`${dist} does not exist; run "bun run build" first`);
    process.exit(1);
  }

  const clean = mkdtempSync(join(tmpdir(), 'check-dist-self-test-clean-'));
  try {
    copyDist(dist, clean);
    const cleanErrors = checkDist(clean);
    if (cleanErrors.length > 0) {
      console.error('self-test FAILED: unmutated dist is not clean, got:');
      for (const e of cleanErrors) console.error(`  ${e}`);
      process.exit(1);
    }
    console.log('self-test: unmutated dist has zero errors (positive control)');
  } finally {
    rmSync(clean, { recursive: true, force: true });
  }

  const tmp = mkdtempSync(join(tmpdir(), 'check-dist-self-test-'));
  try {
    copyDist(dist, tmp);

    mustReplace(join(tmp, 'index.html'), /<link rel="canonical" href="[^"]*">\n?/, '', 'canonical link');
    mustReplace(join(tmp, 'docs', 'index.html'), /<title>[^<]+<\/title>/, '', 'title tag');
    mustReplace(
      join(tmp, 'docs', 'show', 'index.html'),
      /<meta name="description" content="[^"]*">/,
      `<meta name="description" content="${'x'.repeat(MAX_DESCRIPTION + 20)}">`,
      'meta description',
    );
    mustReplace(join(tmp, 'docs', 'find', 'index.html'), /<meta name="description" content="[^"]*">/, '', 'meta description');
    mustReplace(
      join(tmp, 'docs', 'edit', 'index.html'),
      /<link rel="canonical" href="[^"]*">/,
      '<link rel="canonical" href="https://example.com/docs/edit/">',
      'canonical link',
    );
    mustReplace(
      join(tmp, 'docs', 'json', 'index.html'),
      /<link rel="canonical" href="[^"]*">/,
      `<link rel="canonical" href="${SITE}/not-in-sitemap/">`,
      'canonical link',
    );
    mustReplace(join(tmp, 'docs', 'stats', 'index.html'), /<meta property="og:title" content="[^"]*">/, '', 'og:title meta');
    mustReplace(
      join(tmp, 'docs', 'hooks', 'index.html'),
      /<script type="application\/ld\+json">[^<]*<\/script>/,
      '',
      'ld+json block',
    );
    mustReplace(
      join(tmp, 'docs', 'exit-codes', 'index.html'),
      /(<script type="application\/ld\+json">)([^<]*)(<\/script>)/,
      (m, open: string, body: string, close: string) => `${open}${body.slice(0, -5)}${close}`,
      'ld+json block',
    );
    mustReplace(
      join(tmp, 'docs', 'transform', 'index.html'),
      /<link rel="alternate" type="text\/markdown"[^>]*>\n?/,
      '',
      'markdown alternate link',
    );
    mustReplace(
      join(tmp, 'docs', 'install', 'index.html'),
      /(<link rel="alternate" type="text\/markdown" href=")([^"]*)(")/,
      '$1/docs/does-not-exist.md$3',
      'markdown alternate link',
    );
    mustReplace(join(tmp, 'docs', 'write', 'index.html'), /href="\/favicon\.svg"/, 'href="/favicon-missing.svg"', 'favicon href');

    const llmsPath = join(tmp, 'llms.txt');
    mustReplace(llmsPath, `${SITE}/docs/show.md`, `${SITE}/docs/does-not-exist.md`, 'llms.txt link');

    writeFileSync(join(tmp, 'CNAME'), 'wrong.example.com');
    writeFileSync(join(tmp, 'stray.png'), '');
    mkdirSync(join(tmp, 'gallery', '_shots'), { recursive: true });
    writeFileSync(join(tmp, 'gallery', '_shots', 'shot.txt'), '');

    const errors = checkDist(tmp);
    assertCaught(errors, [
      { pathFragment: 'index.html', messageFragment: 'missing canonical', label: 'missing canonical (index.html)' },
      { pathFragment: 'does-not-exist.md', messageFragment: 'llms.txt', label: 'broken llms.txt link' },
      { pathFragment: join('docs', 'index.html'), messageFragment: 'missing <title>', label: 'missing title' },
      { pathFragment: join('docs', 'show', 'index.html'), messageFragment: 'over 160', label: 'description over 160 chars' },
      { pathFragment: join('docs', 'find', 'index.html'), messageFragment: 'missing meta description', label: 'missing meta description' },
      { pathFragment: join('docs', 'edit', 'index.html'), messageFragment: 'does not start with', label: 'canonical not https' },
      { pathFragment: join('docs', 'json', 'index.html'), messageFragment: 'not listed in the sitemap', label: 'canonical not in sitemap' },
      { pathFragment: join('docs', 'stats', 'index.html'), messageFragment: 'missing og:title', label: 'missing og:title' },
      { pathFragment: join('docs', 'hooks', 'index.html'), messageFragment: 'no ld+json block found', label: 'no ld+json block' },
      { pathFragment: join('docs', 'exit-codes', 'index.html'), messageFragment: 'does not parse', label: 'ld+json does not parse' },
      { pathFragment: join('docs', 'transform', 'index.html'), messageFragment: 'missing text/markdown alternate', label: 'missing markdown alternate' },
      { pathFragment: join('docs', 'install', 'index.html'), messageFragment: 'does not exist in dist', label: 'markdown alternate points nowhere' },
      { pathFragment: join('docs', 'write', 'index.html'), messageFragment: 'does not resolve to a dist file', label: 'internal href unresolved' },
      { pathFragment: 'CNAME', messageFragment: 'expected exactly', label: 'wrong CNAME content' },
      { pathFragment: 'stray.png', messageFragment: 'PNG other than og.png', label: 'stray PNG' },
      { pathFragment: '_shots', messageFragment: 'matches _shots', label: 'path containing _shots' },
    ]);
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }

  const noSitemap = mkdtempSync(join(tmpdir(), 'check-dist-self-test-no-sitemap-'));
  try {
    copyDist(dist, noSitemap);
    rmSync(join(noSitemap, 'sitemap-index.xml'));
    assertCaught(checkDist(noSitemap), [
      { pathFragment: 'sitemap-index.xml', messageFragment: 'is missing', label: 'missing sitemap-index.xml' },
    ]);
  } finally {
    rmSync(noSitemap, { recursive: true, force: true });
  }

  const noLlms = mkdtempSync(join(tmpdir(), 'check-dist-self-test-no-llms-'));
  try {
    copyDist(dist, noLlms);
    rmSync(join(noLlms, 'llms.txt'));
    assertCaught(checkDist(noLlms), [{ pathFragment: 'llms.txt', messageFragment: 'is missing', label: 'missing llms.txt' }]);
  } finally {
    rmSync(noLlms, { recursive: true, force: true });
  }
}

if (process.argv.includes('--self-test')) {
  selfTest();
} else {
  const dist = process.argv[2] ?? DEFAULT_DIST;
  if (!existsSync(dist)) {
    console.error(`${dist} does not exist; run "bun run build" first`);
    process.exit(1);
  }
  const errors = checkDist(dist);
  if (errors.length > 0) {
    console.error(`check-dist: ${errors.length} problem(s)`);
    for (const e of errors) console.error(`  ${e}`);
    process.exit(1);
  }
  console.log(`check-dist: ok (${listFiles(dist, ['.html']).length} pages)`);
}
