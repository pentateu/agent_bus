//! Where daemon state lives on disk.
//!
//! Every path either process touches is derived here, so there is exactly one
//! place that knows the layout of the state directory. The socket path in
//! particular is a cross-process contract: if the daemon and the CLI computed
//! it separately they could drift, and the only symptom would be the CLI
//! timing out against a socket nobody ever bound.
//!
//! These are pure path computations — they read the environment but never
//! touch the filesystem. Creating, opening, and locking these paths is the
//! caller's job.

use std::path::{Path, PathBuf};

/// Decide where daemon state lives.
///
/// Priority: an explicit override (used by tests and `AGENT_BUS_STATE_DIR`),
/// then `$XDG_RUNTIME_DIR`, then `$HOME/.local/state`. macOS sets no
/// `XDG_RUNTIME_DIR`, so the home fallback is the normal path there.
///
/// Arguments are passed in rather than read from the environment so this is
/// testable as a pure function.
#[must_use]
pub fn resolve_state_dir(
    override_dir: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir;
    }
    if let Some(dir) = runtime_dir {
        return dir.join("agent-bus");
    }
    if let Some(home) = home_dir {
        return home.join(".local").join("state").join("agent-bus");
    }
    PathBuf::from("/tmp/agent-bus")
}

/// Resolve the state directory from the process environment.
#[must_use]
pub fn state_dir_from_env() -> PathBuf {
    resolve_state_dir(
        std::env::var_os("AGENT_BUS_STATE_DIR").map(PathBuf::from),
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

#[must_use]
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("agent-bus.sock")
}

#[must_use]
pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("agent-bus.lock")
}

#[must_use]
pub fn log_path(state_dir: &Path, partition: &str) -> PathBuf {
    state_dir.join(format!("{partition}.jsonl"))
}

#[must_use]
pub fn cursor_path(state_dir: &Path, partition: &str) -> PathBuf {
    state_dir.join(format!("{partition}.cursors.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper reads three specific variables; a typo in any of them would
    /// send the CLI and the daemon to different sockets. Asserted against
    /// `resolve_state_dir` on the ambient environment rather than by setting
    /// variables, because mutating the environment is unsound once other tests
    /// are running in parallel threads.
    #[test]
    fn env_wrapper_reads_the_variables_the_pure_resolver_expects() {
        let expected = resolve_state_dir(
            std::env::var_os("AGENT_BUS_STATE_DIR").map(PathBuf::from),
            std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        );
        assert_eq!(state_dir_from_env(), expected);
    }

    #[test]
    fn state_dir_prefers_explicit_override() {
        let dir = resolve_state_dir(Some("/tmp/abtest".into()), None, Some("/home/u".into()));
        assert_eq!(dir, std::path::PathBuf::from("/tmp/abtest"));
    }

    #[test]
    fn state_dir_falls_back_to_runtime_dir() {
        let dir = resolve_state_dir(None, Some("/run/user/1000".into()), Some("/home/u".into()));
        assert_eq!(dir, std::path::PathBuf::from("/run/user/1000/agent-bus"));
    }

    #[test]
    fn state_dir_falls_back_to_home_when_no_runtime_dir() {
        // macOS has no XDG_RUNTIME_DIR.
        let dir = resolve_state_dir(None, None, Some("/Users/rafael".into()));
        assert_eq!(dir, std::path::PathBuf::from("/Users/rafael/.local/state/agent-bus"));
    }

    #[test]
    fn socket_lives_inside_the_state_dir() {
        let dir = std::path::PathBuf::from("/tmp/abtest");
        assert_eq!(socket_path(&dir), dir.join("agent-bus.sock"));
        assert_eq!(lock_path(&dir), dir.join("agent-bus.lock"));
    }

    #[test]
    fn partition_files_are_derived_from_the_name() {
        let dir = std::path::PathBuf::from("/tmp/abtest");
        assert_eq!(log_path(&dir, "iot_base"), dir.join("iot_base.jsonl"));
        assert_eq!(cursor_path(&dir, "iot_base"), dir.join("iot_base.cursors.json"));
    }
}
