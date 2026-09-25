# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Astro static site, Bun, GitHub Pages at lets.mayberuk.com. Google Fonts is the only external request.

## Users

Engineers who run coding agents (Claude Code, Codex) every day and pay for their turns and
tokens; and the agents themselves, which read the docs through /llms.txt.

## Product Purpose

`lets` is a file-operations CLI for coding agents (Locate, Edit, Transform, Show). One shell call
reads, searches, edits or transforms several files and returns bounded, numbered output whose
footer names everything left out, so the agent needs no follow-up read. The site's job: an
engineer understands in seconds what changes for their agent, trusts it, and installs it.

## Positioning

It meets agents in the shell where they already work, and its output is the verification: an
edit returns the changed lines plus a syntax check, and a footer that never hides a cut.

## Capabilities and Constraints

show, find, edit (with `--from -` batches, structural checks, reverts), transform (JSON, YAML,
TOML, frontmatter), write, `hooks install claude-code|codex`. Linux and macOS. MIT or
Apache-2.0. Version 0.0.1. `--from` accepts only `-`.

## Brand Commitments

Honesty is the brand: no overclaiming. Plain, specific copy, no hype words. The user's pinned
visual direction for the site: a warm, tactile Linux desktop, between a modern skeuomorphic
desktop and the 2010-era GNOME 2 look, in the spirit of posthog.com's late-2025 desktop site;
readable type (no pixel fonts); slightly rounded corners; the parts-catalog direction's headline
("One call instead of three.") and hover-responsive labelled output drawing.

## Evidence on Hand

Real output: /home/biruk/dev/lets/lets-explainer.md, docs/examples/*.md. Corpus study and trial
numbers with caveats: /home/biruk/dev/lets-site/_brief/brief.md "Claims". No testimonials,
customers, logos, star counts or user counts exist; never fabricate them.

## Product Principles

- The output is the proof: show real output, never describe it instead.
- Nothing hidden: every number carries its caveat, every trimmed sample says it was trimmed.
- Install is the primary action on every first screen.
- Agents are readers too: every word is in the HTML, and markdown twins exist.
