import type { APIRoute, GetStaticPaths } from 'astro';
import { getCollection, type CollectionEntry } from 'astro:content';
import { markdownOf } from '../../lib/llms';

export const getStaticPaths = (async () =>
  (await getCollection('docs')).map((entry) => ({ params: { slug: entry.id }, props: { entry } }))) satisfies GetStaticPaths;

export const GET: APIRoute<{ entry: CollectionEntry<'docs'> }> = ({ props }) =>
  new Response(markdownOf(props.entry.data.title, props.entry.body), { headers: { 'Content-Type': 'text/markdown; charset=utf-8' } });
