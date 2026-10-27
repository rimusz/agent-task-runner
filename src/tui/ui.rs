//! Ratatui chat host: transcript, composer, under-input status strip.
//!
//! Does not open the alternate screen for `atr ask`. Slash handling is
//! [`super::step_line`] — atr-local host commands plus provider proxy.

use super::history::InputHistory;
use super::proxy;
use super::slash;
use super::{
    collect_result, composer_path_line, composer_status_line, enqueue_turn, step_line,
    truncate_middle_end, turn_is_idle, Step, Workspace,
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
    proxy::begin_session_models_refresh(&shared, &key, &ws);
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
    slash_menu: Option<SlashMenu>,
    history: InputHistory,
}

struct SlashMenu {
    items: Vec<SlashRow>,
    state: ListState,
}

struct SlashRow {
    name: String,
    hint: String,
}

struct Picker {
    title: String,
    items: Vec<String>,
    all: Vec<String>,
    filter: String,
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
            "{}\nType a message. / for atr commands · ↑↓ history · /cli for the provider TUI.",
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
            slash_menu: None,
            history: InputHistory::default(),
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
        sync_model_picker(app, shared, key, ws);
        term.draw(|f| draw(f, app, shared, key, ws))?;
        if app.busy {
            scheduler::dispatch_key(shared, key).await?;
            ticks += 1;
            let snap = collect_result(shared, key);
            *app.last_assistant_mut() = snap.text.clone();
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
        return ctrl_c(app, shared, key);
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) && matches!(ev.code, KeyCode::Char('d')) {
        if app.input.is_empty() && app.picker.is_none() {
            return Ok(Some(LoopEnd::Quit(ExitCode::SUCCESS)));
        }
        return Ok(None);
    }
    if matches!(ev.code, KeyCode::Esc) {
        if app.picker.take().is_some() || app.slash_menu.take().is_some() {
            return Ok(None);
        }
        return Ok(None);
    }
    if app.picker.is_some() {
        return handle_picker(app, shared, key, ws, ev);
    }
    match ev.code {
        KeyCode::Enter => submit_or_queue(app, shared, key, ws, pin),
        KeyCode::Tab => {
            complete_slash(app);
            Ok(None)
        }
        KeyCode::Backspace => {
            delete_prev(app);
            refresh_slash_menu(app);
            Ok(None)
        }
        KeyCode::Delete => {
            delete_next(app);
            refresh_slash_menu(app);
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
        KeyCode::Up => {
            if !slash_nav(app, -1) {
                if let Some(line) = app.history.up(&app.input) {
                    set_input(app, &line);
                    refresh_slash_menu(app);
                }
            }
            Ok(None)
        }
        KeyCode::Down => {
            if !slash_nav(app, 1) {
                if let Some(line) = app.history.down() {
                    set_input(app, &line);
                    refresh_slash_menu(app);
                }
            }
            Ok(None)
        }
        KeyCode::PageUp => {
            app.follow = false;
            app.scroll = app.scroll.saturating_add(3);
            Ok(None)
        }
        KeyCode::PageDown => {
            if app.scroll <= 3 {
                app.scroll = 0;
                app.follow = true;
            } else {
                app.scroll = app.scroll.saturating_sub(3);
            }
            Ok(None)
        }
        KeyCode::Char('u') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
            set_input(app, "");
            refresh_slash_menu(app);
            Ok(None)
        }
        KeyCode::Char(c) => {
            insert_char(app, c);
            refresh_slash_menu(app);
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn ctrl_c(app: &mut App, shared: &Shared, key: &str) -> Result<Option<LoopEnd>> {
    // Ctrl+C always leaves atr. Best-effort cancel so a live CLI does
    // not outlive the process. Mid-turn cancel without quit is a later key.
    debug_assert_eq!(super::classify_interrupt(), "quit");
    if app.busy {
        let _ = shared.cancel(key, true);
        app.busy = false;
    }
    Ok(Some(LoopEnd::Quit(ExitCode::SUCCESS)))
}

fn submit_or_queue(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &Workspace,
    pin: Option<&str>,
) -> Result<Option<LoopEnd>> {
    if let Some(cmd) = take_slash_selection(app) {
        app.history.push(&cmd);
        set_input(app, "");
        return submit_line(app, shared, key, ws, pin, &cmd);
    }
    let line = app.input.clone();
    set_input(app, "");
    app.slash_menu = None;
    let trimmed = line.trim();
    if trimmed.is_empty() || slash::is_bare_slash(trimmed) {
        return Ok(None);
    }
    app.history.push(trimmed);
    if app.busy {
        match slash::parse_slash(trimmed) {
            Some(slash::Slash::Help)
            | Some(slash::Slash::Quit)
            | Some(slash::Slash::Status)
            | Some(slash::Slash::New)
            | Some(slash::Slash::Provider(_))
            | Some(slash::Slash::Model(_))
            | Some(slash::Slash::Cli) => {
                return submit_line(app, shared, key, ws, pin, trimmed);
            }
            _ => {
                enqueue_turn(shared, key, ws, trimmed, pin)?;
                app.push(Role::You, trimmed);
                app.push(Role::System, "queued · runs after this turn");
                return Ok(None);
            }
        }
    }
    submit_line(app, shared, key, ws, pin, trimmed)
}

fn set_input(app: &mut App, s: &str) {
    app.input = s.to_string();
    app.cursor = app.input.len();
}

fn refresh_slash_menu(app: &mut App) {
    let hits = slash::filter_host_commands(&app.input);
    if hits.is_empty() {
        app.slash_menu = None;
        return;
    }
    let keep = app
        .slash_menu
        .as_ref()
        .and_then(|m| m.state.selected())
        .and_then(|i| m_name_at(app, i));
    let items: Vec<SlashRow> = hits
        .into_iter()
        .map(|c| SlashRow {
            name: c.name.to_string(),
            hint: c.hint.to_string(),
        })
        .collect();
    let sel = keep
        .and_then(|n| items.iter().position(|r| r.name == n))
        .unwrap_or(0);
    let mut state = ListState::default();
    state.select(Some(sel));
    app.slash_menu = Some(SlashMenu { items, state });
}

fn m_name_at(app: &App, i: usize) -> Option<String> {
    app.slash_menu
        .as_ref()
        .and_then(|m| m.items.get(i).map(|r| r.name.clone()))
}

fn slash_nav(app: &mut App, delta: i32) -> bool {
    let Some(menu) = app.slash_menu.as_mut() else {
        return false;
    };
    if menu.items.is_empty() {
        return false;
    }
    let i = menu.state.selected().unwrap_or(0) as i32;
    let n = menu.items.len() as i32;
    let next = (i + delta).clamp(0, n - 1) as usize;
    menu.state.select(Some(next));
    true
}

fn take_slash_selection(app: &mut App) -> Option<String> {
    let menu = app.slash_menu.take()?;
    let i = menu.state.selected().unwrap_or(0);
    menu.items.get(i).map(|r| r.name.clone())
}

fn complete_slash(app: &mut App) {
    let Some(menu) = app.slash_menu.as_ref() else {
        return;
    };
    let i = menu.state.selected().unwrap_or(0);
    let Some(name) = menu.items.get(i).map(|r| r.name.clone()) else {
        return;
    };
    let with_space = matches!(name.as_str(), "/model" | "/provider");
    set_input(app, &if with_space { format!("{name} ") } else { name });
    refresh_slash_menu(app);
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
        KeyCode::Up | KeyCode::Char('k') if picker.filter.is_empty() => {
            let i = picker.state.selected().unwrap_or(0);
            picker.state.select(Some(i.saturating_sub(1)));
        }
        KeyCode::Down | KeyCode::Char('j') if picker.filter.is_empty() => {
            let i = picker.state.selected().unwrap_or(0);
            let n = picker.items.len().saturating_sub(1);
            picker.state.select(Some((i + 1).min(n)));
        }
        KeyCode::Up => {
            let i = picker.state.selected().unwrap_or(0);
            picker.state.select(Some(i.saturating_sub(1)));
        }
        KeyCode::Down => {
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
        KeyCode::Backspace => {
            picker.filter.pop();
            apply_picker_filter(picker);
        }
        KeyCode::Char(c) if c.is_ascii_digit() && picker.filter.is_empty() => {
            let n = c.to_digit(10).unwrap_or(0) as usize;
            if n >= 1 && n <= picker.items.len() {
                let name = picker.items[n - 1].clone();
                let kind = picker.kind;
                app.picker = None;
                picker_apply(app, shared, key, ws, kind, &name)?;
            }
        }
        KeyCode::Char(c) => {
            picker.filter.push(c);
            apply_picker_filter(picker);
        }
        _ => {}
    }
    Ok(None)
}

fn apply_picker_filter(picker: &mut Picker) {
    picker.items = slash::filter_names(&picker.all, &picker.filter);
    if picker.items.is_empty() {
        picker.state.select(None);
    } else {
        let i = picker.state.selected().unwrap_or(0);
        picker.state.select(Some(i.min(picker.items.len() - 1)));
    }
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
        PickerKind::Provider => super::apply_provider_pick(shared, key, ws, name)?,
        PickerKind::Model => super::apply_model_pick(shared, key, ws, name)?,
    };
    if let Step::Note(msg) = step {
        app.push(Role::System, msg);
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
            let items: Vec<String> = list.iter().map(|p| p.name.clone()).collect();
            app.picker = Some(Picker {
                title: "Provider  (Enter / type to find · Esc)".into(),
                items: items.clone(),
                all: items,
                filter: String::new(),
                state,
                kind: PickerKind::Provider,
            });
            Ok(None)
        }
        Step::OpenModelPicker => {
            // Fast sources + warm cache only. Never spawn a models CLI
            // here — every kind (claude / cursor / codex / grok / opencode)
            // must paint the overlay on the next frame.
            proxy::begin_session_models_refresh(shared, key, ws);
            let list = proxy::collect_models(shared, key, ws, proxy::CliFetch::Cached);
            let (kind, _, pname) = proxy::current_kind_cli(shared, key)
                .unwrap_or_else(|| ("?".into(), Default::default(), "none".into()));
            if list.is_empty() && proxy::models_argv(&kind).is_none() {
                app.push(Role::System, slash::model_empty_hint(&pname, &kind));
                return Ok(None);
            }
            let sel = list.iter().position(|m| m.current).unwrap_or(0);
            let mut state = ListState::default();
            state.select(Some(sel));
            let items: Vec<String> = list.iter().map(|m| m.name.clone()).collect();
            app.picker = Some(Picker {
                title: "Model  (Enter / type to find · Esc)".into(),
                items: items.clone(),
                all: items,
                filter: String::new(),
                state,
                kind: PickerKind::Model,
            });
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

fn sync_model_picker(app: &mut App, shared: &Shared, key: &str, ws: &Workspace) {
    let Some(picker) = app.picker.as_mut() else {
        return;
    };
    if !matches!(picker.kind, PickerKind::Model) {
        return;
    }
    let list = proxy::collect_models(shared, key, ws, proxy::CliFetch::Cached);
    let names: Vec<String> = list.iter().map(|m| m.name.clone()).collect();
    if names == picker.all {
        return;
    }
    let keep = picker
        .state
        .selected()
        .and_then(|i| picker.items.get(i).cloned());
    picker.all = names;
    apply_picker_filter(picker);
    if let Some(n) = keep {
        if let Some(i) = picker.items.iter().position(|x| *x == n) {
            picker.state.select(Some(i));
        }
    } else if let Some(i) = list.iter().position(|m| m.current) {
        let name = list[i].name.clone();
        if let Some(j) = picker.items.iter().position(|x| *x == name) {
            picker.state.select(Some(j));
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

fn draw(f: &mut Frame, app: &App, shared: &Shared, key: &str, ws: &Workspace) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(area);

    let title = format!(" atr tui  {key} ");
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

    let hint = composer_hint(app);
    let input = Paragraph::new(format!("› {}", app.input)).block(
        Block::default()
            .borders(Borders::TOP)
            .title(hint)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(input, chunks[2]);

    let status = composer_status_line(shared, key);
    let path = truncate_middle_end(
        &composer_path_line(&proxy::session_cwd(shared, key, ws)),
        chunks[3].width.max(1) as usize,
    );
    let strip = vec![
        Line::from(Span::styled(status, Style::default().fg(Color::Gray))),
        Line::from(Span::styled(path, Style::default().fg(Color::DarkGray))),
    ];
    f.render_widget(Paragraph::new(strip), chunks[3]);

    if let Some(menu) = &app.slash_menu {
        draw_slash_menu(f, chunks[1], chunks[2], menu);
    }

    if let Some(picker) = &app.picker {
        let pop = centered(chunks[1], picker.items.len() as u16 + 2);
        f.render_widget(Clear, pop);
        let items: Vec<ListItem> = picker
            .items
            .iter()
            .enumerate()
            .map(|(i, n)| ListItem::new(format!(" {:>2}  {n}", i + 1)))
            .collect();
        let title = if picker.filter.is_empty() {
            picker.title.clone()
        } else {
            format!("{}  /{}", picker.title, picker.filter)
        };
        let list = List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        let mut state = picker.state.clone();
        f.render_stateful_widget(list, pop, &mut state);
    }

    let cur_x = (chunks[2].x + 2 + visible_width(&app.input[..app.cursor.min(app.input.len())]))
        .min(chunks[2].right().saturating_sub(1));
    let cur_y = chunks[2].y + 1;
    f.set_cursor_position((cur_x, cur_y));
}

fn composer_hint(app: &App) -> String {
    if app.picker.is_some() {
        return "↵ pick  type to find  Esc".into();
    }
    if app.slash_menu.is_some() {
        return "↵ apply  Tab complete  ↑↓  Esc".into();
    }
    if app.busy {
        return "↵ queue  ^C quit  PgUp scroll".into();
    }
    if !app.follow {
        return "↑↓ history  PgDn latest  / commands".into();
    }
    "↵ send  ↑↓ history  / commands  PgUp scroll  ^C quit".into()
}

fn draw_slash_menu(f: &mut Frame, body: Rect, input: Rect, menu: &SlashMenu) {
    let h = (menu.items.len() as u16 + 2)
        .min(body.height.saturating_sub(1))
        .max(3);
    let y = input.y.saturating_sub(h);
    let pop = Rect {
        x: input.x,
        y: y.max(body.y),
        width: input.width,
        height: h,
    };
    f.render_widget(Clear, pop);
    let items: Vec<ListItem> = menu
        .items
        .iter()
        .map(|r| ListItem::new(format!("  {:<10}  {}", r.name, r.hint)))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .title(" commands ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = menu.state.clone();
    f.render_stateful_widget(list, pop, &mut state);
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
