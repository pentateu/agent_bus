//! Argument definitions and the small pure helpers they need.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

/// Process exit codes. Stable contract: agents and shell loops branch on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Success = 0,
    Usage = 1,
    /// `wait` expired with nothing delivered.
    Timeout = 2,
    /// The daemon is unreachable and could not be started.
    Unavailable = 3,
}

impl ExitCode {
    /// The numeric status to pass to `exit`.
    ///
    /// Spelled out rather than cast from the discriminant so the contract is
    /// one explicit table instead of an implicit `as` conversion.
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Usage => 1,
            Self::Timeout => 2,
            Self::Unavailable => 3,
        }
    }
}

/// A local event bus for coordinating AI agents across terminal sessions.
///
/// Run `agent-bus guide` for a full explanation written for agents.
#[derive(Debug, Parser)]
#[command(name = "agent-bus", version, about, long_about = None)]
#[command(after_help = "Run `agent-bus guide` for the full usage guide, written for AI agents.")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Ensure the daemon is running and the partition exists.
    #[command(alias = "--daemon")]
    Daemon {
        /// Topic pattern, e.g. `iot_base/*`.
        pattern: String,
    },

    /// Publish a message to a topic.
    #[command(alias = "--post")]
    Post {
        /// Concrete topic, e.g. `iot_base/dev_01`. No wildcards.
        topic: String,
        /// Message body. Read from stdin when omitted.
        message: Option<String>,
        /// Delivery hint for receiving agents.
        #[arg(long, value_parser = ["normal", "high"], default_value = "normal")]
        priority: String,
        /// Sender name. Defaults to the topic's last segment.
        #[arg(long)]
        from: Option<String>,
        /// Deliver this message to every consumer (distinct `--as` label)
        /// whose pattern matches the topic, each getting their own copy once.
        /// Without this, delivery is exclusive per pattern.
        #[arg(long)]
        broadcast: bool,
    },

    /// Block until one unread message arrives, print it, and exit.
    ///
    /// Costs no tokens while blocked. Exits 2 on timeout.
    #[command(alias = "--wait")]
    Wait {
        /// Topic pattern to watch, e.g. `iot_base/**`.
        pattern: String,
        /// Label for status output. Delivery is exclusive per pattern, so this
        /// does not create a second position; two consumers of one pattern
        /// share it and the first to read wins.
        #[arg(long = "as")]
        as_id: Option<String>,
        /// Maximum time to block, e.g. `30m`. Defaults to 30m.
        #[arg(long)]
        timeout: Option<String>,
    },

    /// Print all unread messages and exit immediately.
    #[command(alias = "--read")]
    Read {
        /// Topic pattern to drain, e.g. `iot_base/**`.
        pattern: String,
        /// Label for status output. Delivery is exclusive per pattern, so this
        /// does not create a second position; two consumers of one pattern
        /// share it and the first to read wins.
        #[arg(long = "as")]
        as_id: Option<String>,
    },

    /// Stream messages continuously until interrupted.
    #[command(alias = "--subscribe")]
    Follow {
        /// Topic pattern to stream, e.g. `iot_base/**`.
        pattern: String,
        /// Label for status output. Delivery is exclusive per pattern, so this
        /// does not create a second position; two consumers of one pattern
        /// share it and the first to read wins.
        #[arg(long = "as")]
        as_id: Option<String>,
    },

    /// Replay past messages, ignoring cursors.
    #[command(alias = "--history")]
    History {
        /// Topic pattern to replay, e.g. `iot_base/**`.
        pattern: String,
        /// How far back to look, e.g. `10m`. Defaults to everything retained.
        #[arg(long)]
        since: Option<String>,
    },

    /// Install harness integration so messages arrive at turn boundaries.
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Show daemon state, partitions, and subscriber lag.
    Status,

    /// Live TUI of daemon metrics: rates, latencies, sizes, tables.
    ///
    /// Needs a terminal. Quit with `q`.
    Dashboard,

    /// Shut the daemon down.
    Stop,

    /// Print the full usage guide, written for AI agents.
    Guide,
}

#[derive(Debug, Subcommand)]
pub enum HookAction {
    /// Write hook configuration for a harness.
    Install {
        /// Which harness to configure.
        #[arg(value_parser = ["claude-code", "opencode"])]
        harness: String,
        /// Topic pattern the hook should drain.
        pattern: String,
        /// Subscriber identity the hook uses.
        #[arg(long = "as")]
        as_id: Option<String>,
        /// Print the configuration instead of writing it.
        #[arg(long)]
        dry_run: bool,
    },
}

/// The label to send as `--as`: explicit value, else the pattern itself.
///
/// Defaulting to the pattern means a consumer that never passes `--as` is still
/// identifiable in `status`. The label plays no part in delivery — exclusivity
/// is keyed on the pattern — so this is cosmetic, not positional.
#[must_use]
pub fn subscriber_id(as_id: Option<&str>, pattern: &str) -> String {
    as_id.unwrap_or(pattern).to_owned()
}

/// Parse a duration like `30s`, `10m`, `2h`, or a bare number of seconds.
///
/// # Errors
/// Returns an error if the input is empty or not a recognized duration.
pub fn parse_duration_secs(input: &str) -> Result<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("duration must not be empty");
    }
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Ok(secs);
    }
    match humantime::parse_duration(trimmed) {
        Ok(d) => Ok(d.as_secs()),
        Err(e) => bail!("invalid duration {trimmed:?}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duration_suffixes() {
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("10m").unwrap(), 600);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
    }

    #[test]
    fn bare_number_is_seconds() {
        assert_eq!(parse_duration_secs("45").unwrap(), 45);
    }

    #[test]
    fn rejects_nonsense_durations() {
        assert!(parse_duration_secs("soon").is_err());
        assert!(parse_duration_secs("").is_err());
    }

    #[test]
    fn exit_codes_match_the_documented_contract() {
        assert_eq!(ExitCode::Success as i32, 0);
        assert_eq!(ExitCode::Usage as i32, 1);
        assert_eq!(ExitCode::Timeout as i32, 2);
        assert_eq!(ExitCode::Unavailable as i32, 3);
    }

    /// The discriminants above are the documented contract, and `code()` is
    /// what `main` actually exits with; they must not drift apart.
    #[test]
    fn code_matches_the_discriminant() {
        for exit in [ExitCode::Success, ExitCode::Usage, ExitCode::Timeout, ExitCode::Unavailable] {
            assert_eq!(exit.code(), exit as i32);
        }
    }

    #[test]
    fn cli_parses_the_subcommand_form() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["agent-bus", "post", "iot_base/dev_01", "ready"]).unwrap();
        match cli.command {
            Command::Post { topic, message, .. } => {
                assert_eq!(topic, "iot_base/dev_01");
                assert_eq!(message.as_deref(), Some("ready"));
            }
            _ => panic!("expected the post subcommand"),
        }
    }

    #[test]
    fn wait_defaults_subscriber_to_the_pattern() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["agent-bus", "wait", "iot_base/**"]).unwrap();
        match cli.command {
            Command::Wait { pattern, as_id, .. } => {
                assert_eq!(subscriber_id(as_id.as_deref(), &pattern), "iot_base/**");
            }
            _ => panic!("expected the wait subcommand"),
        }
    }

    #[test]
    fn explicit_as_overrides_the_default_subscriber() {
        assert_eq!(subscriber_id(Some("reviewer_01"), "iot_base/**"), "reviewer_01");
    }
}
