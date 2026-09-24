export const faq: { q: string; a: string }[] = [
  {
    q: 'What is lets?',
    a: 'lets is a file-operations CLI for coding agents: Locate, Edit, Transform, Show. One shell call reads, searches, edits or transforms several files and returns bounded, numbered output whose last line names everything it left out.',
  },
  {
    q: 'Which agents does it work with, and how does it reach them?',
    a: "Claude Code and Codex. lets hooks install claude-code (or codex) wires three things: a SessionStart note explaining lets at the start of every session, a SubagentStart note that delivers the same explanation to subagents, and a PreToolUse hook that blocks a bare cat, sed -n or grep and hands back the matching lets command. Nothing is appended to the agent's system prompt, and if lets is missing or crashes, the hook lets the command through.",
  },
  {
    q: 'What does "one call instead of three" mean?',
    a: "Today an agent often reads a file, greps for a line, then re-reads it to confirm an edit landed — three shell calls. lets show, find and edit each return the full answer in one call, and edit includes the changed lines and a syntax check, so there's no follow-up read.",
  },
  {
    q: 'What happens when an edit would break the file?',
    a: 'lets edit checks the result — a structural parse, a JSON/YAML/TOML/frontmatter check, or a command passed with --check — before it keeps the change. If the check fails, the edit is reverted, the file stays unchanged, and lets exits 3 (check_failed).',
  },
  {
    q: 'What did the trial measure?',
    a: 'Five tasks on a private 17.7k-file Go monorepo, a stock Claude Code session against one with lets hooks install, nothing else changed. On Sonnet 5: 16% fewer tool calls, 11% faster, 99.1% vs 97.8% checks passed, cost-neutral (within 0.5%). On Opus 5.5: 16% fewer calls, 12% faster, 100% checks passed both, cost 8% higher on average. The samples are small — 3 to 9 sessions per arm.',
  },
  {
    q: 'What platforms and licence?',
    a: 'Linux and macOS, as one static binary. MIT or Apache-2.0, your choice. Version 0.0.1.',
  },
];
