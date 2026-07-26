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

use super::rows::{Row, Status};
use super::{App, InputKind, Overlay, Screen, ToastKind};
use crate::util::one_line;

const DIM: Color = Color::DarkGray;
const ACCENT: Color = Color::Cyan;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_body(frame, app, body);
    draw_footer(frame, app, footer);

    match &app.overlay {
        Some(Overlay::Help) => draw_help(frame, frame.area()),
        Some(Overlay::Confirm { prompt, .. }) => draw_confirm(frame, prompt, frame.area()),
        Some(Overlay::Input { prompt, value, kind }) => {
            draw_input(frame, prompt, value, kind, frame.area())
        }
        None => {}
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let current = app.stack.last().cloned().unwrap_or(Screen::Folders);
    let active = current.tab_index();

    let mut spans = vec![Span::styled(" bizik ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))];
    for (i, tab) in Screen::TABS.iter().enumerate() {
        let selected = active == Some(i);
        let style = if selected {
            Style::default().fg(Color::Black).bg(ACCENT)
        } else {
            Style::default().fg(DIM)
        };
        spans.push(Span::styled(format!(" {} ", tab.title()), style));
    }

    // A folder's detail screen is not a tab, so it is shown as a trail from the
    // tab it came from — the way back is visible rather than remembered.
    if active.is_none() {
        spans.push(Span::styled(
            format!(" › {} ", current.title()),
            Style::default().fg(Color::Black).bg(ACCENT),
        ));
    }

    if app.refreshing {
        spans.push(Span::styled("  refreshing…", Style::default().fg(DIM)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(frame: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|row| render_row(row, width, app))
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::NONE))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▌");

    frame.render_stateful_widget(list, area, &mut app.list);
}

fn render_row<'a>(row: &'a Row, width: usize, app: &App) -> ListItem<'a> {
    match row {
        Row::Folder {
            host,
            folder,
            sessions,
            running,
            attention,
        } => {
            let mut spans = vec![
                Span::styled(format!("{:<10}", trunc(host, 10)), Style::default().fg(DIM)),
                Span::raw(format!("{:<24}", trunc(&folder.display_name(), 24))),
            ];
            if *attention > 0 {
                spans.push(Span::styled(
                    format!("◆ {attention} waiting  "),
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ));
            } else if *running > 0 {
                spans.push(Span::styled(
                    format!("● {running} running  "),
                    Style::default().fg(Color::Yellow),
                ));
            } else if *sessions > 0 {
                spans.push(Span::styled(
                    format!("· {sessions} stopped  "),
                    Style::default().fg(DIM),
                ));
            } else {
                spans.push(Span::raw("            "));
            }
            if let Some(branch) = &folder.git_branch {
                spans.push(Span::styled(
                    format!("{} ", trunc(branch, 16)),
                    Style::default().fg(Color::Magenta),
                ));
            }
            spans.push(Span::styled(
                trunc(&folder.path, width.saturating_sub(56)),
                Style::default().fg(DIM),
            ));
            ListItem::new(Line::from(spans))
        }

        Row::NewSession { agent, .. } => ListItem::new(Line::from(vec![
            Span::styled("＋ ", Style::default().fg(ACCENT)),
            Span::styled(
                format!("new {agent} session"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ])),

        Row::Session {
            host,
            session,
            status,
            preview,
        } => {
            let picked = app.selected.contains(&(host.clone(), session.id));
            let mut spans = vec![
                Span::styled(
                    if picked { "✓ " } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!("{} {:<10}", status.glyph(), status.label()),
                    status_style(*status),
                ),
                Span::styled(format!("{:<9}", trunc(host, 9)), Style::default().fg(DIM)),
                Span::styled(format!("{:<7}", session.agent.as_str()), Style::default().fg(DIM)),
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
                Span::styled("  ↺ ", Style::default().fg(DIM)),
                Span::styled(format!("{:<7}", chat.agent.as_str()), Style::default().fg(DIM)),
                Span::raw(trunc(&chat.display_title(), 46)),
                Span::styled(format!("  {when}"), Style::default().fg(DIM)),
            ]))
        }

        Row::HostEntry { host, detail, ok } => ListItem::new(Line::from(vec![
            Span::styled(
                if *ok { "● " } else { "✕ " },
                Style::default().fg(if *ok { Color::Green } else { Color::Red }),
            ),
            Span::raw(format!("{:<16}", trunc(&host.name, 16))),
            Span::styled(
                format!("{:<24}", host.ssh.as_deref().unwrap_or("(this machine)")),
                Style::default().fg(DIM),
            ),
            Span::styled(trunc(detail, width.saturating_sub(44)), Style::default().fg(DIM)),
        ])),

        Row::LayoutEntry { layout, missing } => {
            let mut spans = vec![
                Span::styled("▦ ", Style::default().fg(ACCENT)),
                Span::raw(format!("{:<24}", trunc(&layout.name, 24))),
                Span::styled(
                    format!("{} panes", layout.panes.len()),
                    Style::default().fg(DIM),
                ),
            ];
            if *missing > 0 {
                spans.push(Span::styled(
                    format!("  {missing} missing"),
                    Style::default().fg(Color::Red),
                ));
            }
            ListItem::new(Line::from(spans))
        }

        Row::Note(text) => ListItem::new(Line::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(Color::Yellow),
        ))),
    }
}

fn status_style(status: Status) -> Style {
    match status {
        Status::YourTurn => Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        Status::Working => Style::default().fg(Color::Yellow),
        Status::Up => Style::default().fg(Color::Blue),
        Status::Down => Style::default().fg(DIM),
    }
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    // A toast takes the footer, because an error the user cannot see is an
    // error that gets repeated.
    if let Some((text, kind, _)) = &app.toast {
        let style = match kind {
            ToastKind::Info => Style::default().fg(Color::Black).bg(Color::Green),
            ToastKind::Error => Style::default().fg(Color::White).bg(Color::Red),
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {text} "), style))),
            area,
        );
        return;
    }

    if app.filtering {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" filter: ", Style::default().fg(Color::Black).bg(ACCENT)),
                Span::raw(format!(" {}▏", app.filter)),
                Span::styled("  enter keep · esc clear", Style::default().fg(DIM)),
            ])),
            area,
        );
        return;
    }

    let keys = match app.stack.last() {
        Some(Screen::Folders) => "enter open · e rename · d unmark · / filter · w panes · ? keys",
        Some(Screen::Folder { .. }) => {
            "enter start+view · b background · space select · x stop · d forget · esc back"
        }
        Some(Screen::Running) => "enter view · space select · x stop · w panes · esc back",
        Some(Screen::Layouts) => "enter restore · S save open panes · d delete · esc back",
        Some(Screen::Hosts) => "i install bizik · r refresh · esc back",
        None => "",
    };

    let mut spans = vec![Span::styled(format!(" {keys}"), Style::default().fg(DIM))];
    if !app.selected.is_empty() {
        spans.push(Span::styled(
            format!("  [{} selected — enter opens all]", app.selected.len()),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
    }
    if !app.filter.is_empty() {
        spans.push(Span::styled(
            format!("  filter “{}”", app.filter),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
        Line::from("  esc           back — always  q     quit (sessions keep running)"),
        Line::from(""),
        Line::from(Span::styled(
            "sessions",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  enter         start and open a pane"),
        Line::from("  b             start in the background, stay here"),
        Line::from("  space         select · then enter opens them all at once"),
        Line::from("  x             stop (conversation is kept)"),
        Line::from("  d             forget the record · unmark a folder"),
        Line::from("  w             jump to the pane window"),
        Line::from(""),
        Line::from(Span::styled(
            "layouts and hosts",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from("  S             save the open panes as a layout"),
        Line::from("  i             install bizik on the selected host"),
        Line::from("  r             refresh now"),
        Line::from(""),
        Line::from(Span::styled(
            "status  ◆ your turn   ● working   ○ running   · stopped",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            "◆ means the agent stopped and is waiting on you.",
            Style::default().fg(DIM),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "from a pane back to here: tmux prefix, then w or 0",
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled("any key closes this", Style::default().fg(DIM))),
    ];

    let popup = centered(74, lines.len() as u16 + 2, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(" keys "),
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
        .wrap(Wrap { trim: true })
        .alignment(Alignment::Left)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" confirm "),
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
    s.chars().take(max - 1).chain(std::iter::once('…')).collect()
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
        let area = Rect { x: 0, y: 0, width: 40, height: 5 };
        let popup = centered(64, 20, area);
        assert!(popup.height <= area.height);
        assert!(popup.x + popup.width <= area.width);
    }
}
