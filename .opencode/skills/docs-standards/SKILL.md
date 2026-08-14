---
name: docs-standards
description: Use when writing, editing, organizing, reviewing, or cleaning up markdown in this repo — docs/**, docs/agents/**, AGENTS.md, skills, plan/review lifecycle files. Covers Diátaxis doc taxonomy, technical writing heuristics, token economy for agent-consumed docs, and the plan/review/ledger lifecycle. Load before editing any .md file outside of source code.
---

# Documentation standards for HomeTutor

The full sourced rules are in `docs/docs-standards-research.md`. **Read it before writing or editing any markdown in this repo.** Below is the working index.

## What the research doc covers

1. **Diátaxis taxonomy** — four modes (tutorial / how-to / reference / explanation); one mode per doc, split when mixed.
2. **Technical writing heuristics** — audience-first, active voice, defined terms, concrete numbers, lists/tables, headings, filler elimination, pronoun discipline.
3. **docs-as-code** — docs versioned and reviewed with code, freshness over completeness, permanent docs updated at merge time.
4. **Agent-consumed docs (token economy)** — terse imperative style, single source of truth, reference-not-copy, stable anchors, decision tables, mechanical checklists.
5. **Lifecycle docs** — `docs/plans/plan_<yyyy-mm>_<short-title>.md`, `docs/reviews/review_<yyyy-mm>_<scope>[_r<N>].md`, the status state machine (designed → in dev → review complete → merged to main → manually tested and approved), status in `docs/ledger.md` only, deletion as the end state.
6. **Anti-patterns** — copy-paste drift, status rot, orphans, graveyard directories, undated truth, over-indexing.

## House rules (this repo)

- Agent-consumed docs (`docs/agents/**`, `AGENTS.md`, skills): minimize token count; reference the single source of a block instead of repeating it; never paste the same content into two agent files.
- Plans: requirements at the top, locked; status header pointing at the ledger.
- Reviews: verdict + file:line findings; one file per round (`_r<N>` from round 2 on).
- Status changes go to `docs/ledger.md`, never only into a plan's prose.

## How to use this

- Before editing any `.md` file: read the relevant section of the research doc.
- When organizing: classify the doc by Diátaxis mode first, then apply naming and lifecycle rules.
- When the edit is to a doc an agent will load: apply token-economy rules; cut everything the agent doesn't need to act.
- After editing: check for duplicated blocks across files and replace with references; verify cross-file links still resolve.
