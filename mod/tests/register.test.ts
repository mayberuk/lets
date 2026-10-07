import type { On, PreToolUseResult, ProcessRunInit } from 'claude-code';
import { describe, expect, test } from 'claude-code/testing';

import { LETS_TABLE, VERSION } from '../hooks/steer';

const CWD = '/work/repo';

type Answer = { exitCode: number; stdout: string } | 'reject';

type World = {
  session: string;
  ran: string[];
  timeouts: (number | undefined)[];
  runs: { argv: readonly string[]; init?: ProcessRunInit }[];
  toasts: string[];
};

// Stands in for everything beneath the plugin: the session's cwd and id, `lets`, the Bash tool and the toast line.
function world(
  on: On,
  answer: (argv: readonly string[]) => Answer,
  refuse: (command: string) => boolean = () => false,
  fail: (command: string) => string | undefined = () => undefined,
): World {
  const seen: World = { session: 'session-a', ran: [], timeouts: [], runs: [], toasts: [] };
  on('session.cwd', () => ({ value: CWD }));
  on('session.id', () => ({ value: seen.session }));
  on('process.run', ($, e) => {
    seen.runs.push({ argv: e.argv, init: e.init });
    const reply = answer(e.argv);
    if (reply === 'reject') return { deny: 'lets: command not found' };
    return {
      value: { exitCode: reply.exitCode, stdout: reply.stdout, stderr: '', isStdoutTruncated: false, isStderrTruncated: false },
    };
  });
  on('ui.toast', ($, e) => {
    seen.toasts.push(e.text);
    return { value: undefined };
  });
  on('tool.call', ($, e) => {
    if (e.tool !== 'Bash') return { deny: `unexpected tool ${e.tool}` };
    seen.ran.push(e.command);
    seen.timeouts.push(e.timeout);
    if (refuse(e.command)) return { deny: 'refused by the person' };
    const failure = fail(e.command);
    if (failure !== undefined) return { result: failure, isError: true, text: failure };
    return { result: { stdout: 'ok\n', stderr: '', interrupted: false } };
  });
  return seen;
}

// Stands in for the installed settings hook, which runs the same classifier beneath the plugin.
function settingsHook(on: On, verdict: (command: string) => PreToolUseResult): void {
  on('classic.PreToolUse', ($, e) => (e.tool === 'Bash' ? verdict(e.command) : {}));
}

const classifyAnswers = (stdout: string, exitCode = 0) => (): Answer => ({ exitCode, stdout });

const REWRITTEN = 'lets show src/a.ts --all --no-header --no-numbers';
const REWRITE = JSON.stringify({
  hookSpecificOutput: { hookEventName: 'PreToolUse', updatedInput: { command: REWRITTEN } },
});

const ORIGINAL_EDIT = "sed -i 's/cap/limit/g' README.md";
const REPLACEMENT = "lets edit README.md --old 'cap' --new 'limit' --all";
const TEACHING = 'lets edit replaces exact text and checks the result.';
const denyWith = (reason: string) =>
  JSON.stringify({
    hookSpecificOutput: { hookEventName: 'PreToolUse', permissionDecision: 'deny', permissionDecisionReason: reason },
  });
const ONE_RUN = denyWith(`${TEACHING}\nrun: ${REPLACEMENT}`);
const TWO_RUNS = denyWith('lets show reads several files and ranges in one call.\nrun: lets show a.ts\nrun: lets show b.ts');
const NOTE = `lets ran \`${REPLACEMENT}\` in place of the command you wrote: ${TEACHING}`;

describe('tool.call on Bash', () => {
  test('sends classify the PreToolUse event with the session cwd and no turn_id', async ($, on) => {
    const seen = world(on, classifyAnswers(''));

    await $.tool.call({ tool: 'Bash', command: 'cat src/a.ts', timeout: 9000 });

    expect(seen.runs).toHaveLength(1);
    expect(seen.runs[0]!.argv).toEqual(['lets', 'hook', 'classify']);
    expect(seen.runs[0]!.init?.timeoutMs).toBe(5000);
    expect(seen.runs[0]!.init?.env).toBe(undefined);
    expect(JSON.parse(seen.runs[0]!.init?.stdin ?? '')).toStrictEqual({
      hook_event_name: 'PreToolUse',
      tool_name: 'Bash',
      cwd: CWD,
      tool_input: { command: 'cat src/a.ts' },
    });
  });

  test('a rewrite runs the new command and adds no note', async ($, on) => {
    const seen = world(on, classifyAnswers(REWRITE));

    const r = await $.tool.call({ tool: 'Bash', command: 'cat src/a.ts' });

    expect(seen.ran).toEqual([REWRITTEN]);
    expect(r.context).toBe(undefined);
  });

  test('an allow runs the command as written', async ($, on) => {
    const seen = world(on, classifyAnswers(''));

    const r = await $.tool.call({ tool: 'Bash', command: 'git status' });

    expect(seen.ran).toEqual(['git status']);
    expect(r.context).toBe(undefined);
  });

  test('a deny with one run line runs that command and notes it for the model', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN));

    const r = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT]);
    expect(r.context).toEqual([NOTE]);
  });

  test('a deny with two run lines runs the command as written', async ($, on) => {
    const seen = world(on, classifyAnswers(TWO_RUNS));

    const r = await $.tool.call({ tool: 'Bash', command: 'cat a.ts; cat b.ts' });

    expect(seen.ran).toEqual(['cat a.ts; cat b.ts']);
    expect(r.context).toBe(undefined);
  });

  test('classify failing to start runs the command as written', async ($, on) => {
    const seen = world(on, () => 'reject');

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([ORIGINAL_EDIT]);
  });

  test('classify exiting 1 runs the command as written, whatever it printed', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN, 1));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([ORIGINAL_EDIT]);
  });

  test('classify printing something unparsable runs the command as written', async ($, on) => {
    const seen = world(on, classifyAnswers('{"hookSpecificOutput":'));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([ORIGINAL_EDIT]);
  });

  test('a refused replacement is passed back, and the same command is not changed again', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), (command) => command === REPLACEMENT);

    const first = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(first.context).toBe(undefined);
    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(seen.runs).toHaveLength(1);
  });

  test('a refused rewrite is passed back, and the same command is not changed again', async ($, on) => {
    const seen = world(on, classifyAnswers(REWRITE), (command) => command === REWRITTEN);

    await $.tool.call({ tool: 'Bash', command: 'cat src/a.ts' });
    await $.tool.call({ tool: 'Bash', command: 'cat src/a.ts' });

    expect(seen.ran).toEqual([REWRITTEN, 'cat src/a.ts']);
  });

  test('an auto-mode refusal of a changed input is passed back, and the same command is not changed again', async ($, on) => {
    const refusal =
      "Auto mode gave no verdict for Bash: a hook changed this call's input after the model wrote it, so the review was of different input from what would run";
    const seen = world(on, classifyAnswers(ONE_RUN), () => false, (command) => (command === REPLACEMENT ? refusal : undefined));

    const first = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(first.context).toBe(undefined);
    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(seen.runs).toHaveLength(1);
  });

  test('a permission prompt the person declines is passed back, and the same command is not changed again', async ($, on) => {
    const declined =
      "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";
    const seen = world(on, classifyAnswers(ONE_RUN), () => false, (command) => (command === REPLACEMENT ? declined : undefined));

    const first = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(first.text).toBe(declined);
    expect(first.context).toBe(undefined);
    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(seen.runs).toHaveLength(1);
  });

  test('a permission prompt declined with feedback is not changed again', async ($, on) => {
    const declined =
      "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). To tell you how to proceed, the user said:\nuse sed";
    const seen = world(on, classifyAnswers(ONE_RUN), () => false, (command) => (command === REPLACEMENT ? declined : undefined));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
  });

  test('a permission rule denying the replacement is not changed again', async ($, on) => {
    const denied = `Permission to use Bash with command ${REPLACEMENT} has been denied.`;
    const seen = world(on, classifyAnswers(ONE_RUN), () => false, (command) => (command === REPLACEMENT ? denied : undefined));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
  });

  test('a replacement that exits non-zero with a permission error on stderr is changed again', async ($, on) => {
    const failure = "Exit code 1\nThe user doesn't want to proceed with this tool use.\nsed: couldn't open README.md: Permission denied";
    const seen = world(on, classifyAnswers(ONE_RUN), () => false, (command) => (command === REPLACEMENT ? failure : undefined));

    const first = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(first.context).toEqual([NOTE]);
    expect(seen.ran).toEqual([REPLACEMENT, REPLACEMENT]);
  });

  test('a refusal in one session does not exempt the same command in the next', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), (command) => command === REPLACEMENT && seen.session === 'session-a');

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    seen.session = 'session-b';
    const second = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, REPLACEMENT]);
    expect(second.context).toEqual([NOTE]);
    expect(seen.runs).toHaveLength(2);
  });

  test('a refusal still exempts the same command later in its own session', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), (command) => command === REPLACEMENT);

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(seen.runs).toHaveLength(1);
  });

  test('a replacement that ran and failed keeps its note and is changed again', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), () => false, () => 'error: `--old` not found in README.md');

    const first = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(first.context).toEqual([NOTE]);
    expect(seen.ran).toEqual([REPLACEMENT, REPLACEMENT]);
  });

  test('a replacement that ran is changed again the next time', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    const second = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, REPLACEMENT]);
    expect(second.context).toEqual([NOTE]);
  });
});

describe('classic.PreToolUse on Bash after a refusal', () => {
  const OTHER = 'lets show README.md --all';
  const rewriteTo = (command: string) => (typed: string) => (typed === ORIGINAL_EDIT ? { updatedInput: { command } } : {});
  const denyAs = (reason: string) => (typed: string) => (typed === ORIGINAL_EDIT ? { deny: reason } : {});
  const refusedReplacement = (command: string) => command === REPLACEMENT;

  test('the retry reaches the tool as typed although the settings hook rewrites it to the tried replacement', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), refusedReplacement);
    settingsHook(on, rewriteTo(REPLACEMENT));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    const retry = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(retry.isError).toBe(undefined);
  });

  test('the retry reaches the tool as typed although the settings hook denies it with the tried replacement', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), refusedReplacement);
    settingsHook(on, denyAs(`${TEACHING}\nrun: ${REPLACEMENT}`));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    const retry = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(retry.isError).toBe(undefined);
  });

  test('the retry keeps the settings hook other changes when it also rewrites to the tried replacement', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), refusedReplacement);
    settingsHook(on, (typed) => (typed === ORIGINAL_EDIT ? { updatedInput: { command: REPLACEMENT, timeout: 999 } } : {}));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT, timeout: 9000 });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT, timeout: 9000 });

    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(seen.timeouts).toEqual([9000, 999]);
  });

  test('the retry keeps its own input when the settings hook changed only the command', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), refusedReplacement);
    settingsHook(on, rewriteTo(REPLACEMENT));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT, timeout: 9000 });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT, timeout: 9000 });

    expect(seen.ran).toEqual([REPLACEMENT, ORIGINAL_EDIT]);
    expect(seen.timeouts).toEqual([9000, 9000]);
  });

  test('a refusal in one session leaves the settings hook rewrite in the next', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), (command) => command === REPLACEMENT && seen.session === 'session-a');
    settingsHook(on, rewriteTo(REPLACEMENT));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    seen.session = 'session-b';
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, REPLACEMENT]);
  });

  test('a settings hook rewriting the retry to a different command keeps its rewrite', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), refusedReplacement);
    settingsHook(on, rewriteTo(OTHER));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT, OTHER]);
  });

  test('a settings hook denying the retry with a different run line keeps its deny', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), refusedReplacement);
    settingsHook(on, denyAs(`${TEACHING}\nrun: ${OTHER}`));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    const retry = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT]);
    expect(retry.isError).toBe(true);
    expect(retry.text).toContain(OTHER);
  });

  test('a settings hook denying the retry with other text keeps its deny', async ($, on) => {
    const seen = world(on, classifyAnswers(ONE_RUN), refusedReplacement);
    settingsHook(on, denyAs('blocked by team policy'));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });
    const retry = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT]);
    expect(retry.isError).toBe(true);
    expect(retry.text).toContain('blocked by team policy');
  });

  test('a command that was never refused keeps the settings hook rewrite', async ($, on) => {
    const seen = world(on, classifyAnswers(''));
    settingsHook(on, rewriteTo(REPLACEMENT));

    await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([REPLACEMENT]);
  });

  test('a command that was never refused keeps the settings hook deny', async ($, on) => {
    const seen = world(on, classifyAnswers(''));
    settingsHook(on, denyAs(`${TEACHING}\nrun: ${REPLACEMENT}`));

    const r = await $.tool.call({ tool: 'Bash', command: ORIGINAL_EDIT });

    expect(seen.ran).toEqual([]);
    expect(r.isError).toBe(true);
  });
});

describe('session.start', () => {
  const start = (on: On) => on('session.start', ($, e) => ({ cwd: e.cwd }));
  const versionAnswers = (version: string) => () => ({ exitCode: 0, stdout: `{"version":"${version}"}\n` });

  test('a different lets version shows one toast naming both and the fix, and runs nothing else', async ($, on) => {
    start(on);
    const seen = world(on, versionAnswers('9.9.9'));

    await $.session.start({ cwd: CWD, surface: null, isInteractive: false });

    expect(seen.runs.map((run) => run.argv)).toEqual([['lets', 'version', '--json']]);
    expect(seen.runs[0]!.init?.timeoutMs).toBe(2000);
    expect(seen.toasts).toHaveLength(1);
    expect(seen.toasts[0]).toContain('9.9.9');
    expect(seen.toasts[0]).toContain(VERSION);
    expect(seen.toasts[0]).toContain('lets hooks install claude-code');
  });

  test('the same lets version shows no toast', async ($, on) => {
    start(on);
    const seen = world(on, versionAnswers(VERSION));

    await $.session.start({ cwd: CWD, surface: null, isInteractive: false });

    expect(seen.runs).toHaveLength(1);
    expect(seen.toasts).toEqual([]);
  });

  test('lets missing shows no toast and the session still starts', async ($, on) => {
    start(on);
    const seen = world(on, () => 'reject');

    const r = await $.session.start({ cwd: CWD, surface: null, isInteractive: false });

    expect(r).toEqual({ cwd: CWD });
    expect(seen.toasts).toEqual([]);
  });
});

const BASH_DESCRIPTION =
  "Executes a given bash command and returns its output.\n\nIMPORTANT: Avoid using this tool to run `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user:\n\n - Read files: Use Read (NOT cat/head/tail)\n - Edit files: Use Edit (NOT sed/awk)\n - Write files: Use Write (NOT echo >/cat <<EOF)\n - Communication: Output text directly (NOT echo/printf)\nWhile the Bash tool can do similar things, it’s better to use the built-in tools as they provide a better user experience and make it easier to review tool calls and give permission.\n# Instructions\n";
const ENGINE = { plugin: 'engine', tier: 'core' } as const;

describe('tool.describe', () => {
  test('the Bash description carries the lets table in place of the steer paragraph', async ($, on) => {
    on('tool.describe', ($, e) => ({ description: e.description }));

    const r = await $.tool.describe({ tool: 'Bash', description: BASH_DESCRIPTION, provider: ENGINE });

    expect(r.description).toBe(
      `Executes a given bash command and returns its output.\n\n${LETS_TABLE}\n\n# Instructions\n`,
    );
  });

  test('the lean Bash description, which has no steer paragraph, gets the table after its first paragraph', async ($, on) => {
    on('tool.describe', ($, e) => ({ description: e.description }));
    const lean = 'Executes a bash command and returns its output.\n\n- Command output is displayed to you, not reliably to the user.\n';

    const r = await $.tool.describe({ tool: 'Bash', description: lean, provider: ENGINE });

    expect(r.description).toBe(
      `Executes a bash command and returns its output.\n\n${LETS_TABLE}\n\n- Command output is displayed to you, not reliably to the user.\n`,
    );
  });

  test('another tool keeps its description', async ($, on) => {
    on('tool.describe', ($, e) => ({ description: e.description }));

    const r = await $.tool.describe({ tool: 'Read', description: BASH_DESCRIPTION, provider: ENGINE });

    expect(r.description).toBe(BASH_DESCRIPTION);
  });
});

const PREFER = ' - Prefer dedicated tools over Bash when one fits (Read, Edit, Write) — reserve Bash for shell-only operations.\n';
const PARALLEL = ' - You can call multiple tools in a single response.';

const COMPOSE = {
  model: 'claude-opus-5-5',
  promptModel: 'claude-opus-5-5',
  surfaces: [],
  tools: ['Bash', 'Read'],
  outputStyle: null,
  traits: [],
} as const;

describe('prompt.compose', () => {
  test('the tools section loses the prefer-dedicated-tools line and keeps its id and scope', async ($, on) => {
    const intro = { id: 'intro', text: PREFER, scope: 'shared' } as const;
    on('prompt.compose', () => ({
      sections: [intro, { id: 'tools', text: `# Using your tools\n${PREFER}${PARALLEL}`, scope: 'shared' }],
    }));

    const r = await $.prompt.compose(COMPOSE);

    expect(r.sections).toEqual([intro, { id: 'tools', text: `# Using your tools\n${PARALLEL}`, scope: 'shared' }]);
  });

  test('the lean body loses the prefer sentence, keeps the rest of the line, and keeps its id and scope', async ($, on) => {
    const lean = ' - Prefer the dedicated file/search tools over shell commands when one fits. Independent tool calls can run in parallel in one response.\n';
    const other = { id: 'memory', text: lean, scope: 'session' } as const;
    on('prompt.compose', () => ({
      sections: [{ id: 'lean_body', text: `# Harness\n${lean} - Reference code.`, scope: 'shared' }, other],
    }));

    const r = await $.prompt.compose({ ...COMPOSE, traits: ['lean'] });

    expect(r.sections).toEqual([
      { id: 'lean_body', text: '# Harness\n - Independent tool calls can run in parallel in one response.\n - Reference code.', scope: 'shared' },
      other,
    ]);
  });

  test('a tools section without the line is sent as it was', async ($, on) => {
    const sections = [{ id: 'tools', text: `# Using your tools\n${PARALLEL}`, scope: 'shared' }] as const;
    on('prompt.compose', () => ({ sections }));

    const r = await $.prompt.compose(COMPOSE);

    expect(r.sections).toEqual(sections);
  });
});
