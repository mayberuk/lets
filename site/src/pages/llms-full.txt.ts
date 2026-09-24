import type { APIRoute } from 'astro';
import { getCollection, getEntry } from 'astro:content';
import { llmsFull } from '../lib/llms';

export const GET: APIRoute = async () => {
  const home = await getEntry('home', 'index');
  if (!home) throw new Error('src/content/home/index.md is missing');
  return new Response(llmsFull(home, await getCollection('docs')), { headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
};
