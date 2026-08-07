//! The `agent-bus guide` text.
//!
//! Written for an AI agent that has never seen the tool: it explains the model
//! and the decisions, not just the flags. `--help` already lists the flags.

/// The full usage guide, printed by `agent-bus guide`.
pub const GUIDE: &str = r#"
agent-bus — coordinate AI agents across separate terminal sessions
==================================================================

WHAT THIS IS

You are one agent in a terminal. Other agents are running in other terminals,
on the same machine, working on the same project. You cannot see their output
and they cannot see yours. agent-bus is the channel between you: you post a
message, they receive it, and vice versa.

The problem it solves is the handoff. A dev agent finishes a feature and needs
a reviewer to look at it. Without a bus, the human copy-pastes between windows.
With it, the dev agent posts "ready for review" and the reviewer agent — which
has been blocked and idle, costing nothing — wakes up and starts working.

There is no setup. The first command you run starts the daemon automatically.
There is nothing to install, configure, or shut down.


TOPICS AND PARTITIONS

A topic is a slash-separated name:

    iot_base/dev_01
    ^^^^^^^^ ^^^^^^
    partition  rest

The FIRST segment is the partition. It is a hard isolation boundary: a
subscription can never match a topic in a different partition, and there is no
wildcard that crosses one. Use your project name as the partition and you can
never accidentally receive another project's messages, or leak yours to it.

Conventional names within a partition:

    iot_base/dev_01            a specific agent's inbox
    iot_base/reviewer_01       another agent's inbox
    iot_base/bugs_found        a broadcast channel for defects
    iot_base/lessons_learned   a broadcast channel for durable knowledge

These are conventions, not rules. Any name works. Segments may not be empty or
contain whitespace.


WILDCARDS

Patterns are used for receiving (`wait`, `read`, `follow`, `history`). Posting
always uses a concrete topic with no wildcards.

    iot_base/*        exactly ONE more segment
                      matches iot_base/dev_01, not iot_base/dev_01/sub

    iot_base/**       ONE OR MORE segments
                      matches iot_base/dev_01 and iot_base/dev_01/sub

    iot_base          bare partition: the partition and everything under it

The first segment must always be a literal partition name. `*/dev_01` is
rejected — that is the isolation guarantee being enforced. Partial wildcards
like `dev_*` are not supported; a segment is either a literal, `*`, or `**`.


DELIVERY AND CURSORS

Delivery is decided at delivery time, and there are two modes. The default is
EXCLUSIVE per pattern: each pattern string has exactly one position, shared by
every consumer that reads with it. When the daemon hands a message out — via
`wait`, `read`, or `follow` — it marks that message delivered for the whole
pattern in the same instant. That message will not be returned again to the
same client, to another client using the same pattern, or to any overlapping
pattern. There is no separate acknowledgement round-trip, so there is no window
in which two consumers could both claim the same message.

The one pattern, one position model means a pool of workers sharing a pattern
splits the work between them: whichever worker gets the message first is the
only one that receives it, which is what makes "first to pick it up does the
job" true without any coordination.

Broadcast is the exception: a message posted with `--broadcast` is delivered to
every consumer (distinct `--as` label) whose pattern matches its topic, each
getting their own copy exactly once. Broadcast positions are keyed per label,
so a label that already received it never receives it again. See POSTING below.

Because positions belong to the pattern (or, for broadcast, the label), reading
one pattern never consumes another's messages: waiting on `iot_base/dev_01`
leaves anything on `iot_base/planner` untouched and unread. Each pattern has
its own position.

Your label defaults to the pattern you subscribe with, so this:

    agent-bus wait 'iot_base/**'

uses the label "iot_base/**" and needs no flag at all. `--as` is a label for
`status` output only. It does NOT create a second position, and passing
different labels does NOT give each consumer its own copy. Two agents that read
with the same pattern share one position and compete for each message, exactly
once each. If two agents must each see the same message, give each its own
pattern (its own inbox), not its own label.

The only way to see a delivered message again is `history`, which ignores
positions entirely and replays whatever is still retained.


THE TWO WAYS TO RECEIVE

There are exactly two, and they suit different situations.

1. BLOCKING WAIT — when your next action depends on someone else

    agent-bus wait 'iot_base/**' --as reviewer_01 --timeout 30m

This blocks until one message arrives, prints it, and exits. While blocked it
costs ZERO tokens: it is one tool call that returns once, not a loop. This is
the single most important thing to understand about the tool.

DO NOT POLL. Do not run `read` in a loop with sleeps, and do not call `wait`
repeatedly with a short timeout "to check". Call `wait` once with a long
timeout and let it block. Polling burns tokens for no benefit and is strictly
worse in every way.

`wait` checks the stored log BEFORE it blocks. You cannot miss a message by
starting late: if the other agent posted ten minutes before you ran `wait`, the
message is still there and comes back immediately. Subscribe-before-publish
ordering is not something you need to arrange.

`wait` returns AT MOST ONE message, so each returns one unit of work. On
timeout it prints nothing and exits 2, which makes this loop safe and
self-terminating:

    while agent-bus wait 'iot_base/**' --as reviewer_01 --timeout 30m; do
        # handle the message, then block again
    done

Because delivery is exclusive, this loop is also load-balanced across workers:
if three reviewers run the same loop on `iot_base/reviews`, each message wakes
exactly one of them.

2. HOOK DELIVERY — when you want to keep working and be told as you go

    agent-bus hook install opencode 'iot_base/**' --as dev_01     writes a file
    agent-bus hook install claude-code 'iot_base/**' --as dev_01  prints config

This wires your harness to drain unread messages at turn boundaries and inject
them into your context, so you receive things without ever blocking.

The two harnesses differ, and it matters. `opencode` writes its plugin and is
live immediately. `claude-code` only PRINTS a config block: settings.json is a
hand-edited file that may hold other tools' hooks, so a human has to paste it
in. Until they do, you will receive nothing this way. If you ran the
claude-code line yourself, do not assume delivery is active — say the config
needs pasting, and use `wait` meanwhile, which needs no setup.

Be clear about the guarantee: NEITHER Claude Code NOR OpenCode can interrupt an
agent in the middle of a tool call. Nothing will stop you mid-edit. The real
guarantee is "at the next turn boundary" — when you finish what you are doing
and would otherwise go idle. If you need to act on a message the moment it
lands, hooks are the wrong mechanism; use `wait`.

Use `wait` when you are blocked on someone. Use hooks when you are busy and
want ambient awareness. A hook and a `wait` on the SAME pattern compete for the
same messages: whichever runs first takes a message, and the other never sees
it. Do not run both on one pattern unless you want them to split the load.


POSTING

    agent-bus post iot_base/reviewer_01 "auth refactor ready for review"

The topic must be concrete. The body may be given as an argument, or piped:

    git diff | agent-bus post iot_base/reviewer_01

Flags:

    --broadcast   deliver this message to EVERY consumer whose pattern matches
                  the topic, each getting their own copy once. Without it,
                  delivery is exclusive per pattern — the first consumer to
                  read it takes it and nobody else sees it. Use --broadcast for
                  announcements every agent should see (e.g. a project-wide
                  finding); use the default for work handed to one agent (e.g.
                  a review request). A broadcast message is delivered once per
                  distinct `--as` label: two agents sharing a label are the
                  same consumer and only one gets it.

    --priority high   a delivery hint for the receiver. The bus does not act
                      on it and does not deliver it any faster. It travels with
                      the message: `"priority":"high"` in --json output, and a
                      leading `!` in human output. Acting on it is the
                      receiving agent's job. Use it for things that should
                      change what the other agent does next, not for
                      everything.

    --from NAME       sender name. Defaults to the topic's last segment, which
                      is usually right for inbox-style topics.


NON-BLOCKING READS

    agent-bus read 'iot_base/**' --as reviewer_01

Prints everything unread and exits immediately, printing nothing if there is
nothing. Use it to check in without committing to a block.

    agent-bus follow 'iot_base/**' --as reviewer_01

Streams messages continuously until interrupted. Useful for a human watching a
terminal; rarely what an agent wants, since it never returns.

`follow` CONSUMES what it streams: the pattern's position advances as messages
go out, so anything it prints is delivered and will never be returned by a
later `wait` or `read` on the same pattern. Do not run a `follow` on the same
pattern as a `wait` or `read` unless you intend the `follow` to take the
messages first.

    agent-bus history 'iot_base/**' --since 10m

Replays past messages and IGNORES CURSORS entirely — it does not consume
anything and does not move your position, so it is always safe to run. With no
`--since` it returns the entire retained window. This is how you rebuild
context after a restart, and the only way to see messages that were already
delivered.


RETENTION AND LIFECYCLE

Messages are retained for 1 hour, then pruned. The bus is for coordination, not
storage: anything that must outlive an hour belongs in a file or a commit.

The daemon shuts itself down after 1.5 hours with no activity, and any command
restarts it automatically. Cursors and message logs live on disk, so a restart
loses nothing that has not aged out. You never need to start or stop it by
hand, though `agent-bus stop` exists.


EXIT CODES

Branch on these. Do not parse the output text — it is for humans and may change.

    0    success
    1    usage error (bad pattern, bad topic, bad arguments)
    2    `wait` timed out with nothing delivered
    3    the daemon is unreachable and could not be started

Code 2 is the one that matters most: it is a normal, expected outcome, not a
failure. It is what lets a `while` loop around `wait` exit cleanly.

Code 3 means something is wrong with the environment rather than your command,
and is worth retrying once before giving up.


JSON OUTPUT

Pass `--json` to any command for machine-readable output. Messages are printed
one JSON object per line:

    {"id":"01J...","ts":"2026-01-01T12:00:00Z","topic":"iot_base/dev_01",
     "priority":"normal","from":"dev_01","body":"ready for review",
     "broadcast":false}

    id         ULID, sortable by creation time; also the cursor value
    ts         RFC 3339 timestamp
    topic      the full concrete topic
    priority   "normal" or "high"
    from       sender name
    body       the message text
    broadcast  true if this was posted with --broadcast

`status --json` emits the full report as a single pretty-printed object.


WORKED EXAMPLES

1. Dev hands off to a reviewer.

   The reviewer starts first and blocks. It costs nothing while waiting:

       agent-bus wait 'iot_base/reviewer_01' --as reviewer_01 --timeout 1h

   The dev agent finishes and posts:

       agent-bus post iot_base/reviewer_01 "auth refactor on branch feat/auth, \
       please review src/auth/*.rs"

   The reviewer's `wait` returns immediately with that message and exits 0. It
   reviews, then reports back to the dev agent's inbox:

       agent-bus post iot_base/dev_01 "two issues in token.rs, see comments" \
       --priority high

   Note that the reviewer did not have to be waiting when the dev posted. Had
   it started afterwards, the stored message would still be delivered.

2. Broadcasting a finding to everyone.

   You discover something every agent on the project needs to know. Post it
   with `--broadcast` to a shared topic:

       agent-bus post iot_base/bugs_found "the retry helper drops the last \
       error; anything relying on its message is wrong" --broadcast

   Every agent subscribed to `iot_base/**` or `iot_base` receives it, each with
   their own copy, so nobody consumes it away from anyone else. Without
   `--broadcast`, the first reader would take it and the rest would never know.
   Use `--broadcast` for `iot_base/lessons_learned` and `iot_base/bugs_found`,
   which are durable shared knowledge, not work for one agent.

3. Rebuilding context after a restart.

   Your session died and you have lost your working context. Two commands:

       agent-bus history 'iot_base/**' --since 1h
       agent-bus read 'iot_base/**' --as dev_01

   The first replays everything that happened in the last hour without
   consuming it, so you can see the full conversation including messages
   already delivered to you. The second drains what is genuinely still unread
   for you specifically. Run history first to orient, then read to catch up.


INSPECTING THE BUS

    agent-bus status

Shows the daemon pid and uptime, and for each partition the message count, the
age of the oldest message, and one line per subscriber cursor with its lag —
the number of matching messages it has not yet consumed. Cursors are per
(subscriber, pattern), so an id that reads two patterns appears once for each,
shown as `id [pattern] lag=N`. High lag on your own id means you have work
queued.

A subscriber flagged "missed messages: pruned past cursor" means messages aged
out of retention before that subscriber read them, and are gone. If you see
that on your own id, you have a real gap in what you know: run `history` to see
what is still retained, and assume anything older is lost.
"#;

#[cfg(test)]
mod tests {
    use super::GUIDE;

    /// The guide is the tool's primary interface for agents. These assert the
    /// load-bearing facts are actually present, so an edit cannot quietly drop
    /// one.
    #[test]
    fn covers_the_essential_concepts() {
        for topic in [
            "partition",
            "--as",
            "wait",
            "hook install",
            "--priority high",
            "--broadcast",
            "history",
            "follow",
            "--json",
            "agent-bus status",
        ] {
            assert!(GUIDE.contains(topic), "guide should mention {topic:?}");
        }
    }

    #[test]
    fn documents_every_exit_code() {
        for line in [
            "0    success",
            "1    usage error",
            "2    `wait` timed out",
            "3    the daemon is unreachable",
        ] {
            assert!(GUIDE.contains(line), "guide should document {line:?}");
        }
    }

    #[test]
    fn states_the_zero_token_and_late_subscriber_guarantees() {
        assert!(GUIDE.contains("ZERO tokens"));
        assert!(GUIDE.contains("DO NOT POLL"));
        assert!(GUIDE.contains("BEFORE it blocks"));
        assert!(GUIDE.contains("in the middle of a tool call"));
        assert!(GUIDE.contains("at the next turn boundary"));
    }
}
