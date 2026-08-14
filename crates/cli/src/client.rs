//! Talks to the daemon, starting it if it is not already running.

use std::{
    fmt,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use agent_bus_core::paths as core_paths;
use agent_bus_protocol::{Request, Response, decode, encode};
use anyhow::{Context, Result, bail};

/// How long to wait for an auto-started daemon to bind its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Marker error meaning "the daemon could not be reached or started".
///
/// Carried as a typed value rather than recognised by matching on message text,
/// because exit code 3 is a documented contract that agents branch on: it must
/// not change just because someone rewords a `bail!`.
#[derive(Debug)]
pub struct DaemonUnavailable {
    pub socket: PathBuf,
    /// Why it is unavailable. `None` means the spawn itself succeeded but the
    /// socket never appeared before the timeout.
    pub cause: Option<anyhow::Error>,
}

impl fmt::Display for DaemonUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.cause {
            Some(cause) => {
                write!(
                    f,
                    "could not start the daemon (socket {}): {cause:#}",
                    self.socket.display()
                )
            }
            None => write!(
                f,
                "daemon did not start within {}s (socket {})",
                START_TIMEOUT.as_secs(),
                self.socket.display()
            ),
        }
    }
}

impl std::error::Error for DaemonUnavailable {}

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    /// Connect, auto-starting the daemon if the socket is absent or dead.
    ///
    /// # Errors
    /// Returns an error if the daemon cannot be reached or started. That error
    /// carries a [`DaemonUnavailable`] cause, which `main` maps to exit code 3.
    pub fn connect() -> Result<Self> {
        let socket = socket_path();

        if let Ok(stream) = UnixStream::connect(&socket) {
            return Self::from_stream(stream);
        }

        // A failed spawn is itself a "daemon unreachable" condition, so it is
        // wrapped in the marker rather than propagated bare: otherwise a
        // missing or unexecutable daemon binary would report exit code 1 and be
        // indistinguishable from the caller mistyping a pattern.
        if let Err(cause) = spawn_daemon(&socket) {
            return Err(DaemonUnavailable { socket, cause: Some(cause) }.into());
        }

        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(stream) = UnixStream::connect(&socket) {
                return Self::from_stream(stream);
            }
            std::thread::sleep(POLL_INTERVAL);
        }

        Err(DaemonUnavailable { socket, cause: None }.into())
    }

    /// Wrap an already-connected socket, without any auto-start.
    ///
    /// Used by `stop`, which must never resurrect a daemon in order to kill it.
    ///
    /// # Errors
    /// Returns an error if the socket handle cannot be duplicated.
    pub fn from_connected(stream: UnixStream) -> Result<Self> {
        Self::from_stream(stream)
    }

    fn from_stream(stream: UnixStream) -> Result<Self> {
        let reader = BufReader::new(stream.try_clone().context("cloning socket handle")?);
        Ok(Self { stream, reader })
    }

    /// Send a request and read one response.
    ///
    /// The daemon reads requests in a loop over the connection, so several
    /// calls may be made on one `Client` — which is what lets `wait` and `read`
    /// send their `Ack` after printing.
    ///
    /// # Errors
    /// Returns an error if the socket write, read, or decode fails.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        let line = encode(request).context("encoding request")?;
        self.stream.write_all(line.as_bytes()).context("writing request")?;
        self.stream.write_all(b"\n").context("writing request")?;
        self.stream.flush().context("flushing request")?;
        self.read_response()
    }

    /// Read one further response, for streaming commands like `follow`.
    ///
    /// Blocks until a line arrives. Returns `Ok(None)` only at end of stream,
    /// when the daemon has closed the connection.
    ///
    /// # Errors
    /// Returns an error if the read or decode fails.
    pub fn read_optional(&mut self) -> Result<Option<Response>> {
        // Loops rather than returning `None` on a blank line: a stray empty
        // line is not end of stream, and treating it as one would silently cut
        // a `follow` short.
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line).context("reading response")?;
            if read == 0 {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            return Ok(Some(decode(line.trim()).context("decoding response")?));
        }
    }

    fn read_response(&mut self) -> Result<Response> {
        match self.read_optional()? {
            Some(response) => Ok(response),
            None => bail!("daemon closed the connection without responding"),
        }
    }
}

/// Where the daemon socket lives.
///
/// Delegates to `agent_bus_core::paths` rather than re-deriving the layout, so
/// the CLI cannot drift from the daemon it is trying to reach.
#[must_use]
pub fn socket_path() -> PathBuf {
    core_paths::socket_path(&core_paths::state_dir_from_env())
}

/// Launch the daemon detached.
///
/// Several clients may race here. The daemon takes an exclusive lock on
/// startup and the losers exit silently, so at most one survives.
fn spawn_daemon(socket: &Path) -> Result<()> {
    let exe = daemon_binary()?;
    let mut command = std::process::Command::new(&exe);
    command
        .stdin(std::process::Stdio::null())
        // The daemon must not share the caller's process group: when a harness
        // kills a tool call's process group (a `wait` that overran its tool
        // timeout, a cancelled step), a daemon in that group dies with it, and
        // the next command starts another one — the pid churn agents observed
        // as a "crash loop". A private group leaves the daemon to outlive the
        // tool call that spawned it, which is the whole point of a resident
        // bus.
        .process_group(0);

    // Capture whatever the daemon writes to stdout/stderr — diagnostics this
    // CLI has not routed into the daemon's own log — into that same log file,
    // rather than /dev/null where it used to vanish. Opening the log is
    // best-effort: if it fails, the previous /dev/null behaviour is the fallback.
    match std::fs::OpenOptions::new().create(true).append(true).open(core_paths::daemon_log_path())
    {
        Ok(file) => {
            let stdout = file
                .try_clone()
                .map_or_else(|_| std::process::Stdio::null(), std::process::Stdio::from);
            command.stdout(stdout).stderr(std::process::Stdio::from(file));
        }
        Err(_) => {
            command.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        }
    }

    command.spawn().with_context(|| {
        format!("starting daemon {} (socket {})", exe.display(), socket.display())
    })?;
    Ok(())
}

/// Find the daemon binary next to this executable, falling back to `PATH`.
fn daemon_binary() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("AGENT_BUS_DAEMON_BIN") {
        return Ok(PathBuf::from(explicit));
    }
    let current = std::env::current_exe().context("locating the agent-bus executable")?;
    if let Some(dir) = current.parent() {
        let sibling = dir.join("agent-bus-daemon");
        if sibling.exists() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("agent-bus-daemon"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the typed marker: `main` must be able to recognise it
    /// after it has been wrapped by `anyhow` context.
    #[test]
    fn daemon_unavailable_survives_anyhow_wrapping() {
        let error: anyhow::Error =
            DaemonUnavailable { socket: "/tmp/x.sock".into(), cause: None }.into();
        let wrapped = error.context("connecting to the daemon");
        assert!(wrapped.chain().any(<dyn std::error::Error>::is::<DaemonUnavailable>));
    }

    /// A daemon binary that cannot even be spawned is still "unavailable", not
    /// a usage error, so the marker must carry that cause rather than the raw
    /// spawn failure escaping untyped.
    #[test]
    fn a_spawn_failure_is_reported_as_unavailable() {
        let error: anyhow::Error = DaemonUnavailable {
            socket: "/tmp/x.sock".into(),
            cause: Some(anyhow::anyhow!("No such file or directory")),
        }
        .into();
        assert!(error.chain().any(<dyn std::error::Error>::is::<DaemonUnavailable>));
        assert!(error.to_string().contains("could not start the daemon"), "{error}");
    }
}
