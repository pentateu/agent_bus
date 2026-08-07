//! `agent-bus` — a local event bus for coordinating AI agents.

#![cfg_attr(test, allow(clippy::unwrap_used))]

mod cli;
mod client;
mod commands;
mod guide;
mod output;

use clap::Parser;

use crate::{
    cli::{Cli, Command, ExitCode, parse_duration_secs, subscriber_id},
    client::DaemonUnavailable,
};

fn main() {
    // clap exits 2 by default on an argument error, which collides with our
    // documented "wait timed out" code. A shell loop written as
    // `while agent-bus wait ...; do ...; done` would then treat a typo'd flag
    // as a clean timeout and exit silently. Remap argument errors to 1 so 2
    // means exactly one thing.
    let args = match Cli::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            std::process::exit(match error.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    ExitCode::Success.code()
                }
                _ => ExitCode::Usage.code(),
            });
        }
    };
    match run(args) {
        Ok(code) => std::process::exit(code.code()),
        Err(error) => {
            eprintln!("agent-bus: {error:#}");
            std::process::exit(exit_code_for(&error).code());
        }
    }
}

/// Map a failure onto the documented exit-code contract.
///
/// A connection failure is a distinct, retryable condition, so it gets its own
/// code rather than being lumped in with usage errors. It is recognised by
/// downcasting to a typed marker rather than by matching on message text, so
/// rewording an error cannot silently change the exit code.
fn exit_code_for(error: &anyhow::Error) -> ExitCode {
    if error.chain().any(<dyn std::error::Error>::is::<DaemonUnavailable>) {
        ExitCode::Unavailable
    } else {
        ExitCode::Usage
    }
}

fn run(args: Cli) -> anyhow::Result<ExitCode> {
    let json = args.json;
    match args.command {
        Command::Daemon { pattern } => commands::daemon::run(pattern, json),

        Command::Post { topic, message, priority, from, broadcast } => {
            commands::post::run(topic, message, &priority, from, broadcast, json)
        }

        Command::Wait { pattern, as_id, timeout } => {
            let subscriber = subscriber_id(as_id.as_deref(), &pattern);
            let timeout_secs = timeout.as_deref().map(parse_duration_secs).transpose()?;
            commands::wait::run(&pattern, subscriber, timeout_secs, json)
        }

        Command::Read { pattern, as_id } => {
            let subscriber = subscriber_id(as_id.as_deref(), &pattern);
            commands::read::run(&pattern, subscriber, json)
        }

        Command::Follow { pattern, as_id } => {
            let subscriber = subscriber_id(as_id.as_deref(), &pattern);
            commands::follow::run(pattern, subscriber, json)
        }

        Command::History { pattern, since } => {
            let since_secs = since.as_deref().map(parse_duration_secs).transpose()?;
            commands::history::run(pattern, since_secs, json)
        }

        Command::Hook { action } => commands::hook::run(action, json),

        Command::Status => commands::status::run(json),

        Command::Stop => commands::stop::run(json),

        Command::Guide => {
            print!("{}", guide::GUIDE);
            Ok(ExitCode::Success)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_daemon_maps_to_exit_code_three() {
        let error: anyhow::Error =
            DaemonUnavailable { socket: "/tmp/x.sock".into(), cause: None }.into();
        let wrapped = error.context("while connecting");
        assert_eq!(exit_code_for(&wrapped), ExitCode::Unavailable);
    }

    #[test]
    fn other_failures_map_to_the_usage_code() {
        let error = anyhow::anyhow!("invalid duration \"soon\"");
        assert_eq!(exit_code_for(&error), ExitCode::Usage);
    }
}
