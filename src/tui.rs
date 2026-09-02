use crate::agent::{Agent, HttpAgent, Message, Role};
use crate::chats::{self, ChatSession};
use crate::markdown::agent_skin;
use ansi_to_tui::IntoText;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BILLY_ART: &str = include_str!("assets/billy.ans");

enum ChatEvent {
    Response(anyhow::Result<String>),
}

#[derive(PartialEq)]
enum Focus {
    Input,
    Sidebar,
}

pub async fn run(agent: HttpAgent) -> anyhow::Result<()> {
    let agent = Arc::new(agent);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, agent).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent: Arc<HttpAgent>,
) -> anyhow::Result<()> {
    let mut chats: Vec<ChatSession> = chats::list_chats().unwrap_or_default();
    chats.insert(0, ChatSession::new());
    let mut active: usize = 0;

    let mut input = String::new();
    let mut pending = false;
    let mut spinner_frame = 0usize;
    let mut scroll: u16 = 0;
    let mut auto_scroll = true;
    let mut scroll_to_message: Option<usize> = None;
    let mut focus = Focus::Input;

    let (tx, mut rx) = mpsc::unbounded_channel::<ChatEvent>();
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(80));

    loop {
        terminal.draw(|f| {
            let outer = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(28), Constraint::Min(20)])
                .split(f.area());

            let sidebar_items: Vec<ListItem> = chats
                .iter()
                .enumerate()
                .map(|(i, chat)| {
                    let is_active = i == active;
                    let style = if is_active {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let prefix = if is_active { "● " } else { "  " };
                    ListItem::new(Line::from(Span::styled(
                        format!("{prefix}{}", chat.title),
                        style,
                    )))
                })
                .collect();

            let sidebar_border_style = if focus == Focus::Sidebar {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let sidebar = List::new(sidebar_items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(sidebar_border_style)
                    .title(" Чаты (Ctrl+N — новый) "),
            );
            f.render_widget(sidebar, outer[0]);

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(3),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(outer[1]);

            let title = Paragraph::new(Line::from(vec![
                Span::styled(
                    " agentcli ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}", chats[active].title)),
            ]));
            f.render_widget(title, chunks[0]);

            let mut lines: Vec<Line> = Vec::new();
            let mut target_line: Option<u16> = None;
            if chats[active].messages.is_empty() {
                if let Ok(text) = BILLY_ART.into_text() {
                    lines.extend(text.lines);
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(Span::styled(
                    "        agent-cli — консольный AI-агент",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::raw(""));
            }
            for (i, entry) in chats[active].messages.iter().enumerate() {
                if scroll_to_message == Some(i) {
                    target_line = Some(lines.len() as u16);
                }
                let (label, color) = match entry.role {
                    Role::User => ("Вы", Color::Green),
                    Role::Assistant => ("Агент", Color::Cyan),
                };
                lines.push(Line::from(Span::styled(
                    format!("● {label}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )));
                let rendered = agent_skin().term_text(&entry.content).to_string();
                match rendered.into_text() {
                    Ok(text) => lines.extend(text.lines),
                    Err(_) => lines.push(Line::raw(entry.content.clone())),
                }
                lines.push(Line::raw(""));
            }
            if pending {
                lines.push(Line::from(Span::styled(
                    format!("{} Агент думает...", SPINNER_FRAMES[spinner_frame]),
                    Style::default().fg(Color::Magenta),
                )));
            }

            let history_area = chunks[1];
            let total_lines = lines.len() as u16;
            let visible = history_area.height.saturating_sub(2);
            if auto_scroll {
                scroll = total_lines.saturating_sub(visible);
            } else if let Some(target) = target_line {
                scroll = target.min(total_lines.saturating_sub(visible));
                scroll_to_message = None;
            } else {
                scroll = scroll.min(total_lines.saturating_sub(visible));
            }

            let history_widget = Paragraph::new(Text::from(lines))
                .block(Block::default().borders(Borders::ALL).title(" История "))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            f.render_widget(history_widget, history_area);

            let input_border_style = if focus == Focus::Input {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let input_widget = Paragraph::new(input.as_str())
                .style(Style::default().fg(Color::White))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(input_border_style)
                        .title(" Сообщение "),
                );
            f.render_widget(input_widget, chunks[2]);
            if !pending && focus == Focus::Input {
                f.set_cursor_position((
                    chunks[2].x + 1 + input.chars().count() as u16,
                    chunks[2].y + 1,
                ));
            }

            let help = Paragraph::new(Line::from(Span::styled(
                "Tab — переключить панель · Enter — отправить/выбрать чат · Ctrl+N — новый чат · Esc/Ctrl+C — выход",
                Style::default().fg(Color::DarkGray),
            )));
            f.render_widget(help, chunks[3]);
        })?;

        tokio::select! {
            _ = tick.tick() => {
                if pending {
                    spinner_frame = (spinner_frame + 1) % SPINNER_FRAMES.len();
                }
            }
            maybe_event = events.next() => {
                let Some(Ok(event)) = maybe_event else { continue };
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            break;
                        }
                        if key.code == KeyCode::Char('n') && key.modifiers.contains(KeyModifiers::CONTROL) {
                            if pending {
                                continue;
                            }
                            chats.insert(0, ChatSession::new());
                            active = 0;
                            auto_scroll = true;
                            focus = Focus::Input;
                            continue;
                        }
                        if key.code == KeyCode::Tab {
                            focus = match focus {
                                Focus::Input => Focus::Sidebar,
                                Focus::Sidebar => Focus::Input,
                            };
                            continue;
                        }
                        if pending {
                            continue;
                        }

                        if focus == Focus::Sidebar {
                            match key.code {
                                KeyCode::Up => {
                                    if active > 0 {
                                        active -= 1;
                                    }
                                }
                                KeyCode::Down => {
                                    if active + 1 < chats.len() {
                                        active += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    focus = Focus::Input;
                                    auto_scroll = true;
                                }
                                KeyCode::Esc => break,
                                _ => {}
                            }
                            continue;
                        }

                        match key.code {
                            KeyCode::Enter => {
                                let line = input.trim().to_string();
                                input.clear();
                                if line.is_empty() {
                                    continue;
                                }
                                if line == "exit" || line == "quit" {
                                    break;
                                }
                                chats[active].messages.push(Message { role: Role::User, content: line });
                                chats[active].touch();
                                let _ = chats::save_chat(&chats[active]);
                                pending = true;
                                auto_scroll = true;
                                scroll_to_message = None;

                                let agent = agent.clone();
                                let hist = chats[active].messages.clone();
                                let tx = tx.clone();
                                tokio::spawn(async move {
                                    let result = agent.ask(&hist).await;
                                    let _ = tx.send(ChatEvent::Response(result));
                                });
                            }
                            KeyCode::Esc => break,
                            KeyCode::Char(c) => input.push(c),
                            KeyCode::Backspace => {
                                input.pop();
                            }
                            KeyCode::Up => {
                                scroll = scroll.saturating_sub(1);
                                auto_scroll = false;
                            }
                            KeyCode::Down => {
                                scroll = scroll.saturating_add(1);
                            }
                            KeyCode::PageUp => {
                                scroll = scroll.saturating_sub(10);
                                auto_scroll = false;
                            }
                            KeyCode::PageDown => {
                                scroll = scroll.saturating_add(10);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            Some(chat_event) = rx.recv() => {
                match chat_event {
                    ChatEvent::Response(Ok(answer)) => {
                        chats[active].messages.push(Message { role: Role::Assistant, content: answer });
                        chats[active].touch();
                        let _ = chats::save_chat(&chats[active]);
                        pending = false;
                        auto_scroll = false;
                        scroll_to_message = Some(chats[active].messages.len() - 1);
                    }
                    ChatEvent::Response(Err(err)) => {
                        chats[active].messages.push(Message {
                            role: Role::Assistant,
                            content: format!("Ошибка: {err}"),
                        });
                        pending = false;
                        auto_scroll = false;
                        scroll_to_message = Some(chats[active].messages.len() - 1);
                    }
                }
            }
        }
    }

    Ok(())
}
