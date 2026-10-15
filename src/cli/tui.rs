use crate::cli::client::Client;
use chrono::{DateTime, Utc};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout},
    style::{Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Row, Table, TableState},
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Snapshot {
    accounts: Vec<Value>,
    requests: Vec<Value>,
    total_requests: u64,
    total_tokens: u64,
    total_cost: f64,
    message: String,
}

async fn fetch(client: &Client) -> anyhow::Result<Snapshot> {
    let status = client.call("status", json!({})).await?;
    let usage = client.call("usage", json!({ "since": "24h" })).await?;
    let requests = client.call("requests", json!({ "limit": 15 })).await?;
    let rows = usage.as_array().cloned().unwrap_or_default();
    Ok(Snapshot {
        accounts: status["accounts"].as_array().cloned().unwrap_or_default(),
        requests: requests.as_array().cloned().unwrap_or_default(),
        total_requests: rows.iter().filter_map(|r| r["requests"].as_u64()).sum(),
        total_tokens: rows
            .iter()
            .map(|r| r["input_tokens"].as_u64().unwrap_or(0) + r["output_tokens"].as_u64().unwrap_or(0))
            .sum(),
        total_cost: rows.iter().filter_map(|r| r["cost_usd"].as_f64()).sum(),
        message: String::new(),
    })
}

fn pct(v: &Value) -> String {
    v.as_f64().map_or_else(|| "-".into(), |p| format!("{p:.0}%"))
}

fn ago(ts: &Value) -> String {
    let Some(t) = ts.as_str().and_then(|s| s.parse::<DateTime<Utc>>().ok()) else {
        return "-".into();
    };
    let secs = (t - Utc::now()).num_seconds();
    let (sign, s) = if secs < 0 { ("-", -secs) } else { ("", secs) };
    match s {
        0..=59 => format!("{sign}{s}s"),
        60..=3599 => format!("{sign}{}m", s / 60),
        3600..=86399 => format!("{sign}{}h{}m", s / 3600, s % 3600 / 60),
        _ => format!("{sign}{}d{}h", s / 86400, s % 86400 / 3600),
    }
}

fn accounts_table(snap: &Snapshot) -> Table<'_> {
    let rows = snap.accounts.iter().map(|a| {
        let name = a["label"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| a["email"].as_str())
            .unwrap_or("-");
        let flags = format!(
            "{}/{}",
            if a["enabled"].as_bool().unwrap_or(false) { "on" } else { "off" },
            if a["available"].as_bool().unwrap_or(false) { "ok" } else { "busy" }
        );
        Row::new(vec![
            name.to_owned(),
            a["plan"].as_str().unwrap_or("-").to_owned(),
            flags,
            pct(&a["rate_limit"]["primary"]["used_percent"]),
            pct(&a["rate_limit"]["secondary"]["used_percent"]),
            ago(&a["rate_limit"]["secondary"]["resets_at"]),
            a["inflight"].to_string(),
            a["last_error"]["message"].as_str().unwrap_or("").to_owned(),
        ])
    });
    Table::new(
        rows,
        [
            Constraint::Length(28),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
    )
    .header(Row::new(["account", "plan", "state", "5h", "weekly", "resets", "inflight", "last error"]).bold())
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::bordered().title("accounts"))
}

fn requests_table(snap: &Snapshot) -> Table<'_> {
    let rows = snap.requests.iter().map(|r| {
        Row::new(vec![
            r["ts"].as_str().unwrap_or("-").chars().skip(11).take(8).collect::<String>(),
            r["model"].as_str().unwrap_or("-").to_owned(),
            r["status"].to_string(),
            r["input"].to_string(),
            r["output"].to_string(),
            format!("${:.4}", r["cost"].as_f64().unwrap_or(0.0)),
            format!("{}ms", r["latency_ms"]),
        ])
    });
    Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Fill(1),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(10),
        ],
    )
    .header(Row::new(["time", "model", "status", "input", "output", "cost", "latency"]).bold())
    .block(Block::bordered().title("recent requests"))
}

fn draw(f: &mut Frame<'_>, snap: &Snapshot, state: &mut TableState) {
    let [head, accounts, requests, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(17),
        Constraint::Length(1),
    ])
    .areas(f.area());

    f.render_widget(
        Line::from(format!(
            "24h  requests {}  tokens {}  cost ${:.4}",
            snap.total_requests, snap.total_tokens, snap.total_cost
        ))
        .bold(),
        head,
    );

    f.render_stateful_widget(accounts_table(snap), accounts, state);

    f.render_widget(requests_table(snap), requests);

    f.render_widget(
        Line::from(format!(
            "q quit  r refresh  e enable/disable  w warmup  up/down select  {}",
            snap.message
        ))
        .dim(),
        foot,
    );
}

async fn act(client: &Client, snap: &Snapshot, selected: Option<usize>, key: char) -> anyhow::Result<String> {
    let Some(a) = selected.and_then(|i| snap.accounts.get(i)) else {
        return Ok(String::new());
    };
    if key == 'e' {
        let enabled = !a["enabled"].as_bool().unwrap_or(false);
        client.call("account_update", json!({ "id": a["id"], "enabled": enabled })).await?;
        Ok(format!("enabled={enabled}"))
    } else {
        let r = client.call("account_warmup", json!({ "id": a["id"] })).await?;
        Ok(format!("warmup {}", r["status"]))
    }
}

async fn event_loop(term: &mut DefaultTerminal, client: &Client, interval: Duration) -> anyhow::Result<()> {
    let mut snap = fetch(client).await?;
    let mut state = TableState::default().with_selected(Some(0));
    let mut last = Instant::now();
    loop {
        term.draw(|f| draw(f, &snap, &mut state))?;
        let mut refresh = last.elapsed() >= interval;
        if event::poll(Duration::from_millis(200))?
            && let Event::Key(k) = event::read()?
        {
            if k.kind != KeyEventKind::Press {
                continue;
            }
            match k.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('r') => refresh = true,
                KeyCode::Down => state.select_next(),
                KeyCode::Up => state.select_previous(),
                KeyCode::Char(c @ ('e' | 'w')) => {
                    let msg = act(client, &snap, state.selected(), c).await.unwrap_or_else(|e| e.to_string());
                    refresh = true;
                    snap.message = msg;
                }
                _ => {}
            }
        }
        if refresh {
            let message = std::mem::take(&mut snap.message);
            match fetch(client).await {
                Ok(s) => snap = Snapshot { message, ..s },
                Err(e) => snap.message = e.to_string(),
            }
            last = Instant::now();
        }
    }
}

pub(super) async fn run(client: Client, interval_secs: u64) -> anyhow::Result<()> {
    let mut term = ratatui::init();
    let result = event_loop(&mut term, &client, Duration::from_secs(interval_secs.max(1))).await;
    ratatui::restore();
    result
}
