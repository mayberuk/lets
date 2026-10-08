import type { EngineInterface, Register, ToolCallResult } from 'claude-code';

import {
  decide,
  dropPreferDedicatedTools,
  rewriteBashDescription,
  rewriteBashFirstSteer,
  singleRun,
  VERSION,
  type Decision,
} from './steer';

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

export const register: Register = (on) => {
  // Without it, a refused change would be retried as recorded and changed again, every time.
  // Maps each refused original command, keyed with its session, to the replacement that was tried.
  const refused = new Map<string, string>();

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
