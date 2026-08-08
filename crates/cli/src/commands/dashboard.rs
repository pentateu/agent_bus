//! `agent-bus dashboard` — a live TUI over daemon metrics.
//!
//! Two threads: a poller owns the socket and emits one `MetricsReport` per
//! second; the main thread owns the terminal, ticks the [`App`] on every
//! report, and redraws on events. Keeping the socket off the UI thread means a
//! slow daemon cannot stall the terminal, and keeping all state in the `App`
//! keeps the poller dumb.

use std::{
    io::{self, IsTerminal},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use agent_bus_protocol::{MetricsReport, Request, Response};
use anyhow::{Context, Result, bail};
use crossterm::{
    cursor,
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    cli::ExitCode,
    client::Client,
    dashboard::{app::App, ui},
};

/// How long the poller sleeps between metrics requests.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What the poller sends to the terminal thread.
enum Poll {
    Report {
        metrics: Box<MetricsReport>,
        elapsed_secs: u64,
    },
    /// The first request could not reach the daemon: nothing to show.
    Unavailable(anyhow::Error),
}

/// Run the dashboard until `q` (or Ctrl-C / Esc).
///
/// # Errors
/// Returns an error if no terminal is attached, the terminal cannot be
/// entered, or the daemon is unreachable on the first poll (exit 3).
pub fn run() -> Result<ExitCode> {
    if !io::stdout().is_terminal() {
        bail!("a terminal is required to draw the dashboard");
    }

    enable_raw_mode().context("entering raw mode")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    execute!(terminal.backend_mut(), EnterAlternateScreen, cursor::Hide)?;
    let _guard = TtyGuard;

    let (tx, rx) = mpsc::channel();
    let poller = thread::spawn(move || poll_loop(&tx));

    let mut app = App::new();
    let mut first_poll_pending = true;
    loop {
        if event_is_quit()? {
            break;
        }
        while let Ok(poll) = rx.try_recv() {
            match poll {
                Poll::Report { metrics, elapsed_secs } => {
                    app.tick(&metrics, elapsed_secs);
                    first_poll_pending = false;
                }
                Poll::Unavailable(error) if first_poll_pending => return Err(error),
                Poll::Unavailable(_) => {} // daemon died mid-run: keep the stale frame
            }
        }
        terminal.draw(|frame| ui::draw(&app, frame))?;
    }

    match poller.join() {
        Ok(()) => {}
        // A panicking poller should not take the terminal down with it.
        Err(_) => eprintln!("agent-bus: metrics poller crashed"),
    }
    Ok(ExitCode::Success)
}

/// Ask the terminal whether the user just quit, without blocking the loop.
fn event_is_quit() -> Result<bool> {
    if !event::poll(Duration::from_millis(50))? {
        return Ok(false);
    }
    match event::read()? {
        CrosstermEvent::Key(key) => Ok(matches!(key.code, KeyCode::Char('q' | 'Q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))),
        _ => Ok(false),
    }
}

/// Own the socket, ask for a report once a second, forward it. Dumb on
/// purpose: every bit of state lives in the app on the other side.
fn poll_loop(tx: &mpsc::Sender<Poll>) {
    let mut client = match Client::connect() {
        Ok(client) => client,
        Err(error) => {
            let _ = tx.send(Poll::Unavailable(error));
            return;
        }
    };
    let mut last = Instant::now();
    loop {
        thread::sleep(POLL_INTERVAL);
        let now = Instant::now();
        match client.request(&Request::Metrics) {
            Ok(Response::Metrics { metrics }) => {
                let elapsed_secs = now.duration_since(last).as_secs().max(1);
                last = now;
                if tx.send(Poll::Report { metrics: Box::new(*metrics), elapsed_secs }).is_err() {
                    return; // UI thread gone: nothing left to do
                }
            }
            Ok(other) => {
                let _ =
                    tx.send(Poll::Unavailable(anyhow::anyhow!("unexpected response: {other:?}")));
                return;
            }
            Err(_) => match Client::connect() {
                Ok(reconnected) => {
                    // The daemon restarted behind us: rebase onto the fresh
                    // one; the next diff tags the app's restart marker.
                    client = reconnected;
                    last = now;
                }
                Err(error) => {
                    let _ = tx.send(Poll::Unavailable(error));
                    return;
                }
            },
        }
    }
}

/// Restore the terminal on every exit path — early errors, `q`, panics.
struct TtyGuard;

impl Drop for TtyGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = execute!(io::stdout(), cursor::Show);
        let _ = disable_raw_mode();
    }
}
