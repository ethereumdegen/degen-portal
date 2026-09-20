//! The dashboard `degen-portal serve` shows: who it can post as, how much of
//! each budget is left, what is waiting for a human, what has gone out, and
//! every request as it happens.
//!
//! It is the one place the approval queue is usable without thinking — read
//! the text, press `a`, watch it post — which is the whole reason a provider
//! can be put behind one.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use degen_tools_core::errors::DegenError;
use degen_tools_core::server::{Connection, LogEntry};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, LineGauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::{ledger, oauth, policy, queue};

// The same night palette as degen-tools: two windows open side by side should
// look like two windows of the same thing.
const BG_DIM: Color = Color::Rgb(0x56, 0x5f, 0x89);
const TEXT: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const BORDER: Color = Color::Rgb(0x3b, 0x42, 0x61);
const ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);
const PINK: Color = Color::Rgb(0xbb, 0x9a, 0xf7);
const YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68);
const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);
const CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const INK: Color = Color::Rgb(0x1a, 0x1b, 0x26);

const LOG_CAP: usize = 500;
/// One source of truth with `status`, so a new provider shows up in both.
const PROVIDERS: [&str; 3] = crate::status::PROVIDERS;

/// What a background approve/drop came back with.
struct Done {
    what: String,
    ok: bool,
}

/// What the user is about to do that cannot be undone by pressing escape.
enum Confirm {
    Undo { post_id: String, tool: String },
}

struct Dashboard {
    conn: Connection,
    started: Instant,
    show_token: bool,

    accounts: oauth::Accounts,
    policy: policy::Policy,
    entries: Vec<ledger::Entry>,
    held: Vec<queue::Held>,
    selected: usize,

    log: VecDeque<LogEntry>,
    calls: usize,
    failures: usize,

    confirm: Option<Confirm>,
    flash: Option<(String, bool, Instant)>,
    working: bool,
    last_reload: Instant,
}

impl Dashboard {
    fn reload(&mut self) {
        self.last_reload = Instant::now();
        // Metadata only: with the keychain on, reading tokens every second
        // would be a permission prompt every second.
        self.accounts = oauth::load_metadata().unwrap_or_default();
        self.policy = policy::load().unwrap_or_default();
        self.entries = ledger::read();
        self.held = queue::load().map(|q| q.held).unwrap_or_default();
        self.selected = self.selected.min(self.held.len().saturating_sub(1));
    }

    fn say(&mut self, message: impl Into<String>, ok: bool) {
        self.flash = Some((message.into(), ok, Instant::now()));
    }

    /// Published entries within a window, and the cap for that provider.
    fn spend(&self, provider: &str, window: u64) -> (usize, usize) {
        let used = ledger::published_since(&self.entries, provider, window, oauth::now());
        let budget = self.policy.budget(provider);
        (used, if window == 3600 { budget.per_hour } else { budget.per_day })
    }
}

pub fn run(conn: Connection, events: Receiver<LogEntry>) -> Result<(), DegenError> {
    let mut dash = Dashboard {
        conn,
        started: Instant::now(),
        show_token: false,
        accounts: oauth::Accounts::default(),
        policy: policy::Policy::default(),
        entries: Vec::new(),
        held: Vec::new(),
        selected: 0,
        log: VecDeque::new(),
        calls: 0,
        failures: 0,
        confirm: None,
        flash: None,
        working: false,
        last_reload: Instant::now(),
    };
    dash.reload();
    let (done_tx, done_rx) = channel();
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut dash, &events, &done_tx, &done_rx);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    dash: &mut Dashboard,
    events: &Receiver<LogEntry>,
    done_tx: &Sender<Done>,
    done_rx: &Receiver<Done>,
) -> Result<(), DegenError> {
    loop {
        while let Ok(entry) = events.try_recv() {
            if entry.tool.is_some() {
                dash.calls += 1;
                if !entry.ok {
                    dash.failures += 1;
                }
            }
            dash.log.push_front(entry);
            dash.log.truncate(LOG_CAP);
        }
        while let Ok(done) = done_rx.try_recv() {
            dash.working = false;
            dash.say(done.what, done.ok);
            dash.reload();
        }
        if dash.last_reload.elapsed() > Duration::from_secs(1) {
            dash.reload();
        }
        if dash.flash.as_ref().is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(6)) {
            dash.flash = None;
        }
        terminal.draw(|f| draw(f, dash))?;

        if event::poll(Duration::from_millis(200))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // A pending confirmation swallows every other key, so nothing is
            // published because the wrong thing had focus.
            if let Some(pending) = dash.confirm.take() {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Enter => act_confirmed(dash, pending, done_tx),
                    _ => dash.say("cancelled", true),
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                KeyCode::Char('t') => dash.show_token = !dash.show_token,
                KeyCode::Char('r') => {
                    dash.reload();
                    dash.say("reloaded", true);
                }
                KeyCode::Down | KeyCode::Char('j') => dash.selected = (dash.selected + 1).min(dash.held.len().saturating_sub(1)),
                KeyCode::Up | KeyCode::Char('k') => dash.selected = dash.selected.saturating_sub(1),
                KeyCode::Char('a') => approve(dash, done_tx),
                KeyCode::Char('d') => drop_held(dash),
                KeyCode::Char('u') => ask_undo(dash),
                _ => {}
            }
        }
    }
}

fn approve(dash: &mut Dashboard, done_tx: &Sender<Done>) {
    let Some(held) = dash.held.get(dash.selected) else {
        dash.say("nothing is waiting", false);
        return;
    };
    let id = held.id.clone();
    let tool = held.tool.clone();
    let tx = done_tx.clone();
    dash.working = true;
    dash.say(format!("sending {id} ({tool})…"), true);
    // Posting can take seconds; the dashboard keeps drawing while it does.
    std::thread::spawn(move || {
        let done = match queue::approve_quietly(&id) {
            Ok(outcome) if outcome.ok => Done { what: format!("{id} sent"), ok: true },
            Ok(outcome) => Done { what: outcome.error.unwrap_or_else(|| format!("{id} failed")), ok: false },
            Err(e) => Done { what: e.to_string(), ok: false },
        };
        let _ = tx.send(done);
    });
}

fn drop_held(dash: &mut Dashboard) {
    let Some(held) = dash.held.get(dash.selected) else {
        dash.say("nothing is waiting", false);
        return;
    };
    let id = held.id.clone();
    match queue::take(&id) {
        Ok(_) => dash.say(format!("dropped {id}"), true),
        Err(e) => dash.say(e.to_string(), false),
    }
    dash.reload();
}

fn ask_undo(dash: &mut Dashboard) {
    match dash.entries.iter().rev().find(|e| e.ok && e.undo.is_some()) {
        Some(entry) => {
            dash.confirm = Some(Confirm::Undo {
                post_id: entry.post_id.clone().unwrap_or_default(),
                tool: entry.tool.clone(),
            });
        }
        None => dash.say("nothing published can be undone", false),
    }
}

fn act_confirmed(dash: &mut Dashboard, pending: Confirm, done_tx: &Sender<Done>) {
    match pending {
        Confirm::Undo { post_id, .. } => {
            let tx = done_tx.clone();
            dash.working = true;
            dash.say(format!("deleting {post_id}…"), true);
            std::thread::spawn(move || {
                let done = match crate::undo_post(Some(&post_id)) {
                    Ok(()) => Done { what: format!("deleted {post_id}"), ok: true },
                    Err(e) => Done { what: e.to_string(), ok: false },
                };
                let _ = tx.send(done);
            });
        }
    }
}

fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(title, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
        ]))
}

fn draw(f: &mut Frame, dash: &Dashboard) {
    let [header, middle, lower, log, footer] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Min(6),
        Constraint::Percentage(28),
        Constraint::Length(1),
    ])
    .areas(f.area());

    draw_header(f, header, dash);
    let [accounts, budgets] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(middle);
    draw_accounts(f, accounts, dash);
    draw_budgets(f, budgets, dash);
    let [held, published] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(lower);
    draw_queue(f, held, dash);
    draw_published(f, published, dash);
    draw_log(f, log, dash);
    draw_footer(f, footer, dash);
}

fn draw_header(f: &mut Frame, area: Rect, dash: &Dashboard) {
    let pulse = (dash.started.elapsed().as_millis() / 600) % 2 == 0;
    let dot = Span::styled("●", Style::new().fg(if pulse { GREEN } else { Color::Rgb(0x4f, 0x6b, 0x3a) }));
    let uptime = dash.started.elapsed().as_secs();
    let token = if dash.show_token {
        dash.conn.token.clone()
    } else {
        format!("{}…  (t to show)", &dash.conn.token[..6.min(dash.conn.token.len())])
    };
    let queued = dash.held.len();
    let lines = vec![
        Line::from(vec![
            Span::styled(" ◆ DEGEN-PORTAL ", Style::new().fg(INK).bg(PINK).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            dot,
            Span::styled(" LISTENING ", Style::new().fg(GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" v{}  up {}m{:02}s", env!("CARGO_PKG_VERSION"), uptime / 60, uptime % 60), Style::new().fg(BG_DIM)),
            Span::raw("  "),
            if queued > 0 {
                Span::styled(format!(" {queued} waiting for you "), Style::new().fg(INK).bg(YELLOW).add_modifier(Modifier::BOLD))
            } else {
                Span::raw("")
            },
        ]),
        Line::from(vec![
            Span::styled("  api     ", Style::new().fg(BG_DIM)),
            Span::styled(dash.conn.url.clone(), Style::new().fg(CYAN).add_modifier(Modifier::UNDERLINED)),
            Span::styled("   token ", Style::new().fg(BG_DIM)),
            Span::styled(token, Style::new().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled("  .env    ", Style::new().fg(BG_DIM)),
            match &dash.conn.project_env {
                Some(p) => Span::styled(p.clone(), Style::new().fg(GREEN)),
                None => Span::styled("none in this directory (global store only)", Style::new().fg(YELLOW)),
            },
        ]),
    ];
    f.render_widget(Paragraph::new(lines).block(panel("status")), area);
}

fn draw_accounts(f: &mut Frame, area: Rect, dash: &Dashboard) {
    let mut lines: Vec<Line> = Vec::new();
    if dash.accounts.accounts.is_empty() {
        let keys = crate::xauth::keys(&degen_tools_core::config::load_credentials().unwrap_or_default()).is_some();
        lines.push(if keys {
            Line::from(vec![
                Span::styled("   x               ", Style::new().fg(BG_DIM)),
                Span::styled("OAuth 1.0a keys — no expiry, no browser", Style::new().fg(GREEN)),
            ])
        } else {
            Line::from(vec![
                Span::styled("  no X credentials", Style::new().fg(YELLOW)),
                Span::styled("   connect x, or set the four X_API_* keys", Style::new().fg(BG_DIM)),
            ])
        });
    }
    for (id, account) in &dash.accounts.accounts {
        let default = dash.accounts.default.get(&account.provider) == Some(id);
        let live = account.expires_at > oauth::now();
        lines.push(Line::from(vec![
            Span::styled(if default { " ★ " } else { "   " }, Style::new().fg(YELLOW)),
            Span::styled(format!("{id:<22}"), Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
            Span::styled(format!("{:<26}", account.expiry_note()), Style::new().fg(if live { GREEN } else { YELLOW })),
            Span::styled(format!("{} scopes", account.scopes.split_whitespace().count()), Style::new().fg(BG_DIM)),
        ]));
    }

    lines.push(Line::raw(""));
    let bot_set = degen_tools_core::app().credentials.missing("DISCORD_BOT_TOKEN").is_none();
    lines.push(Line::from(vec![
        Span::styled("   discord bot   ", Style::new().fg(BG_DIM)),
        if bot_set {
            Span::styled("token set", Style::new().fg(GREEN))
        } else {
            Span::styled("not set   auth set DISCORD_BOT_TOKEN", Style::new().fg(YELLOW))
        },
    ]));
    lines.push(Line::from(vec![
        Span::styled("   channels      ", Style::new().fg(BG_DIM)),
        if dash.policy.channels.is_empty() {
            Span::styled("none      degen-portal discord allow <id>", Style::new().fg(YELLOW))
        } else {
            Span::styled(dash.policy.channels.iter().cloned().collect::<Vec<_>>().join(", "), Style::new().fg(GREEN))
        },
    ]));
    lines.push(Line::from(vec![
        Span::styled("   dm recipients ", Style::new().fg(BG_DIM)),
        if dash.policy.recipients.is_empty() {
            Span::styled("none      degen-portal instagram allow <igsid>", Style::new().fg(YELLOW))
        } else {
            Span::styled(dash.policy.recipients.iter().cloned().collect::<Vec<_>>().join(", "), Style::new().fg(GREEN))
        },
    ]));

    let title = format!(
        "accounts · {}{}",
        dash.accounts.accounts.len(),
        if dash.accounts.keychain { " · keychain" } else { " · state file" }
    );
    f.render_widget(Paragraph::new(lines).block(panel(&title)), area);
}

fn draw_budgets(f: &mut Frame, area: Rect, dash: &Dashboard) {
    let inner = panel("budget");
    let body = inner.inner(area);
    f.render_widget(inner, area);

    let rows = Layout::vertical([Constraint::Length(2), Constraint::Length(2), Constraint::Length(2), Constraint::Min(0)]).split(body);
    for (i, provider) in PROVIDERS.iter().enumerate() {
        let [label, bars] = Layout::horizontal([Constraint::Length(9), Constraint::Min(10)]).areas(rows[i]);
        let held = dash.policy.approval(provider) == policy::Approval::Queue;
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(*provider, Style::new().fg(TEXT).add_modifier(Modifier::BOLD)),
                Span::styled(if held { " ⏸" } else { "" }, Style::new().fg(YELLOW)),
            ])),
            label,
        );
        let [hour, day] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(bars);
        for (area, window, unit) in [(hour, 3600u64, "hour"), (day, 86_400, "day")] {
            let (used, cap) = dash.spend(provider, window);
            let ratio = if cap == 0 { 1.0 } else { (used as f64 / cap as f64).min(1.0) };
            let colour = if used >= cap {
                RED
            } else if ratio > 0.7 {
                YELLOW
            } else {
                GREEN
            };
            f.render_widget(
                LineGauge::default()
                    .filled_style(Style::new().fg(colour))
                    .unfilled_style(Style::new().fg(BORDER))
                    .ratio(ratio)
                    .label(Span::styled(format!("{used:>3}/{cap:<4} this {unit:<5}"), Style::new().fg(TEXT))),
                area,
            );
        }
    }
}

fn draw_queue(f: &mut Frame, area: Rect, dash: &Dashboard) {
    let title = format!("waiting for you · {}", dash.held.len());
    let block = panel(&title);
    if dash.held.is_empty() {
        let note = if PROVIDERS.iter().any(|p| dash.policy.approval(p) == policy::Approval::Queue) {
            vec![Line::from(Span::styled("nothing held right now", Style::new().fg(BG_DIM)))]
        } else {
            vec![
                Line::from(Span::styled("no provider holds calls for approval", Style::new().fg(BG_DIM))),
                Line::raw(""),
                Line::from(Span::styled("degen-portal approval x queue", Style::new().fg(ACCENT))),
            ]
        };
        f.render_widget(Paragraph::new(note).block(block), area);
        return;
    }
    let rows: Vec<Row> = dash
        .held
        .iter()
        .enumerate()
        .map(|(i, held)| {
            let chosen = i == dash.selected;
            let text = held
                .args
                .get("text")
                .or_else(|| held.args.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{} {}", if chosen { "▸" } else { " " }, held.id),
                    Style::new().fg(if chosen { YELLOW } else { BG_DIM }),
                )),
                Cell::from(Span::styled(held.tool.clone(), Style::new().fg(PINK))),
                Cell::from(Span::styled(text.to_string(), Style::new().fg(if chosen { TEXT } else { BG_DIM }))),
            ])
        })
        .collect();
    let table = Table::new(rows, [Constraint::Length(6), Constraint::Length(20), Constraint::Min(10)])
        .header(Row::new(["ID", "TOOL", "WHAT IT WOULD SAY"]).style(Style::new().fg(BG_DIM).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
        .block(block);
    f.render_widget(table, area);
}

fn draw_published(f: &mut Frame, area: Rect, dash: &Dashboard) {
    let rows: Vec<Row> = dash
        .entries
        .iter()
        .rev()
        .filter(|e| e.post_id.is_some())
        .take(area.height.saturating_sub(3) as usize)
        .map(|entry| {
            let id = entry.post_id.clone().unwrap_or_default();
            // A truncated URL tells you nothing. The handle or channel plus
            // the id is what a human needs to find the thing; `log` has links.
            let where_ = match entry.provider.as_str() {
                "x" | "instagram" => format!("@{}", entry.target.split_once(':').map(|(_, h)| h).unwrap_or(&entry.target)),
                _ => format!("#{}", tail(&entry.target, 6)),
            };
            Row::new(vec![
                Cell::from(Span::styled(ago(entry.at), Style::new().fg(BG_DIM))),
                Cell::from(Span::styled(short_tool(&entry.tool), Style::new().fg(PINK))),
                Cell::from(Span::styled(where_, Style::new().fg(CYAN))),
                Cell::from(Span::styled(id, Style::new().fg(if entry.ok { TEXT } else { RED }))),
            ])
        })
        .collect();
    let block = panel("published");
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("nothing has gone out from this machine", Style::new().fg(BG_DIM))).block(block),
            area,
        );
        return;
    }
    let table = Table::new(
        rows,
        [Constraint::Length(8), Constraint::Length(9), Constraint::Length(16), Constraint::Min(8)],
    )
    .header(Row::new(["WHEN", "WHAT", "WHERE", "ID"]).style(Style::new().fg(BG_DIM).add_modifier(Modifier::BOLD)))
    .column_spacing(1)
    .block(block);
    f.render_widget(table, area);
}

/// `discord_send_message` in nine columns.
fn short_tool(tool: &str) -> String {
    match tool.split_once('_') {
        Some((provider, rest)) => format!("{}·{}", &provider[..1], rest.split('_').next().unwrap_or(rest)),
        None => tool.to_string(),
    }
}

/// The last `n` characters, which is the recognisable end of an id.
fn tail(text: &str, n: usize) -> String {
    match text.char_indices().nth(text.chars().count().saturating_sub(n)) {
        Some((i, _)) if text.chars().count() > n => format!("…{}", &text[i..]),
        _ => text.to_string(),
    }
}

fn draw_log(f: &mut Frame, area: Rect, dash: &Dashboard) {
    let rows: Vec<Row> = dash
        .log
        .iter()
        .take(area.height.saturating_sub(3) as usize)
        .map(|e| {
            let what = e.tool.clone().unwrap_or_else(|| format!("{} {}", e.method, e.path));
            Row::new(vec![
                Cell::from(Span::styled(if e.ok { "ok " } else { "err" }, Style::new().fg(if e.ok { GREEN } else { RED }))),
                Cell::from(Span::styled(what, Style::new().fg(TEXT))),
                Cell::from(Span::styled(format!("{}ms", e.duration_ms), Style::new().fg(BG_DIM))),
                Cell::from(Span::styled(e.note.clone(), Style::new().fg(if e.ok { BG_DIM } else { YELLOW }))),
            ])
        })
        .collect();
    let title = format!("requests · {} calls · {} failed", dash.calls, dash.failures);
    let table = Table::new(rows, [Constraint::Length(3), Constraint::Length(26), Constraint::Length(8), Constraint::Min(10)])
        .column_spacing(1)
        .block(panel(&title));
    f.render_widget(table, area);
}

fn draw_footer(f: &mut Frame, area: Rect, dash: &Dashboard) {
    if let Some(Confirm::Undo { post_id, tool }) = &dash.confirm {
        let line = Line::from(vec![
            Span::styled(" DELETE ", Style::new().fg(INK).bg(RED).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {post_id} ({tool}) — this is public and cannot be taken back twice.  "), Style::new().fg(TEXT)),
            Span::styled("y", Style::new().fg(RED).add_modifier(Modifier::BOLD)),
            Span::styled(" yes   any other key cancels", Style::new().fg(BG_DIM)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if let Some((message, ok, _)) = &dash.flash {
        let line = Line::from(vec![
            Span::styled(if *ok { " ✓ " } else { " ! " }, Style::new().fg(if *ok { GREEN } else { RED }).add_modifier(Modifier::BOLD)),
            Span::styled(first_line(message), Style::new().fg(if *ok { TEXT } else { YELLOW })),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let keys = [("a", "approve"), ("d", "drop"), ("↑↓", "pick"), ("u", "undo last"), ("t", "token"), ("r", "reload"), ("q", "quit")];
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in keys {
        spans.push(Span::styled(key, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)));
        spans.push(Span::styled(format!(" {what}   "), Style::new().fg(BG_DIM)));
    }
    if dash.working {
        spans.push(Span::styled("working…", Style::new().fg(YELLOW)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).alignment(Alignment::Left), area);
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

fn ago(at: u64) -> String {
    let secs = oauth::now().saturating_sub(at);
    match secs {
        0..=90 => format!("{secs}s ago"),
        91..=5400 => format!("{}m ago", secs / 60),
        5401..=172_800 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    pub(super) fn dashboard() -> Dashboard {
        // The panel asks the resolver whether the Discord token is set.
        crate::init();
        Dashboard {
            conn: Connection {
                url: "http://127.0.0.1:7719".into(),
                token: "0123456789abcdef".into(),
                pid: 1,
                cwd: "/Users/you/ai/thing".into(),
                project_env: Some("/Users/you/ai/thing/.env".into()),
                started_at: 0,
            },
            started: Instant::now(),
            show_token: false,
            accounts: oauth::Accounts::default(),
            policy: policy::Policy::default(),
            entries: Vec::new(),
            held: Vec::new(),
            selected: 0,
            log: VecDeque::new(),
            calls: 0,
            failures: 0,
            confirm: None,
            flash: None,
            working: false,
            last_reload: Instant::now(),
        }
    }

    pub(super) fn rendered(dash: &Dashboard) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| draw(f, dash)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(120)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_empty_dashboard_says_what_to_do_next() {
        let screen = rendered(&dashboard());
        assert!(screen.contains("DEGEN-PORTAL"), "{screen}");
        assert!(screen.contains("connect x"), "an empty accounts panel should say how to fill it");
        assert!(screen.contains("no provider holds calls"), "and the queue should explain itself");
        assert!(screen.contains("degen-portal discord allow <id>"), "so should an empty allowlist");
        assert!(screen.contains("0/10") && screen.contains("this hour"), "the budget is visible before anything is spent");
        assert!(!screen.contains("0123456789abcdef"), "the API token is masked until asked for");
    }

    #[test]
    fn a_held_call_shows_its_text_and_the_header_says_so() {
        let mut dash = dashboard();
        let mut args = serde_json::Map::new();
        args.insert("text".into(), serde_json::Value::String("gm degens".into()));
        dash.held.push(queue::Held {
            id: "q1".into(),
            at: oauth::now(),
            provider: "x".into(),
            account: Some("x:me".into()),
            tool: "x_post".into(),
            args,
        });
        let screen = rendered(&dash);
        assert!(screen.contains("gm degens"), "a human approving has to read the words: {screen}");
        assert!(screen.contains("1 waiting for you"), "and be told there is something to read");
        assert!(screen.contains("▸ q1"), "with the selected one marked");
    }

    #[test]
    fn deleting_asks_first() {
        let mut dash = dashboard();
        dash.confirm = Some(Confirm::Undo { post_id: "1790".into(), tool: "x_post".into() });
        let screen = rendered(&dash);
        assert!(screen.contains("DELETE"), "{screen}");
        assert!(screen.contains("1790"));
        assert!(screen.contains("y yes"), "and says which key confirms");
    }

    #[test]
    fn a_spent_budget_reads_as_spent() {
        let mut dash = dashboard();
        dash.policy.budgets.insert("x".into(), policy::Budget { per_hour: 2, per_day: 5 });
        for _ in 0..2 {
            dash.entries.push(ledger::Entry {
                at: oauth::now(),
                provider: "x".into(),
                target: "x:me".into(),
                tool: "x_post".into(),
                digest: "d".into(),
                post_id: Some("1".into()),
                permalink: Some("https://x.com/me/status/1".into()),
                undo: None,
                ok: true,
                status: 201,
            });
        }
        let screen = rendered(&dash);
        assert!(screen.contains("2/2"), "a spent hourly budget shows as spent: {screen}");
        assert!(screen.contains("@me") && screen.contains("x·post"), "what went out, and as whom: {screen}");
    }
}

