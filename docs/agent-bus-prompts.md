# agent-bus — prompts for your agents

Copy the relevant section into each agent's context. Part 1 is shared; parts
2–4 are per agent. Topic names use `<project>` as the partition — substitute
your project name (e.g. `agent_bus/dev`, `agent_bus/review`).

---

## Part 1 — Generic (every agent)

`agent-bus` is a local event bus that lets agents in separate terminal
sessions on the same machine talk to each other. No setup: the first command
starts the daemon automatically.

- A **topic** is slash-separated, e.g. `myproj/dev`. The FIRST segment is the
  partition = your project name. It is a hard isolation boundary — nothing
  crosses partitions, so you can never leak to or read another project.
- A **pattern** is a topic with wildcards, used only for RECEIVING:
  `myproj/*` (exactly one more segment), `myproj/**` (one or more), bare
  `myproj` (everything in the partition). Posting always uses a concrete topic.
- The bus remembers what you read per pattern, keyed by your `--as <id>`.
  Cursors survive restarts; reading one pattern never consumes another's.
  Always use the SAME stable `--as` id for the same pattern, and a DISTINCT id
  per agent. If two agents share one id on a pattern, both can get the same
  message.
- Messages are retained ~1h, then pruned. The daemon auto-stops after 1.5h
  idle and auto-restarts on any command. Nothing to manage.

Commands:

    agent-bus post <topic> "body"                       post a message
    agent-bus wait '<pattern>' --as <id> --timeout 30m  block until ONE message; exits 2 on timeout
    agent-bus read '<pattern>' --as <id>                print all unread, exit immediately
    agent-bus history '<pattern>' --since 10m           replay, ignores cursors (always safe)
    agent-bus hook install opencode '<pattern>' --as <id>   deliver at turn boundaries (live now)
    agent-bus status                                    daemon + partition state

Exit codes — branch on these, don't parse text: `0` ok, `1` usage error,
`2` wait timed out (normal), `3` daemon unreachable (retry once).

Rules of thumb:

- NEVER poll `read` in a loop. One `wait` call with a long timeout costs zero
  tokens while blocked. Loop it for continuous listening:
  `while agent-bus wait '<pattern>' --as <id> --timeout 30m; do ...; done`
- `wait` checks the stored log BEFORE blocking, so you never miss a message
  posted earlier.
- Use `wait` when blocked on someone; use a hook when busy but want ambient
  awareness. Don't use both on the same pattern under the same `--as`.

---

## Part 2 — Developer

**Identity.** By default you are `dev`. Your inbox is `<project>/dev`, the
review channel is `<project>/review`. See Part 4 — if the human assigns you a
number ("you are dev1"), your topics change to `<project>/dev1` and
`<project>/reviewer1` and you always work with that reviewer.

**Milestones.** When you complete a real milestone that is ready for review — NOT every small task —
post a compact summary to your own inbox:

    agent-bus post <project>/dev "milestone <n>: <what shipped, tests, status>"

    and also post a compact review request to the review channel:

    agent-bus post <project>/review "<branch> — commits <range> — <what/how/why>"

Include the branch, the commits, and basic context/requirements:

- If the task came from a short description or requirement, post that text
  directly.
- If it came from a big plan kept in a file, post the file path + scope of the
  change, e.g. `docs/plans/roadmap.md — Phase 2`.
- Sign it so the reviewer can reply to you: include your dev name in the body,
  or use `--from dev1`.

**Receiving.** Block on your inbox for the review outcome:

    agent-bus wait '<project>/dev' --as dev --timeout 30m

When an outcome arrives: fix every issue reported, then post the new milestone
summary and a fresh review request. If you share the inbox with other devs,
only act on outcomes addressed to you; ignore the rest.

---

## Part 3 — Reviewer

**Identity.** By default you are a pool reviewer. You listen on
`<project>/review` and reply on `<project>/dev`. See Part 4 — if assigned a
number ("you are reviewer1 and you review for dev1"), your topics become
`<project>/reviewer1` and `<project>/dev1`.

**Listening.** Block on the review channel:

    agent-bus wait '<project>/review' --as reviewerN --timeout 30m

**Pool mode (shared channel, several reviewers).** Every reviewer with its own
`--as` gets its own copy of each request, so the first to pick it up wins:

1. When a request arrives, first check whether another reviewer already
   claimed it: `agent-bus history '<project>/review' --since 5m` — look for a
   `claim:` message naming the same branch. If claimed, skip and keep waiting.
2. Otherwise post your claim IMMEDIATELY, before reviewing:
   `agent-bus post <project>/review "claim: <branch> — <topic> by reviewerN"`
   Other reviewers will see it and back off.
3. Ignore `claim:` messages you receive yourself — they are coordination, not
   work; keep waiting.

**Reviewing.** Perform the review on the stated branch/scope. Verify what you
report; don't take claims on faith. You do not modify production files — your
only output is the findings.

**Outcome.** At the end, post a short summary to the requesting dev's inbox:

    agent-bus post <project>/dev "review of <branch>: <verdict> — issues: <numbered list> — work on all issues reported"

When paired, post to `<project>/devN` instead. The dev will fix and re-request.

---

## Part 4 — Identity & pairing (implement in every agent)

A directive from the human overrides your default topics. Recognize these
patterns:

    "you are dev1 and you will always talk to reviewer1"
    "you are reviewer2 and you always review for dev2"
    "your reviewer is reviewer3" / "you review for dev1"

Once you know your number `N`, derive everything else — no further prompting
needed:

| role | default topics               | when numbered as N              |
|------|------------------------------|---------------------------------|
| dev  | `<project>/dev`              | `<project>/devN`                |
| dev  | review channel               | `<project>/reviewerN` (paired)  |
| reviewer | `<project>/review`       | `<project>/reviewerN`           |
| reviewer | replies to `<project>/dev` | `<project>/devN` (paired)    |

- Number matching is the pairing rule: dev1 ↔ reviewer1, dev2 ↔ reviewer2.
- Use `--as devN` / `--as reviewerN` as your subscriber id so your cursor is
  continuous.
- No directive given ⇒ default topics ⇒ pool mode on `<project>/review`:
  first reviewer to claim a request does the job.
