// The dashboard module's surface is not reachable until the `dashboard`
// subcommand (Task 7) drives it; the allow goes with that wiring.
#![allow(dead_code)]

//! Rendering for the dashboard: a pure draw of `App` into a ratatui frame.
//!
//! No terminal I/O happens here — `draw` only builds widgets, so unit tests
//! render into a `TestBackend` with no TTY. The crossterm event loop that
//! turns `DBEvent`s into `App::tick`s is the command layer's job.

use super::app::App;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Chart, Dataset, Gauge, Paragraph, Table},
};

/// One terminal event the command layer turns into an app tick.
pub enum DBEvent {
    Tick,
    Key(char),
}

/// Six-row grid: header, throughput, latency ("time to pick up"), message
/// size, gauges, table.
pub fn draw(app: &App, frame: &mut Frame) {
    let area = frame.area();
    if area.width < 20 || area.height < 10 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(5),
            Constraint::Length(4),
            Constraint::Min(0),
        ])
        .split(frame.area());

    frame.render_widget(Line::from(header_spans(app)), rows[0]);
    draw_throughput(app.series(), frame, rows[1]);
    draw_latency(app.series(), frame, rows[2]);
    draw_size(app.series(), frame, rows[3]);
    draw_gauges(app, frame, rows[4]);
    draw_tables(app, frame, rows[5]);
}

fn header_spans(app: &App) -> Vec<Span<'static>> {
    let s = app.series();
    let mut spans = vec![
        Span::styled(" agent-bus", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                "  {p} p/s · {d} d/s · {bytes}",
                p = s.cur_posts,
                d = s.cur_deliveries,
                bytes = human_bytes(s.cur_bytes)
            ),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" · avg {avg}ms · p95 {p95}ms", avg = s.avg_latency_ms(), p95 = s.cur_p95_ms),
            Style::default().fg(Color::Yellow),
        ),
    ];
    if let Some(last) = app.last_report() {
        spans.push(Span::styled(
            format!(
                " · waiters {wm} · followers {flw} · uptime {up}s",
                wm = last.active_waiters,
                flw = last.active_followers,
                up = last.uptime_secs
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(at_ms) = app.last_restart_ms() {
        spans.push(Span::styled(
            format!(" · restarted at {at_ms}ms"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    spans
}

#[allow(clippy::cast_precision_loss)] // chart coords: bounded buckets, not real data
fn throughput_points(s: &super::app::Series) -> Vec<(f64, f64)> {
    s.minute_buckets.iter().enumerate().map(|(i, b)| (i as f64, b.posts as f64)).collect()
}

#[allow(clippy::cast_precision_loss)] // chart coords: bounded buckets, not real data
fn latency_points(s: &super::app::Series) -> Vec<(f64, f64)> {
    s.minute_buckets.iter().enumerate().map(|(i, b)| (i as f64, b.p95_ms as f64)).collect()
}

#[allow(clippy::cast_precision_loss)] // chart coords: bounded buckets, not real data
fn size_points(s: &super::app::Series) -> Vec<(f64, f64)> {
    s.minute_buckets.iter().enumerate().map(|(i, b)| (i as f64, b.size_sum as f64)).collect()
}

fn draw_throughput(s: &super::app::Series, frame: &mut Frame, area: Rect) {
    let points = throughput_points(s);
    let chart = Chart::new(vec![Dataset::default().data(&points)])
        .block(Block::default().title(Line::from(" posts · ")).borders(Borders::ALL));
    frame.render_widget(chart, area);
}

fn draw_latency(s: &super::app::Series, frame: &mut Frame, area: Rect) {
    let points = latency_points(s);
    let chart = Chart::new(vec![Dataset::default().data(&points)]).block(
        Block::default().title(Line::from(" time to pick up · p95 ms ")).borders(Borders::ALL),
    );
    frame.render_widget(chart, area);
}

fn draw_size(s: &super::app::Series, frame: &mut Frame, area: Rect) {
    let points = size_points(s);
    let chart = Chart::new(vec![Dataset::default().data(&points)])
        .block(Block::default().title(Line::from(" message size · bytes ")).borders(Borders::ALL));
    frame.render_widget(chart, area);
}

fn draw_gauges(app: &App, frame: &mut Frame, area: Rect) {
    let s = app.series();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let avg = Gauge::default()
        .block(Block::default().title(Line::from(" avg latency · last 60s ")).borders(Borders::ALL))
        .percent(capped_percent(s.avg_latency_ms(), 1_000));
    let p95 = Gauge::default()
        .block(Block::default().title(Line::from(" p95 latency ")).borders(Borders::ALL))
        .percent(capped_percent(s.cur_p95_ms, 5_000));
    frame.render_widget(avg, cols[0]);
    frame.render_widget(p95, cols[1]);
}

fn draw_tables(app: &App, frame: &mut Frame, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let rows: Vec<Vec<String>> = app
        .last_report()
        .map(|last| {
            last.partitions
                .iter()
                .map(|p| {
                    vec![p.name.clone(), p.message_count.to_string(), p.undelivered_lag.to_string()]
                })
                .collect()
        })
        .unwrap_or_default();
    let widths =
        [Constraint::Percentage(50), Constraint::Percentage(25), Constraint::Percentage(25)];
    let table = Table::new(rows.iter().map(|r| ratatui::widgets::Row::new(r.clone())), widths)
        .header(
            ratatui::widgets::Row::new(vec!["partition", "msgs", "lag"])
                .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().title(Line::from(" partitions ")).borders(Borders::ALL));
    frame.render_widget(table, cols[0]);

    let topics = app.last_report().map_or_else(
        || String::from("no report yet"),
        |last| {
            last.top_topics
                .iter()
                .map(|t| format!("{topic} · {count} posts", topic = t.topic, count = t.count))
                .collect::<Vec<_>>()
                .join("\n")
        },
    );
    let title = Block::default().title(Line::from(" top topics ")).borders(Borders::ALL);
    frame.render_widget(Paragraph::new(topics).block(title), cols[1]);
}

/// 1234 -> "1.2 KiB" — the daemon speaks humantime-style prefixes.
#[allow(clippy::cast_precision_loss)] // display only; n is a byte count, f64 is exact here
fn human_bytes(n: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0}{}", units[unit])
    } else {
        format!("{value:.1}{}", units[unit])
    }
}

/// Latency as a 0..100 gauge share, with the header over 100% clipped.
fn capped_percent(ms: u64, cap_ms: u64) -> u16 {
    (ms * 100 / cap_ms.max(1)).min(100) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::app::support::report;
    use ratatui::backend::TestBackend;

    fn terminal(width: u16, height: u16) -> ratatui::Terminal<TestBackend> {
        ratatui::Terminal::new(TestBackend::new(width, height)).unwrap()
    }

    fn rendered(term: &ratatui::Terminal<TestBackend>) -> Vec<String> {
        let buffer = term.backend().buffer();
        (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn header_lists_title_rates_and_uptime() {
        let mut app = App::new();
        // First tick seeds the baseline; the second one moves the counters.
        app.tick(&report(12, 8, 42, 1), 1);
        app.tick(&report(24, 16, 43, 1), 1);
        let mut term = terminal(120, 24);
        term.draw(|f| draw(&app, f)).unwrap();
        let first = &rendered(&term)[0];
        assert!(first.contains("agent-bus"), "title: {first}");
        assert!(first.contains("12 p/s"), "post rate: {first}");
        assert!(first.contains("8 d/s"), "delivery rate: {first}");
        assert!(first.contains("uptime 43s"), "uptime: {first}");
    }

    #[test]
    fn a_restart_marks_the_header() {
        let mut app = App::new();
        app.tick(&report(10, 8, 100, 1), 1);
        app.tick(&report(20, 8, 101, 1), 1);
        app.tick(&report(0, 0, 1, 2), 1);
        let mut term = terminal(120, 24);
        term.draw(|f| draw(&app, f)).unwrap();
        assert!(
            rendered(&term)[0].contains("restarted"),
            "banner missing: {:?}",
            rendered(&term)[0]
        );
    }
}
