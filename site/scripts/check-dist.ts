#!/usr/bin/env bun
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
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

function selfTest(): void {
  const dist = process.argv[3] && process.argv[3] !== '--self-test' ? process.argv[3] : DEFAULT_DIST;
  if (!existsSync(dist)) {
    console.error(`${dist} does not exist; run "bun run build" first`);
    process.exit(1);
  }
  const tmp = mkdtempSync(join(tmpdir(), 'check-dist-self-test-'));
  try {
    Bun.spawnSync(['cp', '-r', `${dist}/.`, tmp]);

    const indexPath = join(tmp, 'index.html');
    const original = readFileSync(indexPath, 'utf8');
    const mutated = original.replace(/<link rel="canonical" href="[^"]*">\n?/, '');
    if (mutated === original) throw new Error('self-test setup: canonical link not found in index.html to remove');
    writeFileSync(indexPath, mutated);

    const llmsPath = join(tmp, 'llms.txt');
    const llmsOriginal = readFileSync(llmsPath, 'utf8');
    const llmsMutated = llmsOriginal.replace(`${SITE}/docs/show.md`, `${SITE}/docs/does-not-exist.md`);
    if (llmsMutated === llmsOriginal) throw new Error('self-test setup: llms.txt link to mutate not found');
    writeFileSync(llmsPath, llmsMutated);

    const errors = checkDist(tmp);
    const sawMissingCanonical = errors.some((e) => e.includes('index.html') && e.includes('missing canonical'));
    const sawBrokenLlmsLink = errors.some((e) => e.includes('does-not-exist.md'));
    if (!sawMissingCanonical || !sawBrokenLlmsLink) {
      console.error('self-test FAILED: expected both a missing-canonical and a broken-llms-link error, got:');
      for (const e of errors) console.error(`  ${e}`);
      process.exit(1);
    }
    console.log('self-test passed: both injected failures were caught');
    console.log(`  - ${errors.find((e) => e.includes('index.html') && e.includes('missing canonical'))}`);
    console.log(`  - ${errors.find((e) => e.includes('does-not-exist.md'))}`);
  } finally {
    rmSync(tmp, { recursive: true, force: true });
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
