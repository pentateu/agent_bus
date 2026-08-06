//! The agent-bus daemon: one per OS user, owns all partition state.

#![cfg_attr(test, allow(clippy::unwrap_used))]

// Consumed by the request handler and server loop in Tasks 7-8; until then the
// binary itself calls none of it.
#[allow(dead_code)]
mod log;
#[allow(dead_code)]
mod paths;

// The stub cannot fail yet, but the server loop in Task 8 replaces this body
// with fallible startup (lock acquisition, socket bind); keeping the signature
// avoids rewriting the entry point then.
#[allow(clippy::unnecessary_wraps)]
fn main() -> anyhow::Result<()> {
    Ok(())
}
