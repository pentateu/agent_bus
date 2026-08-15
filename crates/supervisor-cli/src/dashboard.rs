//! The ratatui dashboard (C14): attaches to the running daemon via the
//! loopback API and renders workspace + agent state. Quit with `q`.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::client::ApiClient;

const REFRESH: Duration = Duration::from_secs(2);

/// Run the dashboard against the daemon at `config`.
///
/// # Errors
/// Terminal setup failures.
pub fn run(config: crate::client::ClientConfig) -> Result<()> {
    let client = ApiClient::new(config)?;
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, &client);
    ratatui::restore();
    result
}

fn run_loop(terminal: &mut ratatui::DefaultTerminal, client: &ApiClient) -> Result<()> {
    let mut workspaces: Vec<serde_json::Value> = Vec::new();
    let mut agents: Vec<(String, serde_json::Value)> = Vec::new();
    let mut last_error = String::new();
    loop {
        if event::poll(REFRESH)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('q')
        {
            return Ok(());
        }
        match refresh(client) {
            Ok((ws, ag)) => {
                workspaces = ws;
                agents = ag;
                last_error.clear();
            }
            Err(e) => last_error = format!("daemon unreachable: {e:#}"),
        }
        terminal.draw(|frame| render(frame, &workspaces, &agents, &last_error))?;
    }
}

/// A refresh payload: workspaces plus `(ws, agent)` pairs.
pub type DashboardSnapshot = (Vec<serde_json::Value>, Vec<(String, serde_json::Value)>);

fn refresh(client: &ApiClient) -> Result<DashboardSnapshot> {
    let workspaces = client.workspaces()?;
    let mut agents = Vec::new();
    for ws in &workspaces {
        let ws_id = ws["id"].as_str().unwrap_or_default();
        for agent in client.agents(ws_id)? {
            agents.push((ws_id.to_owned(), agent));
        }
    }
    Ok((workspaces, agents))
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    workspaces: &[serde_json::Value],
    agents: &[(String, serde_json::Value)],
    error: &str,
) {
    let area = frame.area();
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)])
            .areas(area);

    frame.render_widget(
        Paragraph::new("supervisor — fleet dashboard")
            .style(Style::default().add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL)),
        header,
    );

    let content = render_content(workspaces, agents);
    frame.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL)),
        body,
    );

    let footer_text = if error.is_empty() {
        Line::from(Span::styled("q to quit", Style::default().fg(Color::DarkGray)))
    } else {
        Line::from(Span::styled(error, Style::default().fg(Color::Red)))
    };
    frame.render_widget(Paragraph::new(footer_text), footer);
}

fn render_content(
    workspaces: &[serde_json::Value],
    agents: &[(String, serde_json::Value)],
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = vec![Line::from("WORKSPACE         STATE      PORT")];
    for ws in workspaces {
        let state = ws["state"].as_str().unwrap_or_default();
        let style = match state {
            "on" => Style::default().fg(Color::Green),
            "draining" => Style::default().fg(Color::Yellow),
            "error" => Style::default().fg(Color::Red),
            _ => Style::default(),
        };
        lines.push(Line::from(vec![
            Span::raw(format!("{:<18}", ws["id"].as_str().unwrap_or_default())),
            Span::styled(format!("{state:<10}"), style),
            Span::raw(ws["port"].as_u64().map_or("-".to_owned(), |p| p.to_string())),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "AGENTS",
        Style::default().add_modifier(Modifier::UNDERLINED),
    )));
    for (ws, agent) in agents {
        let state = agent["state"].as_str().unwrap_or_default();
        lines.push(Line::from(format!(
            "  {ws:20} {:<12} {:10} session={}",
            agent["agent_id"].as_str().unwrap_or_default(),
            state,
            agent["session_id"].as_str().unwrap_or("-"),
        )));
    }
    lines
}
