// The cache TTL is 300 s; prior keep-alive tools use 240 s because a ping after expiry costs more than none.
export const PING_INTERVAL_MS = 240_000;
// Break-even on the 5-minute tier is about 46 min; 20 min avoided most rewrites in a simulation on real pauses.
export const IDLE_CAP_MS = 20 * 60_000;
export const PING_PROMPT = 'Reply with the single character k.';

export function cacheTtl(
  env: (name: string) => string | undefined,
  rateLimits: readonly { percentUsed: number }[],
): '5m' | '1h' {
  const force = env('FORCE_PROMPT_CACHING_5M');
  if (force !== undefined && force !== '' && force !== '0' && force !== 'false') return '5m';
  const ttl = env('CLAUDE_CODE_PROMPT_CACHE_TTL');
  if (ttl === '5m' || ttl === '1h') return ttl;
  const enable = env('ENABLE_PROMPT_CACHING_1H');
  if (enable !== undefined && enable !== '' && enable !== '0' && enable !== 'false') return '1h';
  if (rateLimits.length === 0 || rateLimits.some((window) => window.percentUsed >= 100)) return '5m';
  return '1h';
}

export function keepWarmDisabled(env: (name: string) => string | undefined): boolean {
  const value = env('LETS_KEEP_WARM');
  return value === '0' || value === 'false';
}
