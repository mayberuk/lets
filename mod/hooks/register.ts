import type { EngineInterface, Register, Timer, ToolCallResult } from 'claude-code';

import {
  decide,
  dropPreferDedicatedTools,
  rewriteBashDescription,
  rewriteBashFirstSteer,
  singleRun,
  VERSION,
  type Decision,
} from './steer';

import { cacheTtl, keepWarmDisabled, IDLE_CAP_MS, PING_INTERVAL_MS, PING_PROMPT } from './warm';

const CLASSIFY_TIMEOUT_MS = 5000;
const VERSION_TIMEOUT_MS = 2000;
const PASS: Decision = { kind: 'pass' };
// The full prompt carries the prefer-dedicated-tools line in `tools`; the lean prompt carries its sentence in `lean_body`.
const STEERED_SECTIONS = new Set(['tools', 'lean_body']);

async function classify($: Pick<EngineInterface, 'session' | 'process'>, command: string): Promise<Decision> {
  try {
    // No `turn_id`: its presence is how the classifier tells a Codex event from a Claude Code one.
    const stdin = JSON.stringify({
      hook_event_name: 'PreToolUse',
      tool_name: 'Bash',
      cwd: await $.session.cwd(),
      tool_input: { command },
    });
    const run = await $.process.run(['lets', 'hook', 'classify'], { stdin, timeoutMs: CLASSIFY_TIMEOUT_MS });
    if (run.exitCode !== 0 || run.isStdoutTruncated) return PASS;
    return decide(run.stdout);
  } catch {
    return PASS;
  }
}

// Core's own denial texts (engine 2.1.293): a declined permission prompt, a permission rule, an approval nobody
// could give. A Bash command that ran and failed reads `Exit code N` first, so it never matches.
const PERMISSION_REFUSALS = ["The user doesn't want to proceed with this tool use.", 'Permission to use ', 'Permission for this '];

function refusedByCore(result: ToolCallResult): boolean {
  if (result.isError !== true) return false;
  const text = result.text ?? '';
  // Auto mode refuses an input a hook changed and tells the model to issue the call again as recorded.
  return text.includes("changed this call's input") || PERMISSION_REFUSALS.some((prefix) => text.startsWith(prefix));
}

// `/clear` and an in-process resume change the session id without a `session.start`.
async function refusalKey($: Pick<EngineInterface, 'session'>, command: string): Promise<string> {
  return JSON.stringify([await $.session.id(), command]);
}

type WarmState = {
  timer?: Timer;
  lastResponse?: number;
  inFlight: boolean;
  generation: number;
};

function cancelWarm(state: WarmState): void {
  const timer = state.timer;
  state.timer = undefined;
  try {
    timer?.cancel();
  } catch {}
}

async function ping($: EngineInterface, state: WarmState): Promise<void> {
  const generation = state.generation;
  state.timer = undefined;
  try {
    const disabled = await $.env.get('LETS_KEEP_WARM');
    if (keepWarmDisabled(() => disabled)) {
      $.ui.log('lets keep-warm: disabled', { to: 'debug' });
      return;
    }
    if (state.lastResponse === undefined || (await $.clock.now()) - state.lastResponse > IDLE_CAP_MS) {
      $.ui.log('lets keep-warm: idle-cap', { to: 'debug' });
      return;
    }
    const env: Record<string, string | undefined> = {
      FORCE_PROMPT_CACHING_5M: await $.env.get('FORCE_PROMPT_CACHING_5M'),
      CLAUDE_CODE_PROMPT_CACHE_TTL: await $.env.get('CLAUDE_CODE_PROMPT_CACHE_TTL'),
      ENABLE_PROMPT_CACHING_1H: await $.env.get('ENABLE_PROMPT_CACHING_1H'),
    };
    if (cacheTtl((name) => env[name], (await $.session.usage()).rateLimits) === '1h') {
      $.ui.log('lets keep-warm: 1h', { to: 'debug' });
      return;
    }
    if (state.generation !== generation || state.inFlight) return;
    state.inFlight = true;
    try {
      const reply = await $.model.fork({ prompt: PING_PROMPT });
      if (!reply.isAnswered) {
        $.ui.log('lets keep-warm: fork-failed', { to: 'debug' });
        return;
      }
    } finally {
      state.inFlight = false;
    }
    $.ui.log('lets keep-warm: 5m', { to: 'debug' });
    if (state.generation === generation) {
      state.timer = $.clock.after(PING_INTERVAL_MS, () => { void ping($, state); });
    }
  } catch {
    if (state.generation === generation) cancelWarm(state);
    try {
      $.ui.log('lets keep-warm: fork-failed', { to: 'debug' });
    } catch {}
  }
}

export const register: Register = (on) => {
  // Without it, a refused change would be retried as recorded and changed again, every time.
  // Maps each refused original command, keyed with its session, to the replacement that was tried.
  const refused = new Map<string, string>();
  const warm: WarmState = { inFlight: false, generation: 0 };

  on('turn.step', async function* ($, e, next) {
    if (e.agentId !== undefined || warm.inFlight) return yield* next(e);
    const generation = ++warm.generation;
    cancelWarm(warm);
    const result = yield* next(e);
    try {
      const now = await $.clock.now();
      if (warm.generation === generation) {
        warm.lastResponse = now;
        warm.timer = $.clock.after(PING_INTERVAL_MS, () => { void ping($, warm); });
      }
    } catch {
      if (warm.generation === generation) cancelWarm(warm);
    }
    return result;
  }).catch(async function* ($, e, next) {
    warm.generation += 1;
    cancelWarm(warm);
    return yield* next(e);
  });

  on('session.end', ($, e, next) => {
    warm.generation += 1;
    warm.lastResponse = undefined;
    cancelWarm(warm);
    return next(e);
  }).catch(($, e, next) => next(e));

  on('tool.describe', { tool: 'Bash' }, async ($, e, next) => {
    const r = await next(e);
    return { ...r, description: rewriteBashDescription(r.description) };
  }).catch(($, e, next) => next(e));

  on('prompt.compose', async ($, e, next) => {
    const r = await next(e);
    let changed = false;
    const sections = r.sections.map((section) => {
      if (!STEERED_SECTIONS.has(section.id)) return section;
      const text = dropPreferDedicatedTools(section.text);
      if (text === section.text) return section;
      changed = true;
      return { ...section, text };
    });
    return changed ? { ...r, sections } : r;
  }).catch(($, e, next) => next(e));

  on('session.append', { door: 'attachment' }, async ($, e, next) => {
    let changed = false;
    const content = e.message.content.map((block) => {
      if (block.type !== 'text') return block;
      const text = rewriteBashFirstSteer(block.text);
      if (text === block.text) return block;
      changed = true;
      return { ...block, text };
    });
    return next(changed ? { ...e, message: { ...e.message, content } } : e);
  }).catch(($, e, next) => next(e));

  on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
    if (e.tool !== 'Bash') return next(e);
    const key = await refusalKey($, e.command);
    if (refused.has(key)) return next(e);
    const decision = await classify($, e.command);
    if (decision.kind === 'pass') return next(e);
    const r = await next({ ...e, command: decision.command });
    if (r.deny !== undefined || refusedByCore(r)) {
      refused.set(key, decision.command);
      return r;
    }
    if (decision.kind === 'rewrite') return r;
    return { ...r, context: [...(r.context ?? []), decision.note] };
  }).catch(($, e, next) => next(e));

  // Always calls next: answering in its place would skip the user's other settings hooks.
  on('classic.PreToolUse', { tool: 'Bash' }, async ($, e, next) => {
    const r = await next(e);
    if (e.tool !== 'Bash') return r;
    const tried = refused.get(await refusalKey($, e.command));
    if (tried === undefined) return r;
    if (r.updatedInput?.command === tried) {
      const { updatedInput, ...rest } = r;
      const { command, ...others } = updatedInput;
      // updatedInput replaces the whole input, so a kept field carries the typed command back with it.
      return Object.keys(others).length === 0 ? rest : { ...rest, updatedInput: { ...others, command: e.command } };
    }
    if (r.deny !== undefined && singleRun(r.deny) === tried) {
      const { deny, ...rest } = r;
      return rest;
    }
    return r;
  }).catch(($, e, next) => next(e));

  on('session.start', async ($, e, next) => {
    try {
      const run = await $.process.run(['lets', 'version', '--json'], { timeoutMs: VERSION_TIMEOUT_MS });
      const installed: unknown = JSON.parse(run.stdout).version;
      if (typeof installed === 'string' && installed !== VERSION) {
        $.ui.toast(
          `lets ${installed} is installed but its Claude Code mod is ${VERSION} · run lets hooks install claude-code`,
        );
      }
    } catch {}
    return next(e);
  }).catch(($, e, next) => next(e));
};
