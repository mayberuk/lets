import type { APIRoute } from 'astro';
import { getEntry } from 'astro:content';
import { markdownOf, faqMarkdown } from '../lib/llms';

export const GET: APIRoute = async () => {
  const home = await getEntry('home', 'index');
  if (!home) throw new Error('src/content/home/index.md is missing');
  const body = markdownOf('lets', home.body) + '\n' + faqMarkdown();
  return new Response(body, { headers: { 'Content-Type': 'text/markdown; charset=utf-8' } });
};
