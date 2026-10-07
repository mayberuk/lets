export const VERSION = "0.0.4";
export const LETS_TABLE = "# File work: use `lets` through Bash\n\n| Instead of | Run |\n|---|---|\n| several `cat`/`sed -n`/`grep` calls, Read | `lets show a.ts b.ts:10-40 c.ts#computeFee` |\n| `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |\n| edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |\n| `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |\n\nSeveral edits and the build in one call; each `old` is exact text that occurs once:\n\n```\nlets edit --from - --check @auto <<'LETS'\n@@ a.ts\n<<<<<<< old\ncap = 10\n======= new\ncap = 20\n>>>>>>>\n<<<<<<< old\nfloor = 1\n======= new\nfloor = 2\n>>>>>>>\nLETS\n```\n\nFor the first lines only, pass `--head N` to `lets show` or `lets find` instead of piping to `head`: the footer still names the cut. Do not add `2>/dev/null`: it hides the fix. Keep Read for images and PDFs; use plain Bash for anything else that is not reading, searching or editing files.";

const BASH_STEER_START = 'IMPORTANT: Avoid using this tool to run `cat`';
const BASH_STEER_END = 'give permission.';
const PREFER_DEDICATED_TOOLS = /^ - Prefer dedicated tools over Bash.*(?:\n|$)/m;
const LEAN_PREFER_DEDICATED_TOOLS = /^( - )Prefer the dedicated file\/search tools over shell commands when one fits\.(?: (.*))?(?:\n|$)/m;

/**
 * Puts the lets table in place of Claude Code's "use Read, not cat" paragraph, or, in a description
 * without it (the lean prompt has none), after the first paragraph, so the table always lands.
 */
export function rewriteBashDescription(description: string): string {
  if (description.includes(LETS_TABLE)) return description;
  const start = description.indexOf(BASH_STEER_START);
  const endText = start === -1 ? -1 : description.indexOf(BASH_STEER_END, start);
  if (endText !== -1) {
    let end = endText + BASH_STEER_END.length;
    if (description[end] === '\n') end += 1;
    return description.slice(0, start) + LETS_TABLE + '\n\n' + description.slice(end);
  }
  const firstBreak = description.indexOf('\n\n');
  if (firstBreak === -1) return description + '\n\n' + LETS_TABLE;
  const at = firstBreak + 2;
  return description.slice(0, at) + LETS_TABLE + '\n\n' + description.slice(at);
}

/** Drops the full prompt's prefer-dedicated-tools line, and the lean prompt's sentence while keeping the rest of its line. */
export function dropPreferDedicatedTools(text: string): string {
  return text
    .replace(PREFER_DEDICATED_TOOLS, '')
    .replace(LEAN_PREFER_DEDICATED_TOOLS, (line: string, bullet: string, rest: string | undefined) =>
      rest === undefined || rest === '' ? '' : `${bullet}${rest}${line.endsWith('\n') ? '\n' : ''}`,
    );
}

export type Decision =
  | { kind: 'pass' }
  | { kind: 'rewrite'; command: string }
  | { kind: 'replace'; command: string; note: string };

const PASS: Decision = { kind: 'pass' };
const RUN_PREFIX = 'run: ';

/** Reads `lets hook classify` stdout. A deny with several `run: ` lines names separate replacements, not one equivalent command, so it passes. */
export function decide(stdout: string): Decision {
  if (stdout.trim() === '') return PASS;
  let parsed: unknown;
  try {
    parsed = JSON.parse(stdout);
  } catch {
    return PASS;
  }
  const output = field(parsed, 'hookSpecificOutput');
  const command = field(field(output, 'updatedInput'), 'command');
  if (typeof command === 'string' && command !== '') return { kind: 'rewrite', command };
  const reason = field(output, 'permissionDecisionReason');
  if (field(output, 'permissionDecision') !== 'deny' || typeof reason !== 'string') return PASS;
  const replacement = singleRun(reason);
  if (replacement === undefined) return PASS;
  return {
    kind: 'replace',
    command: replacement,
    note: `lets ran \`${replacement}\` in place of the command you wrote: ${reason.split('\n')[0]}`,
  };
}

export function singleRun(reason: string): string | undefined {
  const runs = reason.split('\n').filter((line) => line.startsWith(RUN_PREFIX));
  return runs.length === 1 ? runs[0]!.slice(RUN_PREFIX.length) : undefined;
}

function field(value: unknown, key: string): unknown {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>)[key] : undefined;
}
