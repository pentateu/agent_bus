//! Human-readable and JSON rendering of messages and status.

use agent_bus_core::{Message, Priority};
use agent_bus_protocol::StatusReport;
use anyhow::{Context, Result};

/// Render one message in the compact human format.
///
/// Split out from the printing so it can be asserted on without capturing
/// stdout. The high-priority marker is a leading `!`, chosen so a normal and an
/// urgent message stay column-aligned when several are printed together.
#[must_use]
pub fn format_message(message: &Message) -> String {
    let marker = match message.priority {
        Priority::High => "!",
        Priority::Normal => " ",
    };
    format!("{marker}[{}] {} <{}>\n{}", message.ts, message.topic, message.from, message.body)
}

/// Print messages either as raw JSON lines or a compact human format.
///
/// # Errors
/// Returns an error if serialization fails.
pub fn print_messages(messages: &[Message], json: bool) -> Result<()> {
    for message in messages {
        if json {
            println!("{}", serde_json::to_string(message).context("serializing message")?);
        } else {
            println!("{}", format_message(message));
        }
    }
    Ok(())
}

/// Print a status report.
///
/// # Errors
/// Returns an error if serialization fails.
pub fn print_status(status: &StatusReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status).context("serializing status")?);
        return Ok(());
    }

    println!("agent-bus daemon");
    println!("  pid:    {}", status.pid);
    println!("  uptime: {}s", status.uptime_secs);
    println!("  socket: {}", status.socket_path);

    if status.partitions.is_empty() {
        println!("  no partitions yet");
        return Ok(());
    }

    for partition in &status.partitions {
        println!("\npartition {}", partition.name);
        println!("  messages: {}", partition.message_count);
        if let Some(age) = partition.oldest_age_secs {
            println!("  oldest:   {age}s ago");
        }
        if partition.skipped_records > 0 {
            println!("  WARNING: {} corrupt record(s) skipped", partition.skipped_records);
        }
        for subscriber in &partition.subscribers {
            let flag =
                if subscriber.snapped { "  (missed messages: pruned past cursor)" } else { "" };
            // The pattern is printed because it is half the cursor key: one
            // subscriber reading two patterns has two independent positions,
            // and the id alone would render them as two identical lines.
            println!(
                "  - {} [{}] lag={}{}",
                subscriber.id, subscriber.pattern, subscriber.lag, flag
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use agent_bus_core::Topic;

    use super::*;

    fn message(priority: Priority) -> Message {
        Message::new(
            Topic::parse("iot_base/dev_01").unwrap(),
            "ready for review".to_owned(),
            priority,
            Some("dev_01".to_owned()),
        )
    }

    #[test]
    fn human_format_carries_topic_sender_and_body() {
        let rendered = format_message(&message(Priority::Normal));
        assert!(rendered.contains("iot_base/dev_01"), "{rendered}");
        assert!(rendered.contains("<dev_01>"), "{rendered}");
        assert!(rendered.ends_with("\nready for review"), "{rendered}");
    }

    #[test]
    fn high_priority_is_flagged_and_normal_is_not() {
        assert!(format_message(&message(Priority::High)).starts_with('!'));
        assert!(format_message(&message(Priority::Normal)).starts_with(' '));
    }
}
