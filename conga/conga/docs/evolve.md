# Self-Evolution (`/evolve`)

A design for conga's self-evolution capability: distill reusable
**experience** and **skills** from session transcripts on demand, and
inject relevant experience into every turn.

Informed by: ReasoningBank (extract→consolidate→retrieve loop), Voyager
(verified admission into a skill library), Anthropic Agent Skills
(filesystem skill packs with frontmatter + progressive disclosure),
Letta/MemGPT (self-editable memory), EvoAgentX (evaluation-gated
evolution), Darwin Gödel Machine / AlphaEvolve (archive + rollback as
the safety net). Scope decisions specific to conga follow.

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
- No per-turn online extraction, no timers, no file watchers.

---

## Storage

### Experience: `~/.conga/memory/*.md`

Same shape as skills (frontmatter + body), so one scanner serves both.
Project-level experience may live at `<project>/.conga/memory/*.md`;
same override semantics as skills (project wins on equal title).

```markdown
---
title: cargo-workspace-cyclic-dependency
tags: [rust, cargo, build-error]
created: 2026-08-25T10:00:00Z
source_session: <session-id>
uses: 0
last_used: 2026-08-25T10:00:00Z
---
On "cyclic package dependency": check workspace members' path refs first,
then [patch] sections; root cause here was conga-ext -> conga-host.
```

Hard limits: **64 entries** (consolidation triggers above), body
≤ 2KB per entry. Small enough to load fully in-process and score per
turn — no index, no vector store.

### Skills: existing `skills/*.md`, plus provenance

Two extra frontmatter keys on evolved skills: `provenance: evolve` and
`source_session: <id>`. Skills **without** those keys are user-authored;
evolve never overwrites user-authored skills.

---

## Write path: `/evolve` (explicit, expensive)

One entry point, `Host::evolve(session_id)`, reached two ways:

- CLI: `/evolve [--session <id>]` in `handle_slash` — direct call, does
  not spend a main-model turn.
- Tool: built-in `evolve` tool (risk = High) for gateway/desktop —
  permission matrix already gates High risk (blocked in Suggest/Plan,
  approved in AutoEdit, free in FullAuto); no new interaction concepts.

Pipeline:

1. **Load trajectory** — project the session's `events.jsonl` to a
   conversation + tool-call summary. Skip segments dropped by
   `Compacted`; if overlong, truncate oldest segments and say so in the
   input.
2. **Extract** — one sub-agent via `HostSubagentSpawner` (inherits the
   fast-model routing). Input includes the current memory/skills
   catalog (titles + descriptions) so it produces deltas, not echoes.
   Structured output:
   ```json
   { "insights": [{"title","tags","content"}],
     "skills":   [{"name","description","body"}],
     "duplicates": ["titles that duplicate existing entries"] }
   ```
3. **Admit** — each candidate passes through the existing `Approver`
   closure (`approver("evolve_write", {title, content})`): CLI prints
   y/N, gateway does a WS round-trip, desktop does IPC. Rejected =
   dropped, no partial writes.
4. **Persist** — insights to `~/.conga/memory/`, skills to the skills
   dirs. Append one JSONL line per run to `~/.conga/evolve.log`
   (timestamp, session, added/merged/retired entries).
5. **Consolidate** (when library > 64) — same sub-agent proposes merges
   and a retirement list for long-unused (`uses == 0`) entries;
   retirement also goes through the Approver.

The extraction output is **never executed**: insights and skills are
prompt text only (deliberately one notch more conservative than
Voyager's executable skill library).

**Rollback** = the files themselves plus `evolve.log`: everything is
human-readable, human-editable; deleting a file reverts it. This is the
minimal viable form of DGM's archive.

---

## Read path: per-turn injection (automatic, cheap)

Seam: `run_turn` already rebuilds the base prompt every turn
(`with_custom_base_prompt` at the settings re-read, `lib.rs`). Move the
one-shot skills append out of `assemble_host` into that seam, then:

```
base_prompt -> settings systemPrompt -> append_skills(...) -> append_memory(...)
```

`append_memory(base, cwd, last_user_message)`:

- Score every entry: keyword hits of `tags`/`title` against the last
  User message (tokenized substring match, in-process).
- Top 3 by score weighted by `uses`; total injection capped at 1.5KB.
- A hit bumps `uses` and `last_used` (once per entry per session).
- Skills are rescanned at the same seam — an evolved skill enters the
  catalog the very next turn.

Degradation: missing/empty memory dir → silent no-op; an unparsable
entry → warn + skip (same semantics as `skills::scan_dir`).

---

## Security boundaries

- Evolved content is prompt text only; no new code-execution surface.
- Prompt-injection in trajectories (e.g. a poisoned web page) can at
  worst pollute a **candidate** — still behind the human Approver, the
  1.5KB injection cap, and the 64-entry cap. Blast radius bounded.
- `evolve` tool is High risk: Suggest/Plan cannot trigger it at all.

---

## Verification

1. Unit (pure functions, `skills.rs` test style): frontmatter
   parse/override priority; scoring + top-3 truncation; 64-cap
   consolidation trigger; `Compacted` segment filtering; `uses`
   write-back debounce.
2. Integration (tempdir fake home): run `/evolve` with a stubbed
   extraction sub-agent returning fixed JSON → assert memory/skills on
   disk + `evolve.log`; next turn asserts the injected prompt section.
3. Smoke (real CLI): create a "stumble → correct" trajectory, `/evolve`,
   observe the approval flow and a hit injection in a fresh session.

---

## Change surface

| File | Change |
|---|---|
| `conga-host/src/memory.rs` (new) | entry format, scanner, scorer, injection append |
| `conga-host/src/evolve.rs` (new) | `Host::evolve` pipeline, admission, consolidation |
| `conga-host/src/lib.rs` | per-turn seam: skills rescan + `append_memory` |
| `conga-host/src/assembly.rs` | move one-shot skills append to the seam |
| `conga-host/src/tools/mod.rs` | register `evolve` tool (High risk) |
| `conga-cli/src/main.rs` | `/evolve [--session <id>]` |
| `docs/` + `.env.example` | document commands, formats, limits |
