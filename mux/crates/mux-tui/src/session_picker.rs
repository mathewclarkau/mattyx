//! Interactive pre-attach session picker for `mtyx attach --session-list`
//! (issue #63, layer L1).
//!
//! Runs BEFORE `run_attach`/`app::run`, so it must not touch `App` or
//! `Session` (those need an already-chosen socket). It is a self-contained
//! raw-mode loop mirroring the terminal lifecycle in `app.rs`:
//! `enable_raw_mode` → `EnterAlternateScreen` → a tiny ratatui
//! `Terminal<CrosstermBackend>` → `crossterm::event::read()` loop →
//! `restore_terminal` on EVERY exit path (Enter/q/Esc/Ctrl-C/panic). A
//! raw-mode or alt-screen leak would corrupt the subsequent `app::run` TUI
//! or the user's shell, so the panic hook (app.rs:674 pattern) is mandatory.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal as RatatuiTerminal;

use crate::cli::{self, DiscoveredSession, GlobalArgs};
use crate::ui::input::{InputEvent, TextInput};

/// What the picker decided. `main()` maps this to an exit code / attach.
pub(crate) enum PickerOutcome {
    /// Attach in-process: caller sets `socket`/`session` and falls through
    /// to the existing `run_attach` path (no fork/exec).
    Attach { socket_path: PathBuf, name: String },
    /// User quit (q/Esc). `destructive` is true if a session was killed
    /// during this run, so `main()` can exit 1 instead of 0 (Claim 3).
    Quit { destructive: bool },
    /// Ctrl-C.
    CtrlC,
}

/// Inline modal the loop can be in. `Browse` is the default list; the others
/// are transient overlays driven by x/s/n that return to `Browse` on commit
/// or cancel. Kept in the picker state (not App) since this runs pre-attach.
enum Mode {
    Browse,
    /// Confirm killing the focused session (Claim 3). y/N.
    ConfirmKill {
        index: usize,
        name: String,
    },
    /// Confirm killing every stale session (Claim 4). y/N.
    ConfirmKillStale {
        count: usize,
    },
    /// Inline new-session name prompt (Claim 5). Enter attaches/creates.
    NewSession(TextInput),
    /// Inline rename of the focused LIVE session (issue #63 L2, Q7).
    /// `socket_path` is the session's current socket (commit sends
    /// rename-session over it); `old_name` is shown in the prompt and
    /// pre-fills the input.
    RenameSession {
        socket_path: PathBuf,
        old_name: String,
        input: TextInput,
    },
}

/// Interactive modal picker. Restores the terminal on every return path
/// (including via the panic hook) before yielding control back to `main`,
/// so the subsequent `app::run` (on Attach) starts from a clean screen.
pub(crate) fn run(global: &GlobalArgs) -> anyhow::Result<PickerOutcome> {
    let stdout_lock = Arc::new(Mutex::new(()));
    enable_raw_mode()?;
    // Enter the alternate screen under the lock so restore_terminal can't
    // race a concurrent write (mirrors app.rs:644-651).
    if let Err(e) = (|| -> anyhow::Result<()> {
        let _guard = stdout_lock.lock().unwrap();
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(())
    })() {
        let _ = restore_terminal(Some(&stdout_lock));
        return Err(e);
    }
    // Restore on panic so a mid-frame panic can't strand the terminal in
    // raw/alt-screen mode (mirrors app.rs:674-680).
    let default_hook = std::panic::take_hook();
    let restore_lock = stdout_lock.clone();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal(Some(&restore_lock));
        default_hook(info);
    }));

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match RatatuiTerminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            let _ = std::panic::take_hook();
            let _ = restore_terminal(Some(&stdout_lock));
            return Err(e.into());
        }
    };

    let outcome = event_loop(&mut terminal, global);

    // Tear down the panic hook and restore the terminal before returning so
    // app::run (Attach) or the shell (Quit/CtrlC) inherits a clean screen.
    let _ = std::panic::take_hook();
    restore_terminal(Some(&stdout_lock))?;
    outcome
}

/// Full-screen raw-mode input loop.
fn event_loop(
    terminal: &mut RatatuiTerminal<CrosstermBackend<io::Stdout>>,
    global: &GlobalArgs,
) -> anyhow::Result<PickerOutcome> {
    let mut sessions = refresh(global);
    let mut state = ListState::default();
    clamp_selection(&mut state, &sessions);
    let mut destructive = false;
    let mut status = String::new();
    let mut mode = Mode::Browse;

    draw(terminal, &mut state, &sessions, &status, &mut mode)?;
    loop {
        // Poll with a short timeout so a future background refresh could
        // re-run discovery; for L1 we just re-loop and redraw on key/resize.
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(k) => k,
            Event::Resize(_, _) => {
                draw(terminal, &mut state, &sessions, &status, &mut mode)?;
                continue;
            }
            _ => continue,
        };
        match dispatch(&key, &mut state, &mut sessions, &mut status, &mut mode, global) {
            Action::Quit => return Ok(PickerOutcome::Quit { destructive }),
            Action::CtrlC => return Ok(PickerOutcome::CtrlC),
            Action::Attach(socket_path, name) => {
                return Ok(PickerOutcome::Attach { socket_path, name });
            }
            Action::None => {}
            Action::Killed => {
                destructive = true;
                clamp_selection(&mut state, &sessions);
                draw(terminal, &mut state, &sessions, &status, &mut mode)?;
            }
            Action::Redraw => {
                draw(terminal, &mut state, &sessions, &status, &mut mode)?;
            }
        }
    }
}

/// What `dispatch` wants the loop to do after handling a key.
enum Action {
    None,
    Redraw,
    /// A session was killed this keystroke; flip `destructive` then redraw.
    Killed,
    Quit,
    CtrlC,
    Attach(PathBuf, String),
}

/// Route a key to the current mode. Side-effecting variants (kill, new,
/// refresh) mutate `sessions`/`status`/`mode` directly so the redraw in
/// `event_loop` reflects them immediately.
fn dispatch(
    key: &KeyEvent,
    state: &mut ListState,
    sessions: &mut Vec<DiscoveredSession>,
    status: &mut String,
    mode: &mut Mode,
    global: &GlobalArgs,
) -> Action {
    // Ctrl-C works from any mode.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Action::CtrlC;
    }
    match mode {
        Mode::Browse => browse_key(key, state, sessions, status, mode),
        Mode::ConfirmKill { index, name } => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let idx = *index;
                    let nm = name.clone();
                    if let Some(s) = sessions.get(idx).cloned() {
                        cli::kill_session_at(&s.socket_path, s.pid);
                        *status = format!("killed {nm}");
                        *sessions = refresh(global);
                        destructive_select(state, &sessions, idx);
                        *mode = Mode::Browse;
                        return Action::Killed;
                    }
                    *mode = Mode::Browse;
                    Action::Redraw
                }
                // any other key = No (Esc, n, Enter, …): cancel
                _ => {
                    *mode = Mode::Browse;
                    Action::Redraw
                }
            }
        }
        Mode::ConfirmKillStale { count } => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    // Reuse the cli kill-stale path: it is already built on
                    // discover_sessions, so semantics match `mtyx kill-stale`.
                    let cleaned = cli::kill_stale(global);
                    *status = format!("cleaned {cleaned} stale session(s)");
                    *sessions = refresh(global);
                    let _ = count; // borrowed only for the prompt label
                    *mode = Mode::Browse;
                    Action::Killed
                }
                _ => {
                    *mode = Mode::Browse;
                    Action::Redraw
                }
            }
        }
        Mode::NewSession(input) => {
            match input.handle_key(key) {
                InputEvent::Commit => {
                    let name = input.as_str().trim().to_string();
                    if name.is_empty() {
                        *status = "session name cannot be empty".to_string();
                        *mode = Mode::Browse;
                        return Action::Redraw;
                    }
                    // Q4 (approved): if the name matches a LIVE session,
                    // attach to it; if stale, kill-stale then create fresh.
                    let existing = sessions.iter().find(|s| s.session == name).cloned();
                    match existing {
                        Some(s) if s.live => {
                            Action::Attach(s.socket_path.clone(), s.session.clone())
                        }
                        Some(_) => {
                            // stale同名: clean it, then create fresh below.
                            cli::kill_stale(global);
                            *sessions = refresh(global);
                            let sock = mux_core::server::default_socket_path(&name);
                            Action::Attach(sock, name)
                        }
                        None => {
                            let sock = mux_core::server::default_socket_path(&name);
                            Action::Attach(sock, name)
                        }
                    }
                }
                InputEvent::Cancel => {
                    *mode = Mode::Browse;
                    Action::Redraw
                }
                InputEvent::None | InputEvent::Changed => Action::Redraw,
            }
        }
        Mode::RenameSession { socket_path, old_name, input } => {
            match input.handle_key(key) {
                InputEvent::Commit => {
                    let new_name = input.as_str().trim().to_string();
                    let socket_path = socket_path.clone();
                    let old_name = old_name.clone();
                    if new_name.is_empty() {
                        *status = "session name cannot be empty".to_string();
                        *mode = Mode::Browse;
                        return Action::Redraw;
                    }
                    // Unchanged name = cancel (no-op).
                    if new_name == old_name {
                        *status = format!("unchanged ({old_name})");
                        *mode = Mode::Browse;
                        return Action::Redraw;
                    }
                    // Server-side validation (defence in depth; the CLI verb
                    // also validates, but the picker talks to the helper
                    // directly, so check here too).
                    if let Err(e) = mux_core::server::validate_session_name(&new_name) {
                        *status = e.to_string();
                        *mode = Mode::Browse;
                        return Action::Redraw;
                    }
                    // Refuse if another LIVE session already has the name.
                    if sessions.iter().any(|s| s.session == new_name && s.live) {
                        *status = format!("session {new_name:?} already exists");
                        *mode = Mode::Browse;
                        return Action::Redraw;
                    }
                    match cli::rename_session_at(&socket_path, &new_name) {
                        Ok(_) => {
                            *sessions = refresh(global);
                            // Keep focus on the renamed row (now listed under
                            // its new name, newest-first).
                            if let Some(i) =
                                sessions.iter().position(|s| s.session == new_name && s.live)
                            {
                                state.select(Some(i));
                            }
                            *status = format!("renamed {old_name} → {new_name}");
                            *mode = Mode::Browse;
                            Action::Redraw
                        }
                        Err(e) => {
                            *status = format!("rename failed: {e}");
                            *mode = Mode::Browse;
                            Action::Redraw
                        }
                    }
                }
                InputEvent::Cancel => {
                    *mode = Mode::Browse;
                    Action::Redraw
                }
                InputEvent::None | InputEvent::Changed => Action::Redraw,
            }
        }
    }
}

/// Keys for the default Browse mode.
fn browse_key(
    key: &KeyEvent,
    state: &mut ListState,
    sessions: &[DiscoveredSession],
    status: &mut String,
    mode: &mut Mode,
) -> Action {
    let len = sessions.len();
    let selected = state.selected();
    match key.code {
        // Navigation. vim j/k matches USAGE "h/j/k/l or arrows move focus".
        KeyCode::Up | KeyCode::Char('k') => move_cursor(state, selected, len, false),
        KeyCode::Down | KeyCode::Char('j') => move_cursor(state, selected, len, true),
        KeyCode::Enter => {
            if let Some(i) = selected {
                if let Some(s) = sessions.get(i) {
                    if s.live {
                        return Action::Attach(s.socket_path.clone(), s.session.clone());
                    }
                    // Q2 (approved): stale stays focusable but Enter does
                    // not attach — the socket isn't connectable.
                    *status = format!(
                        "{} is unreachable (socket not connectable) — cannot attach",
                        s.session
                    );
                    return Action::Redraw;
                }
            }
            Action::None
        }
        // Claim 3: kill focused. `x` (not `k`) — k is 'navigate up' per
        // mtyx USAGE ("h/j/k/l move focus") and matches the whole codebase.
        KeyCode::Char('x') => {
            if let Some(i) = selected {
                if let Some(s) = sessions.get(i) {
                    *mode = Mode::ConfirmKill { index: i, name: s.session.clone() };
                    return Action::Redraw;
                }
            }
            Action::None
        }
        // Claim 4: kill all stale.
        KeyCode::Char('s') => {
            let count = sessions.iter().filter(|s| !s.live).count();
            if count == 0 {
                *status = "no stale sessions to clean".to_string();
                Action::Redraw
            } else {
                *mode = Mode::ConfirmKillStale { count };
                Action::Redraw
            }
        }
        // Claim 5: new session (inline name prompt).
        KeyCode::Char('n') => {
            *mode = Mode::NewSession(TextInput::new(String::new()));
            Action::Redraw
        }
        // Issue #63 L2 (Q7): rename the focused LIVE session inline. A
        // stale/unfocused row gets a hint (nothing to rename).
        KeyCode::Char('r') => {
            if let Some(i) = state.selected() {
                if let Some(s) = sessions.get(i) {
                    if s.live {
                        *mode = Mode::RenameSession {
                            socket_path: s.socket_path.clone(),
                            old_name: s.session.clone(),
                            input: TextInput::new(s.session.clone()),
                        };
                        return Action::Redraw;
                    }
                }
            }
            *status = "no live session focused to rename (move to a live row)".to_string();
            Action::Redraw
        }
        KeyCode::Esc | KeyCode::Char('q') => Action::Quit,
        _ => Action::None,
    }
}

/// Apply an up/down move, returning Redraw iff the selection changed.
fn move_cursor(state: &mut ListState, selected: Option<usize>, len: usize, down: bool) -> Action {
    if len == 0 {
        return Action::None;
    }
    let i = selected.unwrap_or(0);
    let next = if down { i + 1 } else { i.saturating_sub(1) };
    if next >= len || next == i {
        Action::None
    } else {
        state.select(Some(next));
        Action::Redraw
    }
}

/// After a kill shrinks the list, keep a sensible focus (same index clamped,
/// or the new last item) so the user isn't dropped to the top every time.
fn destructive_select(state: &mut ListState, sessions: &[DiscoveredSession], was: usize) {
    if sessions.is_empty() {
        state.select(None);
    } else {
        state.select(Some(was.min(sessions.len() - 1)));
    }
}

/// Re-run discovery, newest-first (mtime DESC; `None` sorts last), for the
/// picker's "most recent session on top" ordering. Lazily reaps stale
/// `.sock`/`.pid` pairs on enumerate (issue #138) using the shared #137 sweep.
fn refresh(global: &GlobalArgs) -> Vec<DiscoveredSession> {
    let (mut sessions, _swept) = cli::discover_sessions_with_gc(global, true);
    sessions.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    sessions
}

fn clamp_selection(state: &mut ListState, sessions: &[DiscoveredSession]) {
    if sessions.is_empty() {
        state.select(None);
    } else if state.selected().map_or(true, |i| i >= sessions.len()) {
        state.select(Some(0));
    }
}

fn draw(
    terminal: &mut RatatuiTerminal<CrosstermBackend<io::Stdout>>,
    state: &mut ListState,
    sessions: &[DiscoveredSession],
    status: &str,
    mode: &mut Mode,
) -> io::Result<()> {
    terminal.draw(|f| {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(f.area());

        let items: Vec<ListItem> = sessions
            .iter()
            .map(|s| {
                let (label, style) = if s.live {
                    (s.session.clone(), Style::default())
                } else {
                    (
                        format!("[unreachable] {}", s.session),
                        Style::default().fg(Color::DarkGray),
                    )
                };
                ListItem::new(Line::from(Span::styled(label, style)))
            })
            .collect();
        let title = if sessions.is_empty() {
            " mtyx sessions — none found (n new, q to quit) "
        } else {
            " mtyx sessions — pick one to attach "
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, chunks[0], state);

        // Footer: mode prompt (if any) on row 1, keymap hints + status on row 2.
        let mut lines = Vec::new();
        match mode {
            Mode::Browse => {
                lines.push(Line::from(Span::styled(
                    "↑/↓ or j/k move · Enter attach · x kill · s kill-stale · n new · r rename focused · q/Esc quit · Ctrl-C abort",
                    Style::default().fg(Color::DarkGray),
                )));
            }
            Mode::ConfirmKill { name, .. } => {
                lines.push(Line::from(Span::styled(
                    format!("Kill {name}?  y/N  (any other key cancels)"),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )));
            }
            Mode::ConfirmKillStale { count } => {
                lines.push(Line::from(Span::styled(
                    format!("Kill {count} stale session(s)?  y/N  (any other key cancels)"),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )));
            }
            Mode::NewSession(input) => {
                // visible_text_and_cursor takes &mut self (it lazily
                // recomputes scroll); `input` is &mut via the &mut Mode.
                let (visible, cur) = input.visible_text_and_cursor(chunks[1].width as usize);
                let mut spans =
                    vec![Span::styled("new session name: ", Style::default().fg(Color::Cyan))];
                spans.push(Span::raw(visible.clone()));
                if cur < visible.len() {
                    spans.push(Span::raw(visible[cur..].to_string()));
                }
                lines.push(Line::from(spans));
            }
            Mode::RenameSession { old_name, input, .. } => {
                let (visible, cur) = input.visible_text_and_cursor(chunks[1].width as usize);
                let mut spans = vec![Span::styled(
                    format!("rename {old_name} → "),
                    Style::default().fg(Color::Cyan),
                )];
                spans.push(Span::raw(visible.clone()));
                if cur < visible.len() {
                    spans.push(Span::raw(visible[cur..].to_string()));
                }
                lines.push(Line::from(spans));
            }
        }
        if !status.is_empty() {
            lines.push(Line::from(Span::styled(
                status,
                Style::default().fg(Color::Yellow),
            )));
        }
        f.render_widget(Paragraph::new(Text::from(lines)), chunks[1]);
    })?;
    Ok(())
}

/// Mirror of app.rs:740 restore_terminal: leave the alternate screen and
/// disable raw mode. Best-effort on the screen switch so one failure can't
/// strand the terminal in raw mode.
fn restore_terminal(stdout_lock: Option<&Arc<Mutex<()>>>) -> anyhow::Result<()> {
    let _guard = stdout_lock.map(|lock| lock.lock().unwrap());
    let _ = io::stdout().execute(LeaveAlternateScreen);
    disable_raw_mode()?;
    Ok(())
}
