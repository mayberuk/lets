import { describe, expect, test } from 'claude-code/testing';

import { cacheTtl, keepWarmDisabled, IDLE_CAP_MS, PING_INTERVAL_MS, PING_PROMPT } from '../hooks/warm';

const environment = (values: Record<string, string>) => (name: string) => values[name];

describe('cacheTtl', () => {
  const cases: { name: string; env: Record<string, string>; windows: number[]; ttl: '5m' | '1h' }[] = [
    { name: 'no windows means 5m', env: {}, windows: [], ttl: '5m' },
    { name: 'every subscription window below 100 means 1h', env: {}, windows: [0, 99.9], ttl: '1h' },
    { name: 'a window at 100 means 5m', env: {}, windows: [0, 100], ttl: '5m' },
    { name: 'a window over 100 means 5m', env: {}, windows: [101], ttl: '5m' },
    { name: 'force beats enable and explicit 1h', env: { FORCE_PROMPT_CACHING_5M: '1', ENABLE_PROMPT_CACHING_1H: '1', CLAUDE_CODE_PROMPT_CACHE_TTL: '1h' }, windows: [0], ttl: '5m' },
    { name: 'explicit 5m beats enable and subscription', env: { CLAUDE_CODE_PROMPT_CACHE_TTL: '5m', ENABLE_PROMPT_CACHING_1H: '1' }, windows: [0], ttl: '5m' },
    { name: 'explicit 1h without windows', env: { CLAUDE_CODE_PROMPT_CACHE_TTL: '1h' }, windows: [], ttl: '1h' },
    { name: 'explicit 1h beats exceeded windows', env: { CLAUDE_CODE_PROMPT_CACHE_TTL: '1h' }, windows: [100], ttl: '1h' },
    { name: 'enable without windows', env: { ENABLE_PROMPT_CACHING_1H: '1' }, windows: [], ttl: '1h' },
    { name: 'enable beats exceeded windows', env: { ENABLE_PROMPT_CACHING_1H: '1' }, windows: [100], ttl: '1h' },
    { name: 'invalid explicit TTL falls through', env: { CLAUDE_CODE_PROMPT_CACHE_TTL: 'bad' }, windows: [0], ttl: '1h' },
    { name: 'empty explicit TTL falls through', env: { CLAUDE_CODE_PROMPT_CACHE_TTL: '' }, windows: [], ttl: '5m' },
  ];
  for (const { name, env, windows, ttl } of cases) {
    test(name, () => expect(cacheTtl(environment(env), windows.map((percentUsed) => ({ percentUsed })))).toBe(ttl));
  }
  for (const value of ['', '0', 'false']) {
    test(`force ${JSON.stringify(value)} is unset`, () => {
      expect(cacheTtl(environment({ FORCE_PROMPT_CACHING_5M: value }), [{ percentUsed: 0 }])).toBe('1h');
    });
    test(`enable ${JSON.stringify(value)} is unset`, () => {
      expect(cacheTtl(environment({ ENABLE_PROMPT_CACHING_1H: value }), [])).toBe('5m');
    });
  }
  for (const value of ['true', 'other']) {
    test(`nonempty force ${value} is set`, () => {
      expect(cacheTtl(environment({ FORCE_PROMPT_CACHING_5M: value }), [{ percentUsed: 0 }])).toBe('5m');
    });
    test(`nonempty enable ${value} is set`, () => {
      expect(cacheTtl(environment({ ENABLE_PROMPT_CACHING_1H: value }), [])).toBe('1h');
    });
  }
});

describe('keepWarmDisabled', () => {
  for (const value of [undefined, '', '0', 'false', '1', 'true', 'bad']) {
    test(`${JSON.stringify(value)} only disables for 0 or false`, () => {
      expect(keepWarmDisabled(() => value)).toBe(value === '0' || value === 'false');
    });
  }
});

test('timing and prompt match the keep-warm contract', () => {
  expect(PING_INTERVAL_MS).toBe(240_000);
  expect(IDLE_CAP_MS).toBe(1_200_000);
  expect(PING_PROMPT).toBe('Reply with the single character k.');
});
