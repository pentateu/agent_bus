//! The supervisor agent (C13) assets: the `supervisor` agent prompt and the
//! slash commands that drive the `supervisor` CLI (§4.14).
//!
//! Distinct from the manager (C11): the supervisor agent is the **human-facing**
//! opencode session on the supervisor workspace (port 4199). These assets are
//! written under `~/.supervisor/agent/` so the supervisor workspace can use
//! them as its agent config.

use std::path::Path;

use anyhow::Result;

/// The supervisor agent prompt (`supervisor.md`).
pub const SUPERVISOR_PROMPT: &str = r"# supervisor

You are the agent-bus Fleet Supervisor's human-facing assistant. The human
opens this session to manage their fleet of project workspaces and read
status. You drive the `supervisor` CLI and read `~/.supervisor/fleet.json`
for context. You are NOT the decision engine — escalations are decided by the
background manager, not you.

Use the slash commands for the common actions, and the `supervisor` CLI for
anything else:

- `/start-workspace <name>`  →  `supervisor on <name>`
- `/status`                  →  `supervisor status`
- `/off <name>`              →  `supervisor off <name>`
- `/rules list|reload`       →  `supervisor rules ...`
- `/dag status [graph]`      →  `supervisor dag status ...`
- `/log [tail]`              →  `supervisor log`

Never expose secrets, never run `supervisor bake-back --apply` without the
human's explicit request, and keep answers short.
";

/// The slash-command definitions (name → markdown body).
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    (
        "start-workspace",
        "```md\n---\nname: start-workspace\ndescription: Bring a project workspace online.\n---\n\nRun: `supervisor on <name>`\n",
    ),
    (
        "status",
        "```md\n---\nname: status\ndescription: Show the fleet status.\n---\n\nRun: `supervisor status`\n",
    ),
    (
        "off",
        "```md\n---\nname: off\ndescription: Gracefully take a workspace offline.\n---\n\nRun: `supervisor off <name>`\n",
    ),
    (
        "rules",
        "```md\n---\nname: rules\ndescription: List or reload the offline rules.\n---\n\nRun: `supervisor rules list` or `supervisor rules reload`\n",
    ),
    (
        "dag",
        "```md\n---\nname: dag\ndescription: Show workflow graph status.\n---\n\nRun: `supervisor dag status [graph]`\n",
    ),
    (
        "log",
        "```md\n---\nname: log\ndescription: Show the recent decision log.\n---\n\nRun: `supervisor log [--tail N]`\n",
    ),
];

/// Write the supervisor agent assets under `~/.supervisor/agent/`.
///
/// # Errors
/// Any I/O failure.
pub fn install(state_dir: &Path) -> Result<()> {
    let base = state_dir.join("agent");
    std::fs::create_dir_all(&base)?;
    std::fs::write(base.join("supervisor.md"), SUPERVISOR_PROMPT)?;
    let commands = base.join("commands");
    std::fs::create_dir_all(&commands)?;
    for (name, body) in SLASH_COMMANDS {
        std::fs::write(commands.join(format!("{name}.md")), *body)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_prompt_and_commands() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        assert!(dir.path().join("agent/supervisor.md").exists());
        for (name, _) in SLASH_COMMANDS {
            assert!(
                dir.path().join("agent/commands").join(format!("{name}.md")).exists(),
                "{name} command file missing"
            );
        }
        let prompt = std::fs::read_to_string(dir.path().join("agent/supervisor.md")).unwrap();
        assert!(prompt.contains("/start-workspace"));
    }
}
