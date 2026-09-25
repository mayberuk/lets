import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { z } from 'astro/zod';

const docs = defineCollection({
  loader: glob({ pattern: '**/*.md', base: './src/content/docs' }),
  schema: z.object({
    title: z.string(),
    description: z.string(),
    order: z.number(),
    group: z.enum(['Verbs', 'Setup', 'Reference']),
    commands: z.array(z.string()),
  }),
});

const home = defineCollection({
  loader: glob({ pattern: '**/*.md', base: './src/content/home' }),
});

export const collections = { docs, home };
