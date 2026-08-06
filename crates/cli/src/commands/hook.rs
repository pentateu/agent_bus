//! Harness integration. Each adapter is a thin wrapper that calls
//! `agent-bus read` at a turn boundary and injects anything unread.
//!
//! Adding a harness means adding one arm here; the daemon is unaffected.

use anyhow::{Context, Result};

use crate::cli::{ExitCode, HookAction};

/// Install or preview harness hook configuration.
///
/// # Errors
/// Returns an error for an unknown harness or if the configuration cannot be
/// rendered or written.
pub fn run(action: HookAction, json: bool) -> Result<ExitCode> {
    let HookAction::Install { harness, pattern, as_id, dry_run } = action;
    let subscriber = crate::cli::subscriber_id(as_id.as_deref(), &pattern);

    match harness.as_str() {
        "claude-code" => install_claude_code(&pattern, &subscriber, dry_run, json),
        "opencode" => install_opencode(&pattern, &subscriber, dry_run, json),
        other => anyhow::bail!("unknown harness {other:?}: expected claude-code or opencode"),
    }
}

/// Render the Claude Code `Stop` hook configuration.
///
/// Claude Code delivers via a `Stop` hook: exiting 2 with output on stderr
/// feeds that text back to the agent and prevents it going idle. The `awk`
/// wrapper is what turns "there was output" into that exit 2, and stays silent
/// when there is nothing unread.
///
/// # Errors
/// Returns an error if the JSON cannot be rendered.
pub fn claude_code_config(pattern: &str, subscriber: &str) -> Result<String> {
    let command = format!(
        "agent-bus read '{pattern}' --as '{subscriber}' --json | \
         awk 'NF {{ found=1; print }} END {{ exit found ? 2 : 0 }}' >&2"
    );

    let snippet = serde_json::json!({
        "hooks": {
            "Stop": [{
                "matcher": "*",
                "hooks": [{ "type": "command", "command": command }]
            }]
        }
    });

    serde_json::to_string_pretty(&snippet).context("rendering hook config")
}

fn install_claude_code(
    pattern: &str,
    subscriber: &str,
    dry_run: bool,
    json: bool,
) -> Result<ExitCode> {
    let rendered = claude_code_config(pattern, subscriber)?;

    if dry_run {
        println!("{rendered}");
        return Ok(ExitCode::Success);
    }

    // Printed for the user to merge by hand rather than written directly:
    // settings.json is shared with every other hook the user has configured,
    // and blindly rewriting it would silently drop them.
    let path = claude_settings_path()?;
    println!("Add the following to {}:\n", path.display());
    println!("{rendered}");
    println!(
        "\nMerge this into the existing \"hooks\" object rather than replacing it, \
         so other hooks keep working."
    );
    if json {
        println!("{}", serde_json::json!({ "harness": "claude-code", "path": path }));
    }
    Ok(ExitCode::Success)
}

/// Render the `OpenCode` plugin source.
///
/// `OpenCode` delivers via a plugin that runs on session idle.
#[must_use]
pub fn opencode_plugin(pattern: &str, subscriber: &str) -> String {
    format!(
        r#"// agent-bus delivery for OpenCode.
// Drains unread bus messages when the session goes idle and injects them.
import {{ execFileSync }} from "node:child_process";

export const AgentBus = async () => ({{
  event: async ({{ event }}) => {{
    if (event.type !== "session.idle") return;
    let out = "";
    try {{
      out = execFileSync("agent-bus",
        ["read", "{pattern}", "--as", "{subscriber}", "--json"],
        {{ encoding: "utf8" }});
    }} catch (e) {{
      // Surface the failure rather than silently dropping messages.
      console.error("agent-bus: read failed:", e.message);
      return;
    }}
    if (out.trim()) {{
      console.log("Messages from agent-bus:\n" + out);
    }}
  }},
}});
"#
    )
}

fn install_opencode(
    pattern: &str,
    subscriber: &str,
    dry_run: bool,
    json: bool,
) -> Result<ExitCode> {
    let plugin = opencode_plugin(pattern, subscriber);

    if dry_run {
        println!("{plugin}");
        return Ok(ExitCode::Success);
    }

    // Written directly, unlike Claude Code's settings.json: this is a
    // dedicated, agent-bus-owned file, so there is nothing of the user's to
    // clobber.
    let path = std::path::PathBuf::from(".opencode/plugin/agent-bus.js");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, &plugin).with_context(|| format!("writing {}", path.display()))?;

    if json {
        println!("{}", serde_json::json!({ "harness": "opencode", "path": path }));
    } else {
        println!("wrote {}", path.display());
    }
    Ok(ExitCode::Success)
}

fn claude_settings_path() -> Result<std::path::PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(std::path::PathBuf::from(home).join(".claude").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_config_is_valid_json_with_a_stop_hook() {
        let rendered = claude_code_config("iot_base/**", "reviewer_01").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains("agent-bus read 'iot_base/**'"), "{command}");
        assert!(command.contains("--as 'reviewer_01'"), "{command}");
        // Exit 2 is what makes Claude Code feed the output back to the agent.
        assert!(command.contains("exit found ? 2 : 0"), "{command}");
    }

    #[test]
    fn opencode_plugin_embeds_the_pattern_and_subscriber() {
        let plugin = opencode_plugin("iot_base/**", "reviewer_01");
        assert!(plugin.contains(r#""read", "iot_base/**", "--as", "reviewer_01""#), "{plugin}");
        assert!(plugin.contains("session.idle"), "{plugin}");
    }

    #[test]
    fn an_unknown_harness_is_rejected() {
        let action = HookAction::Install {
            harness: "emacs".to_owned(),
            pattern: "iot_base/**".to_owned(),
            as_id: None,
            dry_run: true,
        };
        let error = run(action, false).unwrap_err().to_string();
        assert!(error.contains("unknown harness"), "{error}");
    }
}
