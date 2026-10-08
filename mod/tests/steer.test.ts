import { describe, expect, test } from 'claude-code/testing';

import { decide, dropPreferDedicatedTools, LETS_TABLE, rewriteBashDescription, rewriteBashFirstSteer } from '../hooks/steer';

const BEFORE =
  'Executes a given bash command and returns its output.\n\nThe working directory persists between commands, but shell state does not. The shell environment is initialized from the user\'s profile (bash or zsh).\n\n';
const STEER =
  'IMPORTANT: Avoid using this tool to run `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user:\n\n - Read files: Use Read (NOT cat/head/tail)\n - Edit files: Use Edit (NOT sed/awk)\n - Write files: Use Write (NOT echo >/cat <<EOF)\n - Communication: Output text directly (NOT echo/printf)\nWhile the Bash tool can do similar things, it’s better to use the built-in tools as they provide a better user experience and make it easier to review tool calls and give permission.\n';
const AFTER = '# Instructions\n - If your command will create new directories or files, first verify the parent directory exists.\n';
const BASH_DESCRIPTION = BEFORE + STEER + AFTER;

const LEAN_FIRST = 'Executes a bash command and returns its output.\n\n';
const LEAN_REST =
  "- Working directory persists between calls, but prefer absolute paths — `cd` in a compound command can trigger a permission prompt. Shell state (env vars, functions) does not persist; the shell is initialized from the user's profile.\n- Command output is displayed to you, not reliably to the user.\n";
const LEAN_BASH_DESCRIPTION = LEAN_FIRST + LEAN_REST;

const LEAN_PREFER = ' - Prefer the dedicated file/search tools over shell commands when one fits. Independent tool calls can run in parallel in one response.\n';
const LEAN_HARNESS = `# Harness\n - Text you output outside of tool use is displayed to the user.\n${LEAN_PREFER} - Reference code as \`file_path:line_number\`.`;

const TOOLS_SECTION =
  '# Using your tools\n - Prefer dedicated tools over Bash when one fits (Read, Edit, Write) — reserve Bash for shell-only operations.\n - You can call multiple tools in a single response.';

describe('rewriteBashDescription', () => {
  test('replaces the steer paragraph with the lets table and keeps every other byte', () => {
    const rewritten = rewriteBashDescription(BASH_DESCRIPTION);

    expect(rewritten).toBe(BEFORE + LETS_TABLE + '\n\n' + AFTER);
    expect(rewritten).toContain(LETS_TABLE);
    expect(rewritten).toContain('# Instructions');
    expect(rewritten).not.toContain('Use Read (NOT cat/head/tail)');
    expect(rewritten).not.toContain('IMPORTANT: Avoid');
  });

  test('puts the table after the first paragraph of the lean description, which has no steer paragraph', () => {
    expect(rewriteBashDescription(LEAN_BASH_DESCRIPTION)).toBe(LEAN_FIRST + LETS_TABLE + '\n\n' + LEAN_REST);
  });

  test('keeps a steer paragraph that lost its closing sentence and still adds the table once', () => {
    const cut = BEFORE + STEER.slice(0, STEER.indexOf('While the Bash tool')) + AFTER;
    const rewritten = rewriteBashDescription(cut);

    expect(rewritten).toBe('Executes a given bash command and returns its output.\n\n' + LETS_TABLE + '\n\n' + cut.slice(cut.indexOf('\n\n') + 2));
    expect(rewritten.split(LETS_TABLE)).toHaveLength(2);
  });

  test('appends the table to a one-paragraph description', () => {
    expect(rewriteBashDescription('Runs a command.')).toBe('Runs a command.\n\n' + LETS_TABLE);
  });

  test('leaves a description that already carries the table as it is', () => {
    const once = rewriteBashDescription(LEAN_BASH_DESCRIPTION);

    expect(rewriteBashDescription(once)).toBe(once);
  });

  test('answers the same input with the same bytes, so the prompt cache holds', () => {
    expect(rewriteBashDescription(BASH_DESCRIPTION)).toBe(rewriteBashDescription(BASH_DESCRIPTION));
  });
});

describe('LETS_TABLE', () => {
  test('tells the model to use --head N rather than pipe lets to head', () => {
    expect(LETS_TABLE.startsWith('# File work: use `lets` through Bash\n')).toBe(true);
    expect(LETS_TABLE).toContain('pass `--head N` to `lets show` or `lets find`');
    expect(LETS_TABLE).not.toContain('Do not pipe');
  });
});

describe('dropPreferDedicatedTools', () => {
  test('drops the prefer-dedicated-tools line and keeps the others', () => {
    expect(dropPreferDedicatedTools(TOOLS_SECTION)).toBe(
      '# Using your tools\n - You can call multiple tools in a single response.',
    );
  });

  test('returns text without the line unchanged', () => {
    const without = '# Using your tools\n - You can call multiple tools in a single response.';

    expect(dropPreferDedicatedTools(without)).toBe(without);
  });

  test('drops the lean sentence and keeps the rest of its line', () => {
    expect(dropPreferDedicatedTools(LEAN_HARNESS)).toBe(
      '# Harness\n - Text you output outside of tool use is displayed to the user.\n - Independent tool calls can run in parallel in one response.\n - Reference code as `file_path:line_number`.',
    );
  });

  test('drops a lean line that holds only the sentence', () => {
    const alone = '# Harness\n - Prefer the dedicated file/search tools over shell commands when one fits.\n - Next.';

    expect(dropPreferDedicatedTools(alone)).toBe('# Harness\n - Next.');
  });

  test('keeps a line that only mentions the phrase mid-line', () => {
    const quoted = '# Notes\n - Someone wrote " - Prefer dedicated tools over Bash" here.\n';

    expect(dropPreferDedicatedTools(quoted)).toBe(quoted);
  });
});

const STRICT_STEER =
  'Do your work through the Bash tool wherever it can accomplish the job: read files with cat, head, or sed -n, search with grep and find, and make file changes with sed, heredocs, or short scripts, rather than using the dedicated Read, Edit, or Write tools. Fall back to a dedicated tool only when Bash genuinely cannot do the job.';
const RELAXED_STEER =
  'You can do much of your work through the Bash tool when it is the simpler route: read files with cat, head, or sed -n, search with grep and find, and make small, mechanical file changes with sed, heredocs, or short scripts instead of the dedicated Read, Edit, or Write tools. The choice is yours: prefer Edit or Write when a shell edit would be fragile, such as exact or multi-line replacements, or sed/awk flags that differ between GNU and BSD/macOS.';

describe('rewriteBashFirstSteer', () => {
  test('points the strict bypass steer at lets and keeps the rest of it', () => {
    expect(rewriteBashFirstSteer(`While bypass permissions mode is active:\n\n${STRICT_STEER}`)).toBe(
      'While bypass permissions mode is active:\n\nDo your work through the Bash tool wherever it can accomplish the job: read, search and edit files with `lets` (the table in its description), rather than using the dedicated Read, Edit, or Write tools. Fall back to a dedicated tool only when Bash genuinely cannot do the job.',
    );
  });

  test('points the relaxed steer at lets and keeps its choice sentence', () => {
    const rewritten = rewriteBashFirstSteer(RELAXED_STEER);

    expect(rewritten).toBe(
      'You can do much of your work through the Bash tool when it is the simpler route: read, search and edit files with `lets` (the table in its description) instead of the dedicated Read, Edit, or Write tools. The choice is yours: prefer Edit or Write when a shell edit would be fragile, such as exact or multi-line replacements, or sed/awk flags that differ between GNU and BSD/macOS.',
    );
    expect(rewritten).not.toContain('sed -n');
  });

  test('leaves text without the clause byte-identical', () => {
    const other = 'Bias toward working without stopping for clarifying questions. Read files with care.';

    expect(rewriteBashFirstSteer(other)).toBe(other);
  });
});

const deny = (reason: string) =>
  JSON.stringify({
    hookSpecificOutput: {
      hookEventName: 'PreToolUse',
      permissionDecision: 'deny',
      permissionDecisionReason: reason,
    },
  });

describe('decide', () => {
  test('empty or whitespace-only stdout is a pass', () => {
    expect(decide('')).toEqual({ kind: 'pass' });
    expect(decide(' \n')).toEqual({ kind: 'pass' });
  });

  test('a rewrite returns the new command', () => {
    const stdout = JSON.stringify({
      hookSpecificOutput: {
        hookEventName: 'PreToolUse',
        updatedInput: { command: 'lets show src/a.ts --all', description: 'Read a', timeout: 5000 },
      },
    });

    expect(decide(stdout)).toEqual({ kind: 'rewrite', command: 'lets show src/a.ts --all' });
  });

  test('a rewrite to an empty command is a pass', () => {
    const stdout = JSON.stringify({ hookSpecificOutput: { hookEventName: 'PreToolUse', updatedInput: { command: '' } } });

    expect(decide(stdout)).toEqual({ kind: 'pass' });
  });

  test('a deny with one run line is a replacement whose note names the command and the first reason line', () => {
    const stdout = deny(
      "lets edit replaces exact text and checks the result.\nrun: lets edit README.md --old 'cap' --new 'limit' --all",
    );

    expect(decide(stdout)).toEqual({
      kind: 'replace',
      command: "lets edit README.md --old 'cap' --new 'limit' --all",
      note: "lets ran `lets edit README.md --old 'cap' --new 'limit' --all` in place of the command you wrote: lets edit replaces exact text and checks the result.",
    });
  });

  test('a deny with two run lines is a pass', () => {
    const stdout = deny('lets show reads several files and ranges in one call.\nrun: lets show a.ts\nrun: lets show b.ts');

    expect(decide(stdout)).toEqual({ kind: 'pass' });
  });

  test('a deny with no run line is a pass', () => {
    expect(decide(deny('lets show reads several files and ranges in one call.'))).toEqual({ kind: 'pass' });
  });

  test('output that is not JSON is a pass', () => {
    expect(decide('not json')).toEqual({ kind: 'pass' });
  });

  test('JSON that is neither a rewrite nor a deny is a pass', () => {
    expect(decide('{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}')).toEqual({
      kind: 'pass',
    });
    expect(decide('null')).toEqual({ kind: 'pass' });
  });
});
