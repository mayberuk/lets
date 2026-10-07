export const VERSION = "0.0.3";
export const LETS_TABLE = "# File work: use `lets` through Bash\n\n| Instead of | Run |\n|---|---|\n| several `cat`/`sed -n`/`grep` calls, Read | `lets show a.ts b.ts:10-40 c.ts#computeFee` |\n| `sed -i 's/a/b/'`, Edit | `lets edit f.ts --old a --new b` |\n| edit JSON/YAML/TOML | `lets transform f.json --set version=1.4.0` |\n| `cat > new.ts <<'EOF'` | `lets write new.ts <<'EOF'` |\n\nSeveral edits and the build in one call; each `old` is exact text that occurs once:\n\n```\nlets edit --from - --check @auto <<'LETS'\n@@ a.ts\n<<<<<<< old\ncap = 10\n======= new\ncap = 20\n>>>>>>>\n<<<<<<< old\nfloor = 1\n======= new\nfloor = 2\n>>>>>>>\nLETS\n```\n\nFor the first lines only, pass `--head N` to `lets show` or `lets find` instead of piping to `head`: the footer still names the cut. Do not add `2>/dev/null`: it hides the fix. Keep Read for images and PDFs; use plain Bash for anything else that is not reading, searching or editing files.";

const BASH_STEER_START = 'IMPORTANT: Avoid using this tool to run `cat`';
const BASH_STEER_END = 'give permission.';
const PREFER_DEDICATED_TOOLS = /^ - Prefer dedicated tools over Bash.*(?:\n|$)/m;

/** A description without Claude Code 2.1.292's "use Read, not cat" paragraph comes back as given. */
export function rewriteBashDescription(description: string): string {
  const start = description.indexOf(BASH_STEER_START);
  if (start === -1) return description;
  const endText = description.indexOf(BASH_STEER_END, start);
  if (endText === -1) return description;
  let end = endText + BASH_STEER_END.length;
  if (description[end] === '\n') end += 1;
  return description.slice(0, start) + LETS_TABLE + '\n\n' + description.slice(end);
}

export function dropPreferDedicatedTools(text: string): string {
  return text.replace(PREFER_DEDICATED_TOOLS, '');
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
  const lines = reason.split('\n');
  const runs = lines.filter((line) => line.startsWith(RUN_PREFIX));
  if (runs.length !== 1) return PASS;
  const replacement = runs[0]!.slice(RUN_PREFIX.length);
  return {
    kind: 'replace',
    command: replacement,
    note: `lets ran \`${replacement}\` in place of the command you wrote: ${lines[0]}`,
  };
}

function field(value: unknown, key: string): unknown {
  return typeof value === 'object' && value !== null ? (value as Record<string, unknown>)[key] : undefined;
}
