# Self-Evolution (`/evolve`)

A design for conga's self-evolution capability: distill reusable
**experience** and **skills** from session transcripts on demand, and
inject relevant experience into every turn.

Informed by: ReasoningBank (extract→consolidate→retrieve loop), Voyager
(verified admission into a skill library), Anthropic Agent Skills
(filesystem skill packs with frontmatter + progressive disclosure),
Letta/MemGPT (self-editable memory), EvoAgentX (evaluation-gated
evolution), Darwin Gödel Machine / AlphaEvolve (archive + rollback as
the safety net — here distilled to: human-readable files, provenance
keys, and `git init ~/.conga` if you want history).

One sentence: distill sessions into tagged markdown notes, put the
relevant ones into the prompt, gate every write behind a human.

---

## What evolves (and what does not)

Evolves:

1. **Experience memory** — "next time this happens, do X" insights
   extracted from full session trajectories.
2. **Skills** — repeatable procedures distilled into the existing
   skills directory format.

Does **not** evolve (explicit non-goals):

- `systemPrompt` / hooks config / `CODING_AGENT_PROMPT` (offline prompt
  optimization needs an evaluator conga does not have).
- conga's own source code (worst risk/benefit for a personal assistant).
- No embedding store, no FTS5 memory index (in-process keyword scoring
  suffices at the cap below; the injection interface stays stable if the
  library outgrows it).
- No per-turn online extraction, no timers, no file watchers, no
  per-turn bookkeeping (the read path writes zero bytes).

---

## Storage

### Experience: `~/.conga/memory/*.md` (single root)

Same shape as skills (frontmatter + body), so one scanner style serves
both. No project-level root: nobody asked for project-scoped
experience, and a second root costs a second scanner, override rules,
and double the tests. Add it the day someone needs it.

```markdown
---
title: cargo-workspace-cyclic-dependency
tags: [rust, cargo, build-error]
created: 2026-08-25T10:00:00Z
source_session: <session-id>
---
On "cyclic package dependency": check workspace members' path refs first,
then [patch] sections; root cause here was conga-ext -> conga-host.
```

Frontmatter is exactly: `title`, `tags`, `created`, `source_session`.
No counters, no usage tracking. Hard limits: **64 entries**, body
≤ 2KB per entry. Small enough to load fully in-process and score per
turn — no index, no vector store.

Audit and rollback are the files themselves: every entry carries its
`source_session`, everything is human-readable and human-editable,
deleting a file reverts it. No separate log file — one fact, one place.

### Skills: existing `skills/*.md`, plus provenance

Two extra frontmatter keys on evolved skills: `provenance: evolve` and
`source_session: <id>`. Skills **without** those keys are user-authored;
evolve never overwrites user-authored skills. Existing skill parsing
only reads `name:`/`description:` — extra keys are transparent.

---

## Write path: `/evolve` (explicit, expensive)

One entry point, `Host::evolve(session_id)`, reached two ways:

- CLI: `/evolve [--session <id>]` in `handle_slash` — direct call, does
  not spend a main-model turn.
- Tool: built-in `evolve` tool (risk = High) for gateway/desktop —
  the permission matrix already gates High risk (blocked in
  Suggest/Plan, approved in AutoEdit, free in FullAuto); no new
  interaction concepts.

Pipeline (4 steps, no consolidation pass):

1. **Load trajectory** — project the session's `events.jsonl` to a
   conversation + tool-call summary. Skip segments dropped by
   `Compacted`; if overlong, truncate oldest segments and say so in the
   input.
2. **Extract** — one sub-agent via `HostSubagentSpawner` (inherits the
   fast-model routing). Input includes the current memory/skills
   catalog (titles + tags/descriptions) so it proposes deltas, not
   echoes. Structured output:
   ```json
   { "insights":  [{"title","tags","content"}],
     "skills":    [{"name","description","body"}],
     "retires":   ["existing titles made obsolete by the above"],
     "duplicates": ["catalog entries the above would duplicate"] }
   ```
3. **Admit** — each candidate (adds and retires alike) passes through
   the existing `Approver` closure (`approver("evolve_write",
   {title, content})`): CLI prints y/N, gateway does a WS round-trip,
   desktop does IPC. Rejected = dropped, no partial writes.
   The **cap is an admission check**, not a background job: with the
   library at 64, an add is admitted only if a same-run retire freed a
   slot.
4. **Persist** — insights to `~/.conga/memory/`, skills to the skills
   dir, approved retires delete their files. One pass, atomic per file.

The extraction output is **never executed**: insights and skills are
prompt text only (deliberately one notch more conservative than
Voyager's executable skill library).

### Content quality (top implementation priority)

The architectural risk here is near zero (prompt text only); the real
risk is **garbage insights**. An extractor left to its own devices
produces "when the build fails, read the error carefully" — and once
that pollutes the library, every keyword hit injects it into the
prompt. Two defenses, both mandatory:

- The extraction prompt must require each insight to carry
  **root cause + the fix actually applied + evidence from the
  trajectory**. No general advice, only what this session proved.
- The approver prompt shows title, tags, and the full body — enough to
  judge specificity in one glance. If you cannot tell what situation
  an insight applies to, reject it.

---

## Read path: per-turn injection (automatic, cheap, read-only)

Seam: `run_turn` already rebuilds the base prompt every turn
(`with_custom_base_prompt` at the settings re-read, `lib.rs`). Move the
one-shot skills append out of `assemble_host` into that seam, then:

```
base_prompt -> settings systemPrompt -> append_skills(...) -> append_memory(...)
```

`append_memory(base, last_user_message)` — reads, never writes:

- Score every entry: keyword hits of `tags`/`title` against the last
  User message (tokenized substring match, in-process).
- Top 3 by score; ties broken by file mtime (most recently written
  first). Total injection capped at 1.5KB.
- Skills are rescanned at the same seam — an evolved skill enters the
  catalog the very next turn.

Degradation: missing/empty memory dir → silent no-op; an unparsable
entry → warn + skip (same semantics as `skills::scan_dir`).

---

## Security boundaries

- Evolved content is prompt text only; no new code-execution surface.
- Prompt-injection in trajectories (e.g. a poisoned web page) can at
  worst pollute a **candidate** — still behind the human Approver and
  the 1.5KB injection cap. Blast radius bounded.
- `evolve` tool is High risk: Suggest/Plan cannot trigger it at all.
- Multiple concurrent sessions share the library through plain files;
  the read path is stateless and read-only, and the write path runs
  only under human approval — last writer wins, no counters to race on.

---

## Verification

1. Unit (pure functions, `skills.rs` test style): frontmatter parse;
   scoring + top-3 + mtime tie-break; 1.5KB truncation; cap admission
   check (add at cap rejected / freed slot admitted); `Compacted`
   segment filtering.
2. Integration (tempdir fake home): run `/evolve` with a stubbed
   extraction sub-agent returning fixed JSON → assert memory/skills on
   disk, retires deleted, no log sidecar; next turn asserts the
   injected prompt section and that the read path wrote nothing.
3. Smoke (real CLI): create a "stumble → correct" trajectory, `/evolve`,
   observe the approval flow and a hit injection in a fresh session;
   confirm the produced insight names the root cause, not a platitude.

---

## Change surface

| File | Change |
|---|---|
| `conga-host/src/memory.rs` (new) | entry format, scanner, scorer, read-only injection append |
| `conga-host/src/evolve.rs` (new) | `Host::evolve` pipeline, admission (incl. cap check), extraction prompt |
| `conga-host/src/lib.rs` | per-turn seam: skills rescan + `append_memory` |
| `conga-host/src/assembly.rs` | move one-shot skills append to the seam |
| `conga-host/src/tools/mod.rs` | register `evolve` tool (High risk) |
| `conga-cli/src/main.rs` | `/evolve [--session <id>]` |
| `docs/` + `.env.example` | document commands, formats, limits |
