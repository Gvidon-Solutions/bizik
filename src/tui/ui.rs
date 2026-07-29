//! Rendering.
//!
//! Three fixed regions — a tab bar that always says where you are, the list,
//! and a footer that always says which keys do something here. Nothing is
//! modal without being visibly modal, and every screen advertises its own way
//! out.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use super::rows::Row;
use super::{App, InputKind, Overlay, Screen, ToastKind};
use crate::reconcile::State;
use crate::util::one_line;

const BG: Color = Color::Rgb(239, 241, 245);
const SURFACE: Color = Color::Rgb(230, 233, 239);
const SELECTED: Color = Color::Rgb(220, 224, 232);
const FG: Color = Color::Rgb(76, 79, 105);
const DIM: Color = Color::Rgb(140, 143, 161);
const ACCENT: Color = Color::Rgb(136, 57, 239);
const RED: Color = Color::Rgb(210, 15, 57);
const GREEN: Color = Color::Rgb(64, 160, 43);
const YELLOW: Color = Color::Rgb(223, 142, 29);
const BLUE: Color = Color::Rgb(30, 102, 245);
const BORDER: Color = Color::Rgb(188, 192, 204);

pub fn draw(frame: &mut Frame, app: &mut App) {
    frame.render_widget(Clear, frame.area());
    frame.render_widget(
        Block::default().style(Style::default().fg(FG).bg(BG)),
        frame.area(),
    );

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_body(frame, app, body);
    draw_footer(frame, app, footer);

    match &app.state.overlay {
        Some(Overlay::Help) => draw_help(frame, frame.area()),
        Some(Overlay::Confirm { prompt, .. }) => draw_confirm(frame, prompt, frame.area()),
        Some(Overlay::Input {
            prompt,
            value,
            kind,
        }) => draw_input(frame, prompt, value, kind, frame.area()),
        None => {}
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let current = app.state.stack.last().cloned().unwrap_or(Screen::Folders);
    let active = current.tab_index();

    let mut spans = vec![Span::styled(
        "  🧭 BIZIK  ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    for (i, tab) in Screen::TABS.iter().enumerate() {
        let selected = active == Some(i);
        let style = if selected {
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM).bg(SURFACE)
        };
        spans.push(Span::styled(format!(" {} ", tab.title()), style));
    }

    // A folder's detail screen is not a tab, so it is shown as a trail from the
    // tab it came from — the way back is visible rather than remembered.
    if active.is_none() {
        spans.push(Span::styled(
            format!(" › {} ", current.title()),
            Style::default()
                .fg(BG)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Starting happens off the drawing thread, so say it is happening —
    // otherwise a slow ssh looks like a key that did nothing.
    if app.state.starting > 0 {
        spans.push(Span::styled(
            format!("  🚀 starting {}…", app.state.starting),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    } else if app.state.refreshing {
        spans.push(Span::styled("  🔄 refreshing…", Style::default().fg(DIM)));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().fg(FG).bg(SURFACE))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(BORDER)),
            ),
        area,
    );
}

fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .state
        .rows
        .iter()
        .map(|row| render_row(row, width, app))
        .collect();

    let list = List::new(items)
        .style(Style::default().fg(FG).bg(BG))
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(
            Style::default()
                .fg(FG)
                .bg(SELECTED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("  ");

    frame.render_stateful_widget(list, area, &mut app.state.list);
}

fn render_row<'a>(row: &'a Row, width: usize, app: &App) -> ListItem<'a> {
    match row {
        Row::Folder {
            host,
            folder,
            sessions,
            running,
            attention,
            blocked,
        } => {
            let mut spans = vec![
                Span::styled(format!("{:<10}", trunc(host, 10)), Style::default().fg(DIM)),
                Span::raw(format!("📁 {:<21}", trunc(&folder.display_name(), 21))),
            ];
            if *blocked > 0 {
                spans.push(Span::styled(
                    format!("🔔 {blocked} blocked  "),
                    Style::default().fg(RED).add_modifier(Modifier::BOLD),
                ));
            } else if *attention > 0 {
                spans.push(Span::styled(
                    format!("👀 {attention} waiting  "),
                    Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                ));
            } else if *running > 0 {
                spans.push(Span::styled(
                    format!("🟢 {running} running  "),
                    Style::default().fg(YELLOW),
                ));
            } else if *sessions > 0 {
                spans.push(Span::styled(
                    format!("⚫ {sessions} stopped  "),
                    Style::default().fg(DIM),
                ));
            } else {
                spans.push(Span::raw("            "));
            }
            if let Some(branch) = &folder.git_branch {
                spans.push(Span::styled(
                    format!("{} ", trunc(branch, 16)),
                    Style::default().fg(ACCENT),
                ));
            }
            spans.push(Span::styled(
                trunc(&folder.path, width.saturating_sub(56)),
                Style::default().fg(DIM),
            ));
            ListItem::new(Line::from(spans))
        }

        Row::NewSession { agent, .. } => ListItem::new(Line::from(vec![
            Span::styled("➕ ", Style::default().fg(ACCENT)),
            Span::styled(
                format!("new {agent} session"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ])),

        Row::Session { host, view } => {
            let (session, status, preview) = (&view.session, view.state, &view.preview);
            let picked = app.state.selected.contains(&(host.clone(), session.id));
            let mut spans = vec![
                Span::styled(
                    if picked { "✅ " } else { "   " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!("{} {:<10}", status_emoji(status), status.label()),
                    status_style(status),
                ),
                Span::styled(format!("{:<9}", trunc(host, 9)), Style::default().fg(DIM)),
                Span::styled(
                    format!("{:<7}", session.agent.as_str()),
                    Style::default().fg(DIM),
                ),
                Span::raw(trunc(&session.title, 34)),
            ];
            if let Some(p) = preview {
                spans.push(Span::styled(
                    format!("  {}", one_line(p, width.saturating_sub(70).max(10))),
                    Style::default().fg(DIM),
                ));
            }
            ListItem::new(Line::from(spans))
        }

        Row::Chat { chat, .. } => {
            let when = ago(chat.last_active);
            ListItem::new(Line::from(vec![
                Span::styled("  💬 ", Style::default().fg(DIM)),
                Span::styled(
                    format!("{:<7}", chat.agent.as_str()),
                    Style::default().fg(DIM),
                ),
                Span::raw(trunc(&chat.display_title(), 46)),
                Span::styled(format!("  {when}"), Style::default().fg(DIM)),
            ]))
        }

        Row::HostEntry { host, detail, ok } => ListItem::new(Line::from(vec![
            Span::styled(
                if *ok { "🟢 " } else { "🔴 " },
                Style::default().fg(if *ok { GREEN } else { RED }),
            ),
            Span::raw(format!("{:<16}", trunc(&host.name, 16))),
            Span::styled(
                format!("{:<24}", host.ssh.as_deref().unwrap_or("(this machine)")),
                Style::default().fg(DIM),
            ),
            Span::styled(
                trunc(detail, width.saturating_sub(44)),
                Style::default().fg(DIM),
            ),
        ])),

        Row::LayoutEntry { layout, missing } => {
            let mut spans = vec![
                Span::styled("🗂️ ", Style::default().fg(ACCENT)),
                Span::raw(format!("{:<24}", trunc(&layout.name, 24))),
                Span::styled(
                    format!("{} panes", layout.panes.len()),
                    Style::default().fg(DIM),
                ),
            ];
            if *missing > 0 {
                spans.push(Span::styled(
                    format!("  {missing} missing"),
                    Style::default().fg(RED),
                ));
            }
            ListItem::new(Line::from(spans))
        }

        Row::Note(text) => ListItem::new(Line::from(Span::styled(
            format!("  ℹ️ {text}"),
            Style::default().fg(YELLOW),
        ))),
    }
}

fn status_style(status: State) -> Style {
    match status {
        State::NeedsYou => Style::default().fg(RED).add_modifier(Modifier::BOLD),
        State::Done => Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        State::Working => Style::default().fg(YELLOW),
        State::YourTurn => Style::default().fg(GREEN),
        // The agent is gone and only the fallback shell is left. Red, because
        // it looked like "running" for as long as nobody distinguished them.
        State::Exited => Style::default().fg(RED),
        State::Up => Style::default().fg(BLUE),
        State::Down => Style::default().fg(DIM),
    }
}

fn status_emoji(status: State) -> &'static str {
    match status {
        State::NeedsYou => "🔔",
        State::Done => "✅",
        State::Working => "🟡",
        State::YourTurn => "👀",
        State::Up => "🟢",
        State::Exited => "🔴",
        State::Down => "⚫",
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // A toast takes the footer, because an error the user cannot see is an
    // error that gets repeated.
    if let Some((text, kind, _)) = &app.state.toast {
        let style = match kind {
            ToastKind::Info => Style::default()
                .fg(GREEN)
                .bg(SURFACE)
                .add_modifier(Modifier::BOLD),
            ToastKind::Error => Style::default()
                .fg(RED)
                .bg(SURFACE)
                .add_modifier(Modifier::BOLD),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!("  {text} "), style)))
                .style(Style::default().bg(SURFACE))
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(BORDER)),
                ),
            area,
        );
        return;
    }

    if app.state.filtering {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " filter: ",
                    Style::default()
                        .fg(BG)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(" {}▏", app.state.filter)),
                Span::styled("  enter keep · esc clear", Style::default().fg(DIM)),
            ]))
            .style(Style::default().fg(FG).bg(SURFACE))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(BORDER)),
            ),
            area,
        );
        return;
    }

    let keys = match app.state.stack.last() {
        Some(Screen::Folders) => {
            "enter open · e rename · d unmark · / filter · w panes · q detach · ? keys"
        }
        Some(Screen::Folder { .. }) => {
            "enter start+view · b background · space select · x stop · d forget · esc back"
        }
        Some(Screen::Running) => "enter view · space select · x stop · w workspace · esc back",
        Some(Screen::Layouts) => "enter restore · S save workspace · d delete · esc back",
        Some(Screen::Hosts) => "i install bizik · r refresh · esc back",
        None => "",
    };

    let mut spans = vec![Span::styled(format!(" {keys}"), Style::default().fg(DIM))];
    if !app.state.selected.is_empty() {
        spans.push(Span::styled(
            format!(
                "  [{} selected — enter opens all]",
                app.state.selected.len()
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    if !app.state.filter.is_empty() {
        spans.push(Span::styled(
            format!("  filter “{}”", app.state.filter),
            Style::default().fg(YELLOW),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().fg(FG).bg(SURFACE))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(BORDER)),
            ),
        area,
    );
}

// ---------------------------------------------------------------------------
// Overlays
// ---------------------------------------------------------------------------

fn draw_help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "moving",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  ↑ ↓ / j k     move          g G   first / last"),
        Line::from("  tab / ⇧tab    switch screen  /     filter"),
        Line::from("  esc           back — always  q     detach (everything keeps running)"),
        Line::from("  Q             close the panes and the dashboard on this machine"),
        Line::from("  F10           show / hide the project and session sidebar"),
        Line::from(""),
        Line::from(Span::styled(
            "sessions",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  enter         start and show beside the sidebar"),
        Line::from("  b             start in the background, stay here"),
        Line::from("  space         select · then enter starts them all"),
        Line::from("  x             stop (conversation is kept)"),
        Line::from("  d             forget the record · unmark a folder"),
        Line::from("  w             jump to the sidebar workspace"),
        Line::from(""),
        Line::from(Span::styled(
            "layouts and hosts",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  S             save the active workspace as a layout"),
        Line::from("  i             install bizik on the selected host"),
        Line::from("  r             refresh now"),
        Line::from(""),
        Line::from(Span::styled(
            "status  🔔 needs you   ✅ done   🟡 working   👀 your turn   🔴 exited   🟢 running   ⚫ stopped",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "🔔 is blocked on a question and will wait forever. 👀 means idle with",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "no detail — run `bzk hooks install` on a host for precise states.",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "from a pane back to here: {}  (or tmux prefix, then 0)",
                crate::tmux::return_key()
            ),
            Style::default().fg(ACCENT),
        )),
        Line::from(Span::styled(
            "click a top tab or sidebar session to switch · z zooms the agent",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "any key closes this",
            Style::default().fg(DIM),
        )),
    ];

    let content_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let popup = centered(74, content_height, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(FG).bg(BG))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ACCENT))
                    .title(" ⌨️ keys "),
            ),
        popup,
    );
}

fn draw_confirm(frame: &mut Frame, prompt: &str, area: Rect) {
    let popup = centered(64, 7, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(prompt.to_string()),
            Line::from(""),
            Line::from(Span::styled(
                "y confirm · n or esc cancel",
                Style::default().fg(DIM),
            )),
        ])
        .style(Style::default().fg(FG).bg(BG))
        .wrap(Wrap { trim: true })
        .alignment(Alignment::Left)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(YELLOW))
                .title(" ⚠️ confirm "),
        ),
        popup,
    );
}

fn draw_input(frame: &mut Frame, prompt: &str, value: &str, kind: &InputKind, area: Rect) {
    let title = match kind {
        InputKind::SaveLayout => " save layout ",
        InputKind::Relabel { .. } => " rename ",
    };
    let popup = centered(64, 7, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(prompt.to_string(), Style::default().fg(DIM))),
            Line::from(format!("{value}▏")),
            Line::from(""),
            Line::from(Span::styled(
                "enter confirm · esc cancel",
                Style::default().fg(DIM),
            )),
        ])
        .style(Style::default().fg(FG).bg(BG))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(title),
        ),
        popup,
    );
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn centered(width_percent: u16, height: u16, area: Rect) -> Rect {
    let width = area.width * width_percent / 100;
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Clamp to `max` display cells, counting characters rather than bytes so a
/// Cyrillic label is not cut mid-character.
fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".into();
    }
    s.chars()
        .take(max - 1)
        .chain(std::iter::once('…'))
        .collect()
}

/// Coarse relative time. Precision beyond this is noise in a list.
fn ago(when_ms: u64) -> String {
    let now = crate::util::now_ms();
    if when_ms == 0 || when_ms > now {
        return "just now".into();
    }
    let secs = (now - when_ms) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, Chat, Folder, Host, Layout, Session};
    use crate::reconcile::{SessionView, State as SessionState};
    use crate::store::LocalStore;
    use crate::tui::Confirm;
    use crate::tui::state::State as DashboardState;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::mpsc::channel;
    use std::time::Instant;

    fn test_app(rows: Vec<Row>) -> super::App {
        let (tx, rx) = channel();
        let (launch_tx, launch_rx) = channel();
        let mut state = DashboardState {
            rows,
            ..DashboardState::default()
        };
        state.list.select(Some(0));
        super::App {
            local: LocalStore::default(),
            probes: Vec::new(),
            state,
            last_refresh: Instant::now(),
            quit: false,
            tx,
            rx,
            launch_tx,
            launch_rx,
        }
    }

    fn render(app: &mut super::App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("creating the test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("rendering the dashboard");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(trunc("методичка", 5), "мето…");
        assert_eq!(trunc("short", 10), "short");
        assert_eq!(trunc("abc", 1), "…");
    }

    #[test]
    fn relative_time_buckets() {
        let now = crate::util::now_ms();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 120_000), "2m ago");
        assert_eq!(ago(now - 7_200_000), "2h ago");
        assert_eq!(ago(now - 172_800_000), "2d ago");
        // A clock skew between hosts must not render as a negative age.
        assert_eq!(ago(now + 10_000), "just now");
    }

    #[test]
    fn centered_popup_fits_inside_a_small_terminal() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 5,
        };
        let popup = centered(64, 20, area);
        assert!(popup.height <= area.height);
        assert!(popup.x + popup.width <= area.width);
    }

    #[test]
    fn every_row_variant_renders_on_a_headless_terminal() {
        let folder = Folder::new("/srv/repo".into());
        let session = Session::new(folder.id, AgentKind::Claude, "important work".into());
        let host = Host::new("server".into(), Some("root@example".into()));
        let rows = vec![
            Row::Folder {
                host: "server".into(),
                folder: folder.clone(),
                sessions: 1,
                running: 1,
                attention: 0,
                blocked: 0,
            },
            Row::NewSession {
                host: "server".into(),
                folder: folder.id,
                agent: AgentKind::Shell,
            },
            Row::Session {
                host: "server".into(),
                view: SessionView {
                    session,
                    state: SessionState::Working,
                    preview: Some("editing src/lib.rs".into()),
                    attention: None,
                },
            },
            Row::Chat {
                host: "server".into(),
                folder: folder.id,
                chat: Chat {
                    agent: AgentKind::Claude,
                    id: "chat-1".into(),
                    cwd: folder.path,
                    title: Some("history".into()),
                    last_prompt: None,
                    git_branch: None,
                    last_active: crate::util::now_ms(),
                    size: 1,
                },
            },
            Row::HostEntry {
                host,
                detail: "reachable".into(),
                ok: true,
            },
            Row::LayoutEntry {
                layout: Layout::new("focus layout".into(), Vec::new(), None),
                missing: 0,
            },
            Row::Note("plain note".into()),
        ];
        let mut app = test_app(rows);

        let rendered = render(&mut app, 120, 24);
        for expected in [
            "repo",
            "new shell session",
            "important work",
            "history",
            "root@example",
            "focus layout",
            "plain note",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected}: {rendered}"
            );
        }
    }

    #[test]
    fn overlays_and_footer_states_render_without_a_real_terminal() {
        let mut app = test_app(vec![Row::Note("empty".into())]);

        app.state.overlay = Some(Overlay::Help);
        assert!(render(&mut app, 100, 34).contains("any key closes this"));

        app.state.overlay = Some(Overlay::Confirm {
            prompt: "stop it?".into(),
            action: Confirm::CloseViewer,
        });
        assert!(render(&mut app, 100, 20).contains("stop it?"));

        app.state.overlay = Some(Overlay::Input {
            prompt: "layout name".into(),
            value: "focus".into(),
            kind: InputKind::SaveLayout,
        });
        let input = render(&mut app, 100, 20);
        assert!(input.contains("layout name"));
        assert!(input.contains("focus"));

        app.state.overlay = None;
        app.state.filtering = true;
        app.state.filter = "needle".into();
        assert!(render(&mut app, 100, 12).contains("needle"));

        app.state.info("saved");
        assert!(render(&mut app, 100, 12).contains("saved"));
    }
}
