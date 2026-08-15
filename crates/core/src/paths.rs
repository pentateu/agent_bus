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

use crate::error::CoreError;

/// File names the daemon owns inside the state directory.
///
/// A partition contributes `<name>.jsonl` and `<name>.cursors.json` to the same
/// flat directory that holds these, so a partition called `agent-bus` is one
/// suffix away from colliding with the socket or the lock file. Nothing today
/// generates such a collision — `.sock`/`.lock` are not suffixes we append —
/// but the namespace is shared, so the stems are reserved outright rather than
/// left to a future suffix making it reachable.
const RESERVED_STEMS: [&str; 1] = ["agent-bus"];

/// A partition name that is safe to interpolate into a file name.
///
/// This type exists so that the step from "arbitrary client string" to "path
/// component" cannot be skipped. [`log_path`] and [`cursor_path`] take a
/// `PartitionName` rather than a `&str`, so a caller holding untrusted input
/// has no way to reach the filesystem without going through [`Self::parse`]
/// first. That is the whole point: an `Ack` request carries a client-supplied
/// partition, and a bare `&str` parameter once let `../../../../tmp/evil`
/// through into `state_dir.join(format!("{partition}.jsonl"))`, creating and
/// truncating files anywhere the daemon's uid could write.
///
/// The invariant enforced is exactly the one [`crate::Topic::partition`] and
/// [`crate::Pattern::partition`] already satisfy: a single non-empty segment
/// with no separator and no whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionName(String);

impl PartitionName {
    /// Validate a partition name.
    ///
    /// # Errors
    /// Returns [`CoreError::InvalidPartition`] if the name is empty, contains a
    /// path separator, `.`-only traversal, whitespace, a NUL byte, or collides
    /// with a file name the daemon reserves for itself.
    pub fn parse(input: &str) -> Result<Self, CoreError> {
        let err = |reason| CoreError::InvalidPartition { input: input.to_owned(), reason };

        if input.is_empty() {
            return Err(err("must not be empty"));
        }
        // `/` is the segment separator and the traversal primitive; `\` is not
        // meaningful on Unix but is rejected anyway so a name can never mean
        // two different things to two different readers.
        if input.contains('/') || input.contains('\\') {
            return Err(err("must not contain a path separator"));
        }
        // `.` and `..` are directory entries, not names. Checked explicitly
        // rather than relying on the separator rule above, which they pass.
        if input == "." || input == ".." {
            return Err(err("must not be a relative path component"));
        }
        if input.chars().any(char::is_whitespace) {
            return Err(err("must not contain whitespace"));
        }
        if input.contains('\0') {
            return Err(err("must not contain a NUL byte"));
        }
        if RESERVED_STEMS.contains(&input) {
            return Err(err("collides with a file name the daemon reserves"));
        }

        // Belt and braces: whatever the rules above allow must still be a
        // single normal component to the platform's own path parser, so a
        // separator this code does not know about cannot slip through.
        let mut components = Path::new(input).components();
        match (components.next(), components.next()) {
            (Some(std::path::Component::Normal(c)), None) if c == input => {}
            _ => return Err(err("must be a single path component")),
        }

        Ok(Self(input.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PartitionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

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

/// Resolve where the daemon's log file lives.
///
/// An explicit `AGENT_BUS_STATE_DIR` (isolated runs, tests) keeps the log next
/// to its state so a test can find it without touching the user's logs.
/// Otherwise the log goes to the platform log directory, where a human looking
/// for it would actually look. `fallback_state_dir` is used only on platforms
/// without a conventional log directory.
#[must_use]
pub fn resolve_daemon_log_path(
    override_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    fallback_state_dir: &Path,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir.join("daemon.log");
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = home_dir {
        return home.join("Library").join("Logs").join("agent-bus-daemon.log");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home_dir;
    }
    fallback_state_dir.join("daemon.log")
}

/// Resolve the daemon log path from the process environment.
#[must_use]
pub fn daemon_log_path() -> PathBuf {
    resolve_daemon_log_path(
        std::env::var_os("AGENT_BUS_STATE_DIR").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        &state_dir_from_env(),
    )
}

#[must_use]
pub fn lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join("agent-bus.lock")
}

/// Takes a validated [`PartitionName`], not a `&str`: this is the boundary
/// where a name becomes a path, so the type system enforces that the name was
/// checked rather than trusting every call site to remember.
#[must_use]
pub fn log_path(state_dir: &Path, partition: &PartitionName) -> PathBuf {
    state_dir.join(format!("{partition}.jsonl"))
}

/// See [`log_path`] for why this takes a [`PartitionName`].
#[must_use]
pub fn cursor_path(state_dir: &Path, partition: &PartitionName) -> PathBuf {
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
    fn daemon_log_follows_an_explicit_state_dir() {
        let dir = std::path::PathBuf::from("/tmp/abtest");
        let path = resolve_daemon_log_path(Some(dir.clone()), None, &dir);
        assert_eq!(path, dir.join("daemon.log"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn daemon_log_uses_the_platform_log_dir_by_default() {
        let path =
            resolve_daemon_log_path(None, Some("/Users/u".into()), std::path::Path::new("/tmp/ab"));
        assert_eq!(path, std::path::PathBuf::from("/Users/u/Library/Logs/agent-bus-daemon.log"));
    }

    #[test]
    fn partition_files_are_derived_from_the_name() {
        let dir = std::path::PathBuf::from("/tmp/abtest");
        let name = PartitionName::parse("iot_base").unwrap();
        assert_eq!(log_path(&dir, &name), dir.join("iot_base.jsonl"));
        assert_eq!(cursor_path(&dir, &name), dir.join("iot_base.cursors.json"));
    }

    #[test]
    fn a_plain_name_is_accepted_and_preserved() {
        let name = PartitionName::parse("iot_base").unwrap();
        assert_eq!(name.as_str(), "iot_base");
        assert_eq!(name.to_string(), "iot_base");
    }

    /// Every partition name in the system comes from `Topic::partition` or
    /// `Pattern::partition`, which are already single non-empty segments. This
    /// pins that the validator does not reject what the rest of the system
    /// legitimately produces.
    #[test]
    fn accepts_the_names_topics_and_patterns_actually_produce() {
        for name in ["iot_base", "other_proj", "a", "proj-1", "proj.1", "PROJ_2"] {
            assert!(PartitionName::parse(name).is_ok(), "{name} must be a valid partition");
        }
    }

    /// The path-traversal regression: an `Ack` carries a client-supplied
    /// partition string, and these must never become a path.
    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for bad in [
            "../../../../tmp/evilzone/pwned",
            "..",
            ".",
            "../evil",
            "/etc/passwd",
            "a/b",
            "sub/../../escape",
            "back\\slash",
        ] {
            assert!(PartitionName::parse(bad).is_err(), "{bad:?} must be rejected as a path");
        }
    }

    #[test]
    fn rejects_empty_whitespace_and_nul() {
        assert!(PartitionName::parse("").is_err());
        assert!(PartitionName::parse("has space").is_err());
        assert!(PartitionName::parse("tab\there").is_err());
        assert!(PartitionName::parse("nul\0byte").is_err());
    }

    /// `agent-bus.sock` and `agent-bus.lock` share the state directory with
    /// every `<partition>.jsonl`, so the stem is reserved.
    #[test]
    fn rejects_names_reserved_by_the_daemon() {
        assert!(PartitionName::parse("agent-bus").is_err());
    }
}
