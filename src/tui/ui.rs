//! Ratatui chat host: transcript, composer, under-input status strip.
//!
//! The TUI is a provider proxy, not a second personality. Mouse capture
//! stays off so the terminal can select/copy; scroll is PgUp/PgDn.
//! Does not open the alternate screen for `atr ask`. Slash handling is
//! [`super::step_line`] — atr-local host commands plus provider proxy.

use super::history::InputHistory;
use super::proxy;
use super::slash;
use super::{
    busy_enter_action, collect_result, composer_status_line, enqueue_turn, esc_action,
    park_busy_line, step_line, truncate_middle_end, turn_is_idle, workspace_chrome_line, BusyEnter,
    EscAction, PendingTurns, Step, Workspace,
};
use crate::app::Shared;
use crate::scheduler;
use crate::tz::DisplayTz;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
    let _quiet = crate::logrotate::TuiConsoleQuiet::enter();
    let mut ws = ws;
    let mut app = App::new(&shared, &key);
    proxy::begin_session_models_refresh(&shared, &key, &ws);
    loop {
        let mut term = setup()?;
        let end = event_loop(&mut term, &shared, &key, &mut ws, pin.as_deref(), &mut app).await;
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
    // No mouse capture: the terminal keeps native selection for copy/paste.
    // Scroll is keyboard-only (PgUp/PgDn). Do not EnableMouseCapture.
    execute!(out, EnterAlternateScreen).context("enter alt screen")?;
    let backend = CrosstermBackend::new(out);
    Terminal::new(backend).context("ratatui terminal")
}

fn restore() -> Result<()> {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen, crossterm::cursor::Show);
    Ok(())
}

struct App {
    messages: Vec<Msg>,
    input: String,
    cursor: usize,
    scroll: u16,
    follow: bool,
    busy: bool,
    /// Esc stopped the live turn; don't report it as "turn failed".
    interrupted: bool,
    pending: PendingTurns,
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
    Effort,
    Context,
    Repo,
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
    at: DateTime<Utc>,
}

impl App {
    fn new(shared: &Shared, key: &str) -> Self {
        let welcome = super::banner_text(shared, key).trim_end().to_string();
        Self {
            messages: vec![Msg {
                role: Role::System,
                text: welcome,
                at: Utc::now(),
            }],
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow: true,
            busy: false,
            interrupted: false,
            pending: PendingTurns::default(),
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
        self.messages.push(Msg {
            role,
            text,
            at: Utc::now(),
        });
        self.follow = true;
        self.scroll = 0;
    }

    fn last_assistant_mut(&mut self) -> &mut String {
        if !matches!(self.messages.last().map(|m| m.role), Some(Role::Assistant)) {
            self.messages.push(Msg {
                role: Role::Assistant,
                text: String::new(),
                at: Utc::now(),
            });
        }
        &mut self.messages.last_mut().unwrap().text
    }
}

async fn event_loop(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    shared: &Arc<Shared>,
    key: &str,
    ws: &mut Workspace,
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
                ticks = 0;
                if !snap.ok && !std::mem::take(&mut app.interrupted) {
                    app.push(Role::Error, "turn failed");
                }
                app.interrupted = false;
                if let Some(end) = flush_parked(app, shared, key, ws, pin)? {
                    return Ok(end);
                }
            } else if ticks >= 3 {
                if let Err(e) = super::check_provider_available(shared, key) {
                    app.busy = false;
                    ticks = 0;
                    app.push(Role::Error, format!("{e:#}"));
                    if let Some(end) = flush_parked(app, shared, key, ws, pin)? {
                        return Ok(end);
                    }
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
    ws: &mut Workspace,
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
        let overlay = app.picker.is_some() || app.slash_menu.is_some();
        match esc_action(overlay, app.busy, app.pending.len()) {
            EscAction::Dismiss => {
                app.picker = None;
                app.slash_menu = None;
            }
            EscAction::Ignore => {}
            EscAction::Interrupt => esc_interrupt(app, shared, key, ws, pin, None)?,
            EscAction::InterruptWithParked => {
                let msg = app.pending.drain_joined();
                esc_interrupt(app, shared, key, ws, pin, msg)?;
            }
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
            let page = page_rows(term_rows());
            apply_page_up(app, page);
            Ok(None)
        }
        KeyCode::PageDown => {
            let page = page_rows(term_rows());
            apply_page_down(app, page);
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

/// Esc while busy: send any parked lines as the next message, then stop the
/// live turn so the scheduler resumes the same session with them.
fn esc_interrupt(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
    pin: Option<&str>,
    message: Option<String>,
) -> Result<()> {
    if let Some(m) = &message {
        enqueue_turn(shared, key, ws, m, pin)?;
        app.push(Role::You, m.clone());
    }
    match shared.interrupt_live(key)? {
        crate::InterruptOutcome::Interrupted => {
            app.interrupted = true;
            if message.is_some() {
                app.push(Role::System, "interrupted · resuming with your message");
                app.push(Role::Assistant, " ");
            } else {
                app.push(Role::System, "interrupted · type to continue");
            }
        }
        crate::InterruptOutcome::NotResumable => app.push(
            Role::System,
            "this provider cannot resume a session; not interrupted (message waits for this turn)",
        ),
        crate::InterruptOutcome::NotRunning => {}
    }
    Ok(())
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

fn flush_parked(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
    pin: Option<&str>,
) -> Result<Option<LoopEnd>> {
    let Some(next) = app.pending.pop() else {
        return Ok(None);
    };
    submit_line(app, shared, key, ws, pin, &next)
}

fn submit_or_queue(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
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
    if slash::is_bare_slash(trimmed) || !super::is_turn_text(trimmed) {
        return Ok(None);
    }
    app.history.push(trimmed);
    if app.busy {
        match busy_enter_action(trimmed) {
            BusyEnter::Host => return submit_line(app, shared, key, ws, pin, trimmed),
            BusyEnter::Queue => {
                park_busy_line(&mut app.pending, trimmed);
                return Ok(None);
            }
            BusyEnter::Ignore => return Ok(None),
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
    let with_space = matches!(
        name.as_str(),
        "/model" | "/provider" | "/repo" | "/effort" | "/context"
    );
    set_input(app, &if with_space { format!("{name} ") } else { name });
    refresh_slash_menu(app);
}

fn handle_picker(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
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
    picker.items = match picker.kind {
        PickerKind::Repo => slash::filter_repo_items(&picker.all, &picker.filter),
        PickerKind::Provider | PickerKind::Model | PickerKind::Effort | PickerKind::Context => {
            slash::filter_names(&picker.all, &picker.filter)
        }
    };
    if picker.items.is_empty() {
        picker.state.select(None);
    } else {
        let i = picker.state.selected().unwrap_or(0);
        picker.state.select(Some(i.min(picker.items.len() - 1)));
    }
}

fn picker_apply(
    _app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
    kind: PickerKind,
    name: &str,
) -> Result<()> {
    // Overlay pick: strip already shows the new provider/model/repo.
    // Do not push You/System confirmations into the transcript.
    let _step = match kind {
        PickerKind::Provider => super::apply_provider_pick(shared, key, ws, name)?,
        PickerKind::Model => super::apply_model_pick(shared, key, ws, name)?,
        PickerKind::Effort => {
            super::apply_knob_pick(shared, key, super::knobs::Knob::Effort, name)?
        }
        PickerKind::Context => {
            super::apply_knob_pick(shared, key, super::knobs::Knob::Context, name)?
        }
        PickerKind::Repo => super::apply_repo_pick(shared, key, ws, name)?,
    };
    Ok(())
}

fn submit_line(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
    pin: Option<&str>,
    line: &str,
) -> Result<Option<LoopEnd>> {
    let quiet = slash::quiet_transcript(line);
    if !quiet {
        app.push(Role::You, line.to_string());
    }
    let step = step_line(shared, key, ws, pin, line)?;
    apply_step(app, shared, key, ws, pin, quiet, step)
}

fn apply_step(
    app: &mut App,
    shared: &Shared,
    key: &str,
    ws: &mut Workspace,
    pin: Option<&str>,
    quiet: bool,
    step: Step,
) -> Result<Option<LoopEnd>> {
    match step {
        Step::Quit => Ok(Some(LoopEnd::Quit(ExitCode::SUCCESS))),
        Step::Note(msg) => {
            if !quiet {
                app.push(Role::System, msg);
            }
            Ok(None)
        }
        Step::OpenProviderPicker => {
            let list = slash::list_providers(shared, key);
            if list.is_empty() {
                if !quiet {
                    app.push(Role::System, slash::provider_pick_usage(&list));
                }
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
                if !quiet {
                    app.push(Role::System, slash::model_empty_hint(&pname, &kind));
                }
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
        Step::OpenEffortPicker => {
            let (kind, _) = super::proxy::current_kind_cli(shared, key)
                .map(|(k, _, n)| (k, n))
                .unwrap_or_else(|| ("?".into(), "none".into()));
            if super::knobs::values(super::knobs::Knob::Effort, &kind).is_none() {
                if !quiet {
                    app.push(
                        Role::System,
                        super::knobs::unsupported_note(super::knobs::Knob::Effort, &kind),
                    );
                }
                return Ok(None);
            }
            open_knob_picker(app, super::knobs::Knob::Effort, &kind);
            Ok(None)
        }
        Step::OpenContextPicker => {
            let (kind, _) = super::proxy::current_kind_cli(shared, key)
                .map(|(k, _, n)| (k, n))
                .unwrap_or_else(|| ("?".into(), "none".into()));
            if super::knobs::values(super::knobs::Knob::Context, &kind).is_none() {
                if !quiet {
                    app.push(
                        Role::System,
                        super::knobs::unsupported_note(super::knobs::Knob::Context, &kind),
                    );
                }
                return Ok(None);
            }
            open_knob_picker(app, super::knobs::Knob::Context, &kind);
            Ok(None)
        }
        Step::OpenRepoPicker => {
            let list = super::repo_choices(shared, ws);
            if list.is_empty() {
                if !quiet {
                    app.push(Role::System, super::slash::repo_pick_usage(&list));
                }
                return Ok(None);
            }
            let sel = list.iter().position(|p| p.current).unwrap_or(0);
            let mut state = ListState::default();
            state.select(Some(sel));
            let items: Vec<String> = list.iter().map(|p| p.token.clone()).collect();
            app.picker = Some(Picker {
                title: "Repo  (Enter / type to find · Esc)".into(),
                items: items.clone(),
                all: items,
                filter: String::new(),
                state,
                kind: PickerKind::Repo,
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

fn open_knob_picker(app: &mut App, knob: super::knobs::Knob, kind: &str) {
    let items = super::knobs::picker_items(knob, kind);
    let mut state = ListState::default();
    state.select(Some(0));
    let title = match knob {
        super::knobs::Knob::Effort => "Effort  (Enter / type to find · Esc)",
        super::knobs::Knob::Context => "Context  (Enter / type to find · Esc)",
    };
    app.picker = Some(Picker {
        title: title.into(),
        items: items.clone(),
        all: items,
        filter: String::new(),
        state,
        kind: match knob {
            super::knobs::Knob::Effort => PickerKind::Effort,
            super::knobs::Knob::Context => PickerKind::Context,
        },
    });
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
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .split(area);

    let title = super::session_title(key);
    f.render_widget(
        Paragraph::new(title).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        chunks[0],
    );

    let width = chunks[1].width.max(1);
    let height = chunks[1].height.max(1);
    let tz = shared.config.read().unwrap().display_timezone.clone();
    let body = transcript_lines(app, width, &tz);
    // Lines are already wrapped to `width`; wrap stays on as a safety net.
    let para = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .scroll(scroll_pair(app, height, width, &tz));
    f.render_widget(para, chunks[1]);

    f.render_widget(Paragraph::new(divider_line(app, width)), chunks[2]);

    let hint = composer_hint(app);
    let well = Style::default().bg(Color::Rgb(18, 18, 22));
    let input = Paragraph::new(vec![
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        Line::from(vec![
            Span::styled("› ".to_string(), Style::default().fg(Color::Cyan)),
            Span::raw(app.input.clone()),
        ]),
    ])
    .style(well);
    f.render_widget(input, chunks[3]);

    let status = composer_status_line(shared, key);
    let workspace = truncate_middle_end(
        &workspace_chrome_line(shared, ws),
        chunks[4].width.max(1) as usize,
    );
    let strip = vec![
        Line::from(Span::styled(status, Style::default().fg(Color::Gray))),
        Line::from(Span::styled(
            workspace,
            Style::default().fg(Color::DarkGray),
        )),
    ];
    f.render_widget(Paragraph::new(strip), chunks[4]);

    if let Some(menu) = &app.slash_menu {
        draw_slash_menu(f, chunks[1], chunks[3], menu);
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
        let title = slash::picker_filter_title(&picker.title, &picker.filter);
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

    let cur_x = (chunks[3].x + 2 + visible_width(&app.input[..app.cursor.min(app.input.len())]))
        .min(chunks[3].right().saturating_sub(1));
    let cur_y = chunks[3].y + 1;
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
        return if app.pending.is_empty() {
            "↵ queue  Esc interrupt  ^C quit  PgUp scroll".into()
        } else {
            format!(
                "↵ queue · {} waiting  Esc send now  ^C quit",
                app.pending.len()
            )
        };
    }
    if !app.follow {
        return "↑ scrolled  PgUp/PgDn  PgDn latest  / commands".into();
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

fn transcript_lines(app: &App, width: u16, tz: &DisplayTz) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut prev: Option<Role> = None;
    for m in &app.messages {
        if m.text.is_empty() {
            continue;
        }
        match m.role {
            Role::You => {
                if prev.is_some() {
                    lines.push(Line::from(""));
                }
                lines.extend(user_turn_lines(&m.text, width, &tz.format_ampm(m.at)));
            }
            Role::Assistant => {
                lines.extend(assistant_lines(&m.text, width));
            }
            Role::System => {
                if prev.is_some() {
                    lines.push(Line::from(""));
                }
                lines.extend(styled_wrap(
                    &m.text,
                    width,
                    Style::default().fg(Color::DarkGray),
                ));
            }
            Role::Error => {
                if prev.is_some() {
                    lines.push(Line::from(""));
                }
                lines.extend(styled_wrap(&m.text, width, Style::default().fg(Color::Red)));
            }
        }
        prev = Some(m.role);
    }
    lines
}

fn user_turn_lines(text: &str, width: u16, clock: &str) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut rows = text.split('\n');
    let first = rows.next().unwrap_or("");
    let (head, rest) = split_user_head(first, width, clock);
    let mut out = vec![user_first_line(&head, width, clock)];
    if !rest.is_empty() {
        out.extend(wrap_prefixed("  ", &rest, width));
    }
    for row in rows {
        out.extend(wrap_prefixed("  ", row, width));
    }
    out
}

fn wrap_prefixed(prefix: &str, text: &str, width: usize) -> Vec<Line<'static>> {
    let w = width.max(1);
    let indent = prefix.chars().count();
    let inner = w.saturating_sub(indent).max(1);
    wrap_cols(text, inner)
        .into_iter()
        .map(|piece| Line::from(format!("{prefix}{piece}")))
        .collect()
}

fn split_user_head(first: &str, width: usize, clock: &str) -> (String, String) {
    let prefix_w: usize = 2;
    let clock_w = clock.chars().count();
    let budget = width.saturating_sub(prefix_w.saturating_add(clock_w).saturating_add(1));
    if budget == 0 || first.chars().count() <= budget {
        return (first.to_string(), String::new());
    }
    let head: String = first.chars().take(budget).collect();
    let rest: String = first.chars().skip(budget).collect();
    (head, rest)
}

fn user_first_line(text: &str, width: usize, clock: &str) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let bar = Style::default().bg(Color::Rgb(28, 28, 32));
    if width < 8 {
        return Line::from(vec![
            Span::styled("> ".to_string(), dim),
            Span::raw(text.to_string()),
        ])
        .style(bar);
    }
    let clock_w = clock.chars().count();
    let used = 2 + text.chars().count();
    let pad = width.saturating_sub(used.saturating_add(clock_w));
    Line::from(vec![
        Span::styled("> ".to_string(), dim),
        Span::raw(text.to_string()),
        Span::raw(" ".repeat(pad)),
        Span::styled(clock.to_string(), dim),
    ])
    .style(bar)
}

#[cfg(test)]
fn line_plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn styled_wrap(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    let w = width.max(1) as usize;
    let mut out = Vec::new();
    for row in text.split('\n') {
        for piece in wrap_cols(row, w) {
            out.push(Line::from(Span::styled(piece, style)));
        }
    }
    out
}

fn assistant_lines(text: &str, width: u16) -> Vec<Line<'static>> {
    let w = width.max(1) as usize;
    let mut out = Vec::new();
    for row in text.split('\n') {
        let style = md_row_style(row);
        for (i, piece) in wrap_cols(row, w).into_iter().enumerate() {
            let st = if i == 0 { style } else { Style::default() };
            out.push(Line::from(Span::styled(piece, st)));
        }
    }
    out
}

fn md_row_style(row: &str) -> Style {
    let t = row.trim_start();
    if t.starts_with("### ") {
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else if t.starts_with("## ") || t.starts_with("# ") {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if t.starts_with("- ") || t.starts_with("* ") {
        Style::default().fg(Color::Gray)
    } else {
        Style::default()
    }
}

fn wrap_cols(s: &str, width: usize) -> Vec<String> {
    let w = width.max(1);
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut n = 0;
    for ch in s.chars() {
        if n == w {
            out.push(std::mem::take(&mut cur));
            n = 0;
        }
        cur.push(ch);
        n += 1;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn divider_line(app: &App, width: u16) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let w = width.max(1) as usize;
    if app.follow {
        return Line::from(Span::styled("─".repeat(w), dim));
    }
    let tag = " ↑ scrolled · PgDn latest ";
    let tag_w = tag.chars().count();
    if w <= tag_w {
        return Line::from(Span::styled(tag.chars().take(w).collect::<String>(), dim));
    }
    let left = (w.saturating_sub(tag_w)) / 2;
    let right = w.saturating_sub(left + tag_w);
    Line::from(vec![
        Span::styled("─".repeat(left), dim),
        Span::styled(tag.to_string(), Style::default().fg(Color::Gray)),
        Span::styled("─".repeat(right), dim),
    ])
}

fn term_rows() -> u16 {
    crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24)
}

fn page_rows(term_h: u16) -> u16 {
    // title + divider + composer + strip ≈ 6 rows
    term_h.saturating_sub(7).max(3)
}

fn apply_page_up(app: &mut App, page: u16) {
    app.follow = false;
    app.scroll = app.scroll.saturating_add(page);
}

fn apply_page_down(app: &mut App, page: u16) {
    if app.scroll <= page {
        app.scroll = 0;
        app.follow = true;
    } else {
        app.scroll = app.scroll.saturating_sub(page);
    }
}

fn scroll_pair(app: &App, height: u16, width: u16, tz: &DisplayTz) -> (u16, u16) {
    let n = wrapped_count(app, width, tz);
    let max = n.saturating_sub(height as usize) as u16;
    if app.follow {
        (max, 0)
    } else {
        (max.saturating_sub(app.scroll.min(max)), 0)
    }
}

fn wrapped_count(app: &App, width: u16, tz: &DisplayTz) -> usize {
    transcript_lines(app, width, tz).len()
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn test_app() -> App {
        App {
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll: 0,
            follow: true,
            busy: false,
            interrupted: false,
            pending: PendingTurns::default(),
            picker: None,
            slash_menu: None,
            history: InputHistory::default(),
        }
    }

    #[test]
    fn transcript_has_no_you_or_atr_chips() {
        let mut app = test_app();
        app.messages.push(Msg {
            role: Role::You,
            text: "what about Helium?".into(),
            at: Utc.with_ymd_and_hms(2026, 10, 28, 11, 4, 0).unwrap(),
        });
        app.push(Role::Assistant, "Brave: no.");
        let tz = DisplayTz::parse("UTC").unwrap();
        let lines = transcript_lines(&app, 48, &tz);
        let texts: Vec<String> = lines.iter().map(line_plain).collect();
        let joined = texts.join("\n");
        assert!(
            texts
                .iter()
                .any(|t| t.contains("> what about Helium?") && t.contains("11:04 AM")),
            "user row must be `>` + local clock: {joined:?}"
        );
        assert!(
            texts.iter().any(|t| t == "Brave: no."),
            "assistant text starts with no role chip: {joined:?}"
        );
        for chip in ["You", "you", "atr", "Grok", "err"] {
            assert!(
                !texts.iter().any(|t| t.trim() == chip),
                "role chip {chip:?} leaked into {joined:?}"
            );
        }
    }

    #[test]
    fn wrap_cols_splits_long_row() {
        let pieces = wrap_cols(&"x".repeat(200), 40);
        assert_eq!(pieces.len(), 5);
        assert!(pieces.iter().all(|p| p.chars().count() <= 40));
    }

    #[test]
    fn huge_assistant_wrap_count_matches_display_rows() {
        let mut app = test_app();
        app.push(Role::Assistant, "word ".repeat(400));
        let tz = DisplayTz::parse("UTC").unwrap();
        let width = 40;
        let n = wrapped_count(&app, width, &tz);
        assert!(
            n >= 50,
            "long assistant blob must wrap into many rows, got {n}"
        );
        assert_eq!(n, transcript_lines(&app, width, &tz).len());
    }

    #[test]
    fn page_up_moves_by_viewport_not_three_lines() {
        let mut app = test_app();
        app.push(Role::Assistant, "line\n".repeat(200));
        let page = page_rows(24);
        assert!(page >= 10, "page size {page} should be viewport-sized");
        apply_page_up(&mut app, page);
        assert!(!app.follow);
        assert_eq!(app.scroll, page);
        apply_page_up(&mut app, page);
        assert_eq!(app.scroll, page * 2);
        let (y, _) = scroll_pair(&app, 17, 40, &DisplayTz::parse("UTC").unwrap());
        assert!(y < 200, "scrolled-up pair must leave the bottom: {y}");
        apply_page_down(&mut app, page);
        apply_page_down(&mut app, page);
        assert!(app.follow);
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn follow_scroll_pair_pins_bottom() {
        let mut app = test_app();
        app.push(Role::Assistant, "line\n".repeat(80));
        let tz = DisplayTz::parse("UTC").unwrap();
        let n = wrapped_count(&app, 40, &tz);
        let (y, _) = scroll_pair(&app, 10, 40, &tz);
        assert_eq!(y, n.saturating_sub(10) as u16);
        app.follow = false;
        app.scroll = 12;
        let (y2, _) = scroll_pair(&app, 10, 40, &tz);
        assert_eq!(y2, (n.saturating_sub(10) as u16).saturating_sub(12));
    }

    #[test]
    fn divider_marks_scrolled() {
        let mut app = test_app();
        let follow = line_plain(&divider_line(&app, 40));
        assert!(!follow.contains("scrolled"));
        app.follow = false;
        let up = line_plain(&divider_line(&app, 40));
        assert!(up.contains("scrolled"), "{up}");
    }
}
