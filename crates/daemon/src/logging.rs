//! File logging for the daemon, plus a panic hook that captures crashes.
//!
//! The daemon is normally started detached with stderr pointing at `/dev/null`
//! (see the CLI's `spawn_daemon`), so the `eprintln!` diagnostics scattered
//! through this crate used to vanish without a trace. Everything worth knowing
//! now lands in one append-only file — including the panics that killed the
//! daemon and, until this module existed, could only be blamed on a hunch.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

use agent_bus_core::paths;

/// The open log handle, shared across threads. `None` means opening failed, in
/// which case messages fall back to stderr (visible when the daemon is run by
/// hand in a terminal).
static LOG: Mutex<Option<File>> = Mutex::new(None);

/// Where the log file lives. Delegates to `agent_bus_core::paths` so the CLI
/// and the daemon cannot disagree about the path they are talking about.
#[must_use]
pub fn log_path() -> PathBuf {
    paths::daemon_log_path()
}

/// Open the log file for appending. Idempotent; call once at startup.
pub fn init() {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new().create(true).append(true).open(&path);
    let mut guard = LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match file {
        Ok(file) => *guard = Some(file),
        Err(error) => eprintln!("agent-bus: cannot open daemon log {}: {error}", path.display()),
    }
}

/// Append one timestamped line to the log, falling back to stderr.
pub fn log_msg(message: &str) {
    let line = format!("{} pid={} {message}\n", agent_bus_core::now_rfc3339(), std::process::id());
    let mut guard = LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(file) = guard.as_mut() {
        // A write failure is not worth crashing over; the next line retries.
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    } else {
        eprintln!("agent-bus: {line}");
    }
}

/// Install a hook so a panic on any thread lands in the log before the process
/// (or task) unwinds. This turns a mystery crash into a diagnosable backtrace;
/// without it, a panicking task was indistinguishable from a hang.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .map(str::to_owned)
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_owned());
        let location =
            info.location().map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
        log_msg(&format!("PANIC at {location}: {payload}"));
        log_msg(&format!("{}", std::backtrace::Backtrace::force_capture()));
        // Also print for a foreground run, where stderr is a terminal.
        let location = info.location().map_or_else(String::new, ToString::to_string);
        eprintln!("thread panicked at {location}:\n{payload}");
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `log_msg` must not crash even before `init` has opened the file: the
    /// very first panic hook output happens before the log is guaranteed open,
    /// and that path is the one under stress.
    #[test]
    fn log_msg_works_before_init() {
        log_msg("before init");
    }
}
