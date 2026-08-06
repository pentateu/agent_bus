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


SUBSCRIBER IDENTITY AND CURSORS

The bus remembers what you have already received. That memory is keyed by a
subscriber id, and it persists across restarts — kill your terminal, come back
an hour later, and you resume exactly where you left off rather than re-reading
everything or missing what arrived while you were gone.

Your subscriber id defaults to the pattern you subscribe with. So this:

    agent-bus wait 'iot_base/**'

uses the id "iot_base/**" and needs no flag at all. But if TWO agents both
watch `iot_base/**`, they would share one cursor and steal each other's
messages. Give them distinct identities:

    agent-bus wait 'iot_base/**' --as reviewer_01
    agent-bus wait 'iot_base/**' --as reviewer_02

Now each gets its own copy of every matching message. Rule of thumb: if you are
the only agent on a pattern, omit `--as`; otherwise always pass it, and use the
same value every time so your cursor is continuous.


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

2. HOOK DELIVERY — when you want to keep working and be told as you go

    agent-bus hook install claude-code 'iot_base/**' --as dev_01
    agent-bus hook install opencode 'iot_base/**' --as dev_01

This wires your harness to drain unread messages at turn boundaries and inject
them into your context, so you receive things without ever blocking.

Be clear about the guarantee: NEITHER Claude Code NOR OpenCode can interrupt an
agent in the middle of a tool call. Nothing will stop you mid-edit. The real
guarantee is "at the next turn boundary" — when you finish what you are doing
and would otherwise go idle. If you need to act on a message the moment it
lands, hooks are the wrong mechanism; use `wait`.

Use `wait` when you are blocked on someone. Use hooks when you are busy and
want ambient awareness. They can be combined, but give them different `--as`
ids so they do not consume each other's messages.


POSTING

    agent-bus post iot_base/reviewer_01 "auth refactor ready for review"

The topic must be concrete. The body may be given as an argument, or piped:

    git diff | agent-bus post iot_base/reviewer_01

Flags:

    --priority high   a delivery hint for the receiver. The bus does not act on
                      it; hook adapters render urgent text differently. Use it
                      for things that should change what the other agent does
                      next, not for everything.

    --from NAME       sender name. Defaults to the topic's last segment, which
                      is usually right for inbox-style topics.


NON-BLOCKING READS

    agent-bus read 'iot_base/**' --as reviewer_01

Prints everything unread and exits immediately, printing nothing if there is
nothing. Use it to check in without committing to a block.

    agent-bus follow 'iot_base/**' --as reviewer_01

Streams messages continuously until interrupted. Useful for a human watching a
terminal; rarely what an agent wants, since it never returns.

    agent-bus history 'iot_base/**' --since 10m

Replays past messages and IGNORES CURSORS entirely — it does not consume
anything and does not move your position, so it is always safe to run. With no
`--since` it returns the entire retained window. This is how you rebuild
context after a restart.


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
     "priority":"normal","from":"dev_01","body":"ready for review"}

    id        ULID, sortable by creation time; also the cursor value
    ts        RFC 3339 timestamp
    topic     the full concrete topic
    priority  "normal" or "high"
    from      sender name
    body      the message text

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

   You discover something every agent on the project needs to know. Post it to
   a shared topic rather than to each inbox:

       agent-bus post iot_base/bugs_found "the retry helper drops the last \
       error; anything relying on its message is wrong"

   Any agent subscribed to `iot_base/**` or `iot_base` receives it, each with
   its own cursor, so nobody consumes it away from anyone else. Use
   `iot_base/lessons_learned` for durable knowledge and `iot_base/bugs_found`
   for defects.

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
age of the oldest message, and every subscriber with its lag — the number of
messages it has not yet consumed. High lag on your own id means you have work
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
