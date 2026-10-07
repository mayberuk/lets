import type { EngineInterface, Register, ToolCallResult } from 'claude-code';

import { decide, dropPreferDedicatedTools, rewriteBashDescription, singleRun, VERSION, type Decision } from './steer';

const CLASSIFY_TIMEOUT_MS = 5000;
const VERSION_TIMEOUT_MS = 2000;
const PASS: Decision = { kind: 'pass' };

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

// Auto mode refuses an input a hook changed and tells the model to issue the call again as recorded.
function refusedChangedInput(result: ToolCallResult): boolean {
  return result.isError === true && (result.text ?? '').includes("changed this call's input");
}

export const register: Register = (on) => {
  // Without it, a refused change would be retried as recorded and changed again, every time.
  // Maps each refused original command to the replacement that was tried.
  const refused = new Map<string, string>();

  on('tool.describe', { tool: 'Bash' }, async ($, e, next) => {
    const r = await next(e);
    return { ...r, description: rewriteBashDescription(r.description) };
  }).catch(($, e, next) => next(e));

  on('prompt.compose', async ($, e, next) => {
    const r = await next(e);
    let changed = false;
    const sections = r.sections.map((section) => {
      if (section.id !== 'tools') return section;
      const text = dropPreferDedicatedTools(section.text);
      if (text === section.text) return section;
      changed = true;
      return { ...section, text };
    });
    return changed ? { ...r, sections } : r;
  }).catch(($, e, next) => next(e));

  on('tool.call', { tool: 'Bash' }, async ($, e, next) => {
    if (e.tool !== 'Bash' || refused.has(e.command)) return next(e);
    const decision = await classify($, e.command);
    if (decision.kind === 'pass') return next(e);
    const r = await next({ ...e, command: decision.command });
    if (r.deny !== undefined || refusedChangedInput(r)) {
      refused.set(e.command, decision.command);
      return r;
    }
    if (decision.kind === 'rewrite') return r;
    return { ...r, context: [...(r.context ?? []), decision.note] };
  }).catch(($, e, next) => next(e));

  // Always calls next: answering in its place would skip the user's other settings hooks.
  on('classic.PreToolUse', { tool: 'Bash' }, async ($, e, next) => {
    const r = await next(e);
    const tried = e.tool === 'Bash' ? refused.get(e.command) : undefined;
    if (tried === undefined) return r;
    if (r.updatedInput?.command === tried) {
      const { updatedInput, ...rest } = r;
      return rest;
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
