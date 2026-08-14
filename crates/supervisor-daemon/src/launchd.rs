//! The launchd user-agent plist (§5).
//!
//! `com.agentbus.supervisor` runs the daemon at load, restarts on unexpected
//! exit, and logs to `~/.supervisor/logs/`.

/// Render the launchd plist for the supervisor daemon.
///
/// `daemon_bin` is the absolute path to the `supervisor-daemon` binary;
/// `state_dir` is `~/.supervisor` (expanded).
#[must_use]
pub fn render_plist(daemon_bin: &str, state_dir: &std::path::Path, label: &str) -> String {
    let out_log = state_dir.join("logs").join("out.log");
    let err_log = state_dir.join("logs").join("err.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{daemon_bin}</string>
		<string>daemon</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>{out}</string>
	<key>StandardErrorPath</key>
	<string>{err}</string>
</dict>
</plist>
"#,
        label = label,
        daemon_bin = daemon_bin,
        out = out_log.display(),
        err = err_log.display(),
    )
}

/// The default label.
pub const DEFAULT_LABEL: &str = "com.agentbus.supervisor";

/// The `LaunchAgents` path for the current user.
#[must_use]
pub fn launch_agents_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    std::path::PathBuf::from(home).join("Library").join("LaunchAgents")
}

/// Write the plist and load it with `launchctl`.
///
/// # Errors
/// Any I/O or `launchctl` failure.
pub fn install(daemon_bin: &str, state_dir: &std::path::Path, label: &str) -> anyhow::Result<()> {
    let dir = launch_agents_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("creating {}: {e}", dir.display()))?;
    std::fs::create_dir_all(state_dir.join("logs"))
        .map_err(|e| anyhow::anyhow!("creating logs dir: {e}"))?;
    let path = dir.join(format!("{label}.plist"));
    let plist = render_plist(daemon_bin, state_dir, label);
    std::fs::write(&path, plist).map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
    // Load (ignore failure if already loaded).
    let _ = std::process::Command::new("launchctl")
        .args(["load", path.to_str().unwrap_or_default()])
        .status();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_contains_expected_keys() {
        let state = std::path::Path::new("/Users/u/.supervisor");
        let plist = render_plist("/opt/bin/supervisor-daemon", state, DEFAULT_LABEL);
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(DEFAULT_LABEL));
        assert!(plist.contains("/opt/bin/supervisor-daemon"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>SuccessfulExit</key>"));
        assert!(plist.contains("/Users/u/.supervisor/logs/out.log"));
    }
}
