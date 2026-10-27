//! Ratatui chat host: transcript, status footer, input, provider picker.
//!
//! Does not open the alternate screen for `atr ask`. Slash handling is
//! [`super::step_line`] — atr-local host commands plus provider proxy.

use super::proxy;
use super::slash;
use super::{
    collect_result, daemon_line, enqueue_turn, footer_line, step_line, turn_is_idle, Step,
    Workspace,
};
use crate::app::Shared;
use crate::scheduler;
use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, Stdout};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

enum LoopEnd {
    Quit(ExitCode),
    Native,
}

pub(crate) async fn run(
    shared: Arc<Shared>,
    key: String,
    ws: Workspace,
    pin: Option<String>,
) -> Result<ExitCode> {
    let mut app = App::new(&shared, &key);
    loop {
        let mut term = setup()?;
        let end = event_loop(&mut term, &shared, &key, &ws, pin.as_deref(), &mut app).await;
        restore()?;
        match end? {
            LoopEnd::Quit(code) => return Ok(code),
            LoopEnd::Native => {
                let msg = match proxy::run_native_cli(&shared, &key, &ws) {
                    Ok(m) => m,
                    Err(e) => format!("{e:#}"),
                };
                app.push(Role::System, msg);
                app.status = footer_line(&shared, &key);
            }
        }
    }
}

fn setup() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture).context("enter alt screen")?;
    let backend = CrosstermBackend::new(out);
    Terminal::new(backend).context("ratatui terminal")
}

fn restore() -> Result<()> {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(
        out,
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    Ok(())
}

struct App {
    messages: Vec<Msg>,
    input: String,
    cursor: usize,
    scroll: u16,
    follow: bool,
    busy: bool,
    picker: Option<Picker>,
    status: String,
}

struct Picker {
    title: String,
    items: Vec<String>,
    state: ListState,
    kind: PickerKind,
}

#[derive(Clone, Copy)]
enum PickerKind {
    Provider,
    Model,
}

#[derive(Clone, Copy)]
enum Role {
    You,
    Assistant,
    System,
    Error,
}

struct Msg {
    role: Role,
    text: String,
}

impl App {
    fn new(shared: &Shared, key: &str) -> Self {
        let welcome = format!(
            "{}\nType a message. /help lists atr commands; other /slashes go to the provider.",
            super::banner_text(shared, key).trim_end()
        );
        Self {
            messages: vec![Msg {
                role: Role::System,
                text: welcome,
            }],
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow: true,
            busy: false,
            picker: None,
            status: footer_line(shared, key),
        }
    }

    fn push(&mut self, role: Role, text: impl Into<String>) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        self.messages.push(Msg { role, text });
        self.follow = true;
        self.scroll = 0;
    }

    fn last_assistant_mut(&mut self) -> &mut String {
        if !matches!(self.messages.last().map(|m| m.role), Some(Role::Assistant)) {
            self.messages.push(Msg {
                role: Role::Assistant,
                text: String::new(),
            });
        }
        &mut self.messages.last_mut().unwrap().text
    }
}

async fn event_loop(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Shared>,
    key: &str,
    ws: &Workspace,
    pin: Option<&str>,
    app: &mut App,
) -> Result<LoopEnd> {
    let mut ticks = 0u32;
    loop {
        term.draw(|f| draw(f, app, shared, key))?;
        if app.busy {
            scheduler::dispatch_key(shared, key).await?;
            ticks += 1;
            let snap = collect_result(shared, key);
            *app.last_assistant_mut() = snap.text.clone();
            app.status = snap.footer.clone();
            if turn_is_idle(shared, key)? {
                app.busy = false;
                if !snap.ok {
                    app.push(Role::Error, "turn failed");
                }
            } else if ticks >= 3 {
                if let Err(e) = super::check_provider_available(shared, key) {
                    app.busy = false;
                    app.push(Role::Error, format!("{e:#}"));
                }
            }
        }

        if !event::poll(Duration::from_millis(40))? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if key_down(&k) => match handle_key(app, shared, key, ws, pin, k)? {
                Some(LoopEnd::Quit(code)) => return Ok(LoopEnd::Quit(code)),
                Some(LoopEnd::Native) => return Ok(LoopEnd::Native),
                None => {}
            },
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => {
                    app.follow = false;
                    app.scroll = app.scroll.saturating_add(1);
                }
                MouseEventKind::ScrollDown => {
                    if app.scroll == 0 {
                        app.follow = true;
                    } else {
                        app.scroll = app.scroll.saturating_sub(1);
                    }
                }
                _ => {}
            },
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn key_down(k: &KeyEvent) -> bool {
    !matches!(k.kind, KeyEventKind::Release)
}

fn handle_key(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    pin: Option<&str>,
    ev: KeyEvent,
) -> Result<Option<LoopEnd>> {
    if ev.modifiers.contains(KeyModifiers::CONTROL) && matches!(ev.code, KeyCode::Char('c')) {
        return Ok(Some(LoopEnd::Quit(ExitCode::SUCCESS)));
    }
    if app.picker.is_some() {
        return handle_picker(app, shared, key, ws, ev);
    }
    match ev.code {
        KeyCode::Enter => {
            if app.busy {
                return Ok(None);
            }
            let line = app.input.clone();
            app.input.clear();
            app.cursor = 0;
            if line.trim().is_empty() {
                return Ok(None);
            }
            submit_line(app, shared, key, ws, pin, &line)
        }
        KeyCode::Backspace => {
            delete_prev(app);
            Ok(None)
        }
        KeyCode::Delete => {
            delete_next(app);
            Ok(None)
        }
        KeyCode::Left => {
            move_left(app);
            Ok(None)
        }
        KeyCode::Right => {
            move_right(app);
            Ok(None)
        }
        KeyCode::Home => {
            app.cursor = 0;
            Ok(None)
        }
        KeyCode::End => {
            app.cursor = app.input.len();
            Ok(None)
        }
        KeyCode::Up | KeyCode::PageUp => {
            app.follow = false;
            app.scroll = app.scroll.saturating_add(3);
            Ok(None)
        }
        KeyCode::Down | KeyCode::PageDown => {
            if app.scroll <= 3 {
                app.scroll = 0;
                app.follow = true;
            } else {
                app.scroll = app.scroll.saturating_sub(3);
            }
            Ok(None)
        }
        KeyCode::Char('u') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
            app.input.clear();
            app.cursor = 0;
            Ok(None)
        }
        KeyCode::Char(c) => {
            insert_char(app, c);
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn handle_picker(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    ev: KeyEvent,
) -> Result<Option<LoopEnd>> {
    let Some(picker) = app.picker.as_mut() else {
        return Ok(None);
    };
    match ev.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.picker = None;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = picker.state.selected().unwrap_or(0);
            picker.state.select(Some(i.saturating_sub(1)));
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let i = picker.state.selected().unwrap_or(0);
            let n = picker.items.len().saturating_sub(1);
            picker.state.select(Some((i + 1).min(n)));
        }
        KeyCode::Enter => {
            let i = picker.state.selected().unwrap_or(0);
            if let Some(name) = picker.items.get(i).cloned() {
                let kind = picker.kind;
                app.picker = None;
                picker_apply(app, shared, key, ws, kind, &name)?;
            }
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            let n = c.to_digit(10).unwrap_or(0) as usize;
            if n >= 1 && n <= picker.items.len() {
                let name = picker.items[n - 1].clone();
                let kind = picker.kind;
                app.picker = None;
                picker_apply(app, shared, key, ws, kind, &name)?;
            }
        }
        _ => {}
    }
    Ok(None)
}

fn picker_apply(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    kind: PickerKind,
    name: &str,
) -> Result<()> {
    let step = match kind {
        PickerKind::Provider => super::apply_provider_pick(shared, key, name)?,
        PickerKind::Model => super::apply_model_pick(shared, key, ws, name)?,
    };
    if let Step::Note(msg) = step {
        app.push(Role::System, msg);
        app.status = footer_line(shared, key);
    }
    Ok(())
}

fn submit_line(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    pin: Option<&str>,
    line: &str,
) -> Result<Option<LoopEnd>> {
    app.push(Role::You, line.to_string());
    apply_step(
        app,
        shared,
        key,
        ws,
        pin,
        step_line(shared, key, ws, pin, line)?,
    )
}

fn apply_step(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    pin: Option<&str>,
    step: Step,
) -> Result<Option<LoopEnd>> {
    match step {
        Step::Quit => Ok(Some(LoopEnd::Quit(ExitCode::SUCCESS))),
        Step::Note(msg) => {
            app.push(Role::System, msg);
            app.status = footer_line(shared, key);
            Ok(None)
        }
        Step::OpenProviderPicker => {
            let list = slash::list_providers(shared, key);
            if list.is_empty() {
                app.push(Role::System, slash::provider_pick_usage(&list));
                return Ok(None);
            }
            let sel = list.iter().position(|p| p.current).unwrap_or(0);
            let mut state = ListState::default();
            state.select(Some(sel));
            app.picker = Some(Picker {
                title: "Provider  (Enter / number · Esc cancel)".into(),
                items: list.iter().map(|p| p.name.clone()).collect(),
                state,
                kind: PickerKind::Provider,
            });
            app.push(Role::System, slash::format_provider_list(&list));
            Ok(None)
        }
        Step::OpenModelPicker => {
            let list = proxy::collect_models(shared, key, ws);
            let (kind, _, pname) = proxy::current_kind_cli(shared, key)
                .unwrap_or_else(|| ("?".into(), Default::default(), "none".into()));
            if list.is_empty() {
                app.push(Role::System, slash::model_empty_hint(&pname, &kind));
                return Ok(None);
            }
            let sel = list.iter().position(|m| m.current).unwrap_or(0);
            let mut state = ListState::default();
            state.select(Some(sel));
            app.picker = Some(Picker {
                title: "Model  (Enter / number · Esc cancel)".into(),
                items: list.iter().map(|m| m.name.clone()).collect(),
                state,
                kind: PickerKind::Model,
            });
            app.push(Role::System, slash::model_pick_usage(&pname, &kind, &list));
            Ok(None)
        }
        Step::NativeCli => Ok(Some(LoopEnd::Native)),
        Step::Turn(text) => {
            enqueue_turn(shared, key, ws, &text, pin)?;
            app.push(Role::Assistant, String::new());
            app.busy = true;
            Ok(None)
        }
    }
}

fn insert_char(app: &mut App, c: char) {
    app.input.insert(app.cursor, c);
    app.cursor += c.len_utf8();
}

fn delete_prev(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    let prev = app.input[..app.cursor]
        .chars()
        .next_back()
        .map(|c| c.len_utf8())
        .unwrap_or(1);
    let start = app.cursor - prev;
    app.input.replace_range(start..app.cursor, "");
    app.cursor = start;
}

fn delete_next(app: &mut App) {
    if app.cursor >= app.input.len() {
        return;
    }
    let next = app.input[app.cursor..]
        .chars()
        .next()
        .map(|c| c.len_utf8())
        .unwrap_or(1);
    let end = app.cursor + next;
    app.input.replace_range(app.cursor..end, "");
}

fn move_left(app: &mut App) {
    if app.cursor == 0 {
        return;
    }
    let prev = app.input[..app.cursor]
        .chars()
        .next_back()
        .map(|c| c.len_utf8())
        .unwrap_or(1);
    app.cursor -= prev;
}

fn move_right(app: &mut App) {
    if app.cursor >= app.input.len() {
        return;
    }
    let next = app.input[app.cursor..]
        .chars()
        .next()
        .map(|c| c.len_utf8())
        .unwrap_or(1);
    app.cursor += next;
}

fn draw(f: &mut Frame, app: &App, shared: &Shared, key: &str) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(3),
        ])
        .split(area);

    let title = format!(" atr tui  {} ", footer_line(shared, key));
    f.render_widget(
        Paragraph::new(title).style(Style::default().add_modifier(Modifier::BOLD)),
        chunks[0],
    );

    let body = transcript_lines(app);
    let width = chunks[1].width.max(1);
    let height = chunks[1].height.max(1);
    let para = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .scroll(scroll_pair(app, height, width));
    f.render_widget(para, chunks[1]);

    let daemon = daemon_line(&shared.config_path);
    let foot = format!("{}\n{daemon}", app.status);
    f.render_widget(
        Paragraph::new(foot).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    let hint = if app.busy {
        "working…  ↑↓ scroll  Ctrl-C quit"
    } else {
        "message or /help   ↑↓ scroll  Ctrl-C quit"
    };
    let input = Paragraph::new(format!("› {}", app.input)).block(
        Block::default()
            .borders(Borders::TOP)
            .title(hint)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(input, chunks[3]);

    if let Some(picker) = &app.picker {
        let pop = centered(chunks[1], picker.items.len() as u16 + 2);
        f.render_widget(Clear, pop);
        let items: Vec<ListItem> = picker
            .items
            .iter()
            .enumerate()
            .map(|(i, n)| ListItem::new(format!(" {:>2}  {n}", i + 1)))
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .title(picker.title.as_str())
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        let mut state = picker.state.clone();
        f.render_stateful_widget(list, pop, &mut state);
    }

    let cur_x = (chunks[3].x + 2 + visible_width(&app.input[..app.cursor.min(app.input.len())]))
        .min(chunks[3].right().saturating_sub(1));
    let cur_y = chunks[3].y + 1;
    f.set_cursor_position((cur_x, cur_y));
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, m) in app.messages.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        let (label, color) = match m.role {
            Role::You => ("you", Color::Cyan),
            Role::Assistant => ("atr", Color::Green),
            Role::System => ("atr", Color::Yellow),
            Role::Error => ("err", Color::Red),
        };
        lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
        for row in m.text.split('\n') {
            lines.push(Line::from(row.to_string()));
        }
    }
    lines
}

fn scroll_pair(app: &App, height: u16, width: u16) -> (u16, u16) {
    let n = wrapped_count(app, width);
    let max = n.saturating_sub(height as usize) as u16;
    if app.follow {
        (max, 0)
    } else {
        (max.saturating_sub(app.scroll), 0)
    }
}

fn wrapped_count(app: &App, width: u16) -> usize {
    let w = width.max(1) as usize;
    let mut n = 0usize;
    for (i, m) in app.messages.iter().enumerate() {
        if i > 0 {
            n += 1;
        }
        n += 1;
        for row in m.text.split('\n') {
            let cols = row.chars().count().max(1);
            n += cols.div_ceil(w);
        }
    }
    n
}

fn visible_width(s: &str) -> u16 {
    s.chars().count().min(u16::MAX as usize) as u16
}

fn centered(area: Rect, height: u16) -> Rect {
    let h = height.min(area.height.saturating_sub(1)).max(3);
    let w = area.width.saturating_sub(4).clamp(20, 48);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}
