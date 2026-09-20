//! Fuzzy finder overlay (`leader G`): type-ahead search across
//! workspace names, pane names, surface titles, and agent session ids,
//! with a state filter (`B`/`W`/`I`/`D`/`A`).
//!
//! The finder is a pure presentation layer over the existing
//! [`TreeView`](crate::session::TreeView) snapshot, so it needs no new
//! protocol verbs. `build_items` walks the tree in display order; the
//! matcher is a small subsequence scorer written from scratch (no new
//! crate, keeping the Cargo.lock at version 3).

use mux_core::{AgentState, PaneId, Rect, SurfaceId, WorkspaceId};
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Frame;

use crate::session::TreeView;
use crate::ui::input::TextInput;
use crate::ui::truncate;

/// Which backing tree node a finder row points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderTarget {
    Workspace(WorkspaceId),
    Pane(PaneId),
    Surface(SurfaceId),
}

/// Agent-state filter. `All` shows every row; the others exclude both
/// non-matching states and rows with no state at all (workspaces and
/// panes have no agent state, only surfaces do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFilter {
    All,
    Working,
    Blocked,
    Idle,
    Done,
}

impl StateFilter {
    /// Keep a row whose `agent_state` matches this filter.
    fn keeps(self, agent_state: Option<AgentState>) -> bool {
        match self {
            StateFilter::All => true,
            StateFilter::Working => agent_state == Some(AgentState::Working),
            StateFilter::Blocked => agent_state == Some(AgentState::Blocked),
            StateFilter::Idle => agent_state == Some(AgentState::Idle),
            StateFilter::Done => agent_state == Some(AgentState::Done),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StateFilter::All => "all",
            StateFilter::Working => "working",
            StateFilter::Blocked => "blocked",
            StateFilter::Idle => "idle",
            StateFilter::Done => "done",
        }
    }
}

/// Outcome of an Esc/Cancel on the finder: clear an active state filter
/// first (keeping the overlay open), and only close the finder when the
/// filter is already `All`. See [`FinderState::cancel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderCancel {
    /// An active state filter was reset to `All`; the finder stayed open.
    ClearedFilter,
    /// No filter was active, so the caller should close the finder.
    CloseFinder,
}

/// One searchable row. `agent_state` is `None` for workspaces and panes
/// (they have no agent report) and `Some` for surfaces, sourced from the
/// tab's reported state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinderItem {
    pub target: FinderTarget,
    pub label: String,
    /// The agent session id reported on this row's surface, if any.
    /// Workspaces and panes carry no session; surfaces carry the same
    /// `session` string `list-agents` reports, so type-ahead matches it.
    pub agent_session: Option<String>,
    pub agent_state: Option<AgentState>,
}

/// Overlay state: the query input, the active state filter, the
/// selection cursor, the scroll offset of the first visible row, and the
/// item list (built once on open and refreshed while the user types).
/// `scroll` keeps `cursor` inside `[scroll, scroll + rows_visible)` so
/// the selection is always drawn and the click hit-test (`finder_row_at`)
/// and the draw path share one viewport, fixing off-screen selection and
/// stale mouse hit-tests on lists longer than the overlay window.
#[derive(Debug, Clone)]
pub struct FinderState {
    pub input: TextInput,
    pub state_filter: StateFilter,
    pub cursor: usize,
    pub scroll: usize,
    pub items: Vec<FinderItem>,
}

impl FinderState {
    pub fn new(items: Vec<FinderItem>) -> Self {
        FinderState {
            input: TextInput::new(String::new()),
            state_filter: StateFilter::All,
            cursor: 0,
            scroll: 0,
            items,
        }
    }

    /// Apply `next` as the active state filter, toggling back to `All`
    /// when the same filter key is pressed again (so `B` then `B`
    /// returns to the unfiltered list). Pressing `A` always collapses to
    /// `All` (it is the explicit "no filter" key), which is a no-op when
    /// the filter is already `All`. The cursor and scroll are clamped to
    /// the new (smaller or larger) ranked list so the selection never
    /// points past the end.
    pub fn set_state_filter(&mut self, next: StateFilter) {
        self.state_filter = if self.state_filter == next { StateFilter::All } else { next };
        let len = self.ranked().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
        } else {
            if self.cursor >= len {
                self.cursor = len - 1;
            }
            self.clamp_scroll_after_shrink(len);
        }
    }

    /// Handle an Esc/Cancel. If a state filter is active, clear it back to
    /// `All` and stay open (returns `ClearedFilter`); only when the filter
    /// is already `All` does this signal the caller to close the finder
    /// (`CloseFinder`). Mirrors the same-key-toggle clear so Esc clears the
    /// filter first and only closes on a second press (AC3 Esc path). The
    /// cursor and scroll reset to the top, matching what the toggle path
    /// does after a filter change.
    pub fn cancel(&mut self) -> FinderCancel {
        if self.state_filter != StateFilter::All {
            self.state_filter = StateFilter::All;
            self.cursor = 0;
            self.scroll = 0;
            FinderCancel::ClearedFilter
        } else {
            FinderCancel::CloseFinder
        }
    }

    /// Replace the snapshot of finder items built on open. Used when an
    /// `agent-state-changed` event arrives while the finder is open so the
    /// live agent states refresh without losing the current query, state
    /// filter, or cursor position. The cursor is clamped to the new ranked
    /// list length in case the refresh shrank the matching set.
    pub fn set_items(&mut self, items: Vec<FinderItem>) {
        self.items = items;
        let len = self.ranked().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
        } else {
            if self.cursor >= len {
                self.cursor = len - 1;
            }
            self.clamp_scroll_after_shrink(len);
        }
    }

    /// Clamp `scroll` so it never sits past the cursor (or past the last
    /// row) after the ranked list shrinks. Without this an
    /// `agent-state-changed` refresh that shrinks the matching set below
    /// the current scroll offset would leave `draw()`'s
    /// `ranked.iter().skip(scroll).take(rows)` skipping every row and
    /// rendering a blank viewport until the next keypress resets scroll.
    /// The cursor is already clamped to `[0, len - 1]` so capping scroll
    /// at the cursor keeps a row on screen without needing the screen size
    /// (which the finder does not store).
    fn clamp_scroll_after_shrink(&mut self, len: usize) {
        let max_scroll = len.saturating_sub(1);
        if self.scroll > self.cursor {
            self.scroll = self.cursor;
        }
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    /// Number of result rows the overlay can show at once on `screen`.
    /// Zero when the overlay does not fit. Shared by the draw path, the
    /// cursor/scroll handling, and the click hit-test arithmetic so all
    /// three agree on the viewport height.
    pub fn rows_visible(screen: Rect) -> usize {
        finder_rect(screen)
            .map(|rect| (rect.height.saturating_sub((ROWS_TOP_OFFSET + 1) as u16)) as usize)
            .unwrap_or(0)
    }

    /// Rows that pass the state filter and match the current query, kept
    /// in tree order when the query is empty. Returns the index into
    /// `self.items` plus the match score (lower is better).
    pub fn ranked(&self) -> Vec<(usize, u32)> {
        let query = self.input.as_str();
        let mut out: Vec<(usize, u32)> = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            if !self.state_filter.keeps(item.agent_state) {
                continue;
            }
            // A row matches if the query is a subsequence of its label OR
            // of its agent session id (when present). The label still
            // drives display; the session is purely extra searchable text
            // so type-ahead covers agent session ids per AC2.
            let score = fuzzy_score(query, &item.label)
                .or_else(|| item.agent_session.as_deref().and_then(|s| fuzzy_score(query, s)));
            if let Some(score) = score {
                out.push((i, score));
            }
        }
        // Stable sort by score (lower is better); empty query gives every
        // row score 0, so the original tree order is preserved.
        out.sort_by_key(|(_, score)| *score);
        out
    }

    /// Move the cursor by `delta` rows through the ranked list, then
    /// scroll the viewport so the cursor stays inside the visible window.
    /// `rows_visible` is [`FinderState::rows_visible`] for the current
    /// screen; passing it in (rather than re-deriving it) keeps the finder
    /// a pure function of its arguments with no hidden screen dependency.
    pub fn move_cursor(&mut self, delta: isize, rows_visible: usize) {
        let len = self.ranked().len();
        if len == 0 {
            self.cursor = 0;
            self.scroll = 0;
            return;
        }
        let next = (self.cursor as isize + delta).rem_euclid(len as isize) as usize;
        self.cursor = next.min(len - 1);
        self.clamp_scroll(rows_visible, len);
    }

    /// Keep `cursor` inside `[scroll, scroll + rows_visible)` so the
    /// selection is always one of the drawn rows. When `rows_visible` is
    /// 0 (screen too small for a results region) there is no window to
    /// track, so the viewport stays at the top.
    pub fn clamp_scroll(&mut self, rows_visible: usize, len: usize) {
        if len == 0 {
            self.scroll = 0;
            return;
        }
        let visible = rows_visible.max(1).min(len);
        let max_scroll = len - visible;
        if self.cursor >= self.scroll + visible {
            self.scroll = self.cursor + 1 - visible;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    /// The selected row's target, if any.
    pub fn selected(&self) -> Option<FinderTarget> {
        let ranked = self.ranked();
        let entry = ranked.get(self.cursor)?;
        Some(self.items[entry.0].target)
    }
}

/// Build the finder item list from a tree snapshot, walking workspaces,
/// screens, panes, then tabs in display order. Pure: identical input
/// trees produce identical item lists.
pub fn build_items(tree: &TreeView) -> Vec<FinderItem> {
    let mut items = Vec::new();
    for ws in &tree.workspaces {
        items.push(FinderItem {
            target: FinderTarget::Workspace(ws.id),
            label: format!("#{} {}", ws.short_id, ws.name),
            agent_session: None,
            agent_state: None,
        });
        for screen in &ws.screens {
            for pane in &screen.panes {
                items.push(FinderItem {
                    target: FinderTarget::Pane(pane.id),
                    label: format!(
                        "#{} {}",
                        pane.short_id,
                        pane.name.as_deref().unwrap_or(pane.display_name())
                    ),
                    agent_session: None,
                    agent_state: None,
                });
                for tab in &pane.tabs {
                    let title = tab.name.as_deref().filter(|s| !s.is_empty()).unwrap_or(&tab.title);
                    items.push(FinderItem {
                        target: FinderTarget::Surface(tab.surface),
                        label: format!("#{} {}", tab.short_id, title),
                        agent_session: tab.agent_session.clone(),
                        agent_state: tab.agent_state,
                    });
                }
            }
        }
    }
    items
}

/// Subsequence fuzzy match. Returns `None` when `query` is not a
/// subsequence of `hay` (case-insensitive), else `Some(score)` where a
/// lower score is a tighter match (consecutive matches score 0 extra;
/// each gap adds its length). An empty query matches everything with
/// score 0 so the caller keeps the tree order intact.
pub fn fuzzy_score(query: &str, hay: &str) -> Option<u32> {
    let query: Vec<char> = query.chars().collect();
    if query.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = hay.chars().collect();
    let mut qi = 0usize;
    let mut score: u32 = 0;
    let mut prev: i64 = -1;
    for (hi, hc) in hay.iter().enumerate() {
        if qi >= query.len() {
            break;
        }
        if hc.eq_ignore_ascii_case(&query[qi]) {
            if prev >= 0 {
                let gap = (hi as i64 - prev) as u32;
                score = score.saturating_add(gap.saturating_sub(1));
            }
            prev = hi as i64;
            qi += 1;
        }
    }
    if qi == query.len() {
        Some(score)
    } else {
        None
    }
}

/// The centered bordered-box rectangle the finder overlay occupies,
/// matching [`draw`]'s geometry. Pure: identical screen sizes produce
/// identical rects, so the draw path and the click hit-test path share
/// one source of truth. Returns `None` when the screen is too small.
pub fn finder_rect(screen: Rect) -> Option<Rect> {
    let width = 60u16.min(screen.width.saturating_sub(2)).max(30);
    let height = 14u16.min(screen.height.saturating_sub(2)).max(4);
    if screen.width < width || screen.height < height {
        return None;
    }
    Some(Rect { x: (screen.width - width) / 2, y: (screen.height - height) / 2, width, height })
}

/// Y offset of the first results row inside the overlay box. Kept
/// alongside [`finder_rect`] so the click hit-test path maps a
/// screen-space point to a results row with the same arithmetic the
/// draw path uses.
const ROWS_TOP_OFFSET: u16 = 4;

/// Map a screen-space click to the ranked-row index it lands on,
/// accounting for the viewport `scroll` so what's drawn and what's
/// clickable always agree (the off-screen selection fix). Returns `None`
/// when the click is outside the overlay box or in its title/query/filter
/// chrome (anything above the results region). Callers index the ranked
/// list with the returned value, so a click on a row beyond the ranked
/// list simply yields no target (the caller's `ranked().get(row)` is
/// `None`).
pub fn finder_row_at(screen: Rect, x: u16, y: u16, scroll: usize) -> Option<usize> {
    let rect = finder_rect(screen)?;
    if !rect.contains(x, y) {
        return None;
    }
    let rows_y = rect.y + ROWS_TOP_OFFSET;
    let rows_h = rect.height.saturating_sub((ROWS_TOP_OFFSET + 1) as u16);
    if y < rows_y || y >= rows_y + rows_h {
        return None;
    }
    let visible_row = (y - rows_y) as usize;
    Some(scroll + visible_row)
}

/// Draw the finder overlay: a bordered box centered on the frame with
/// the query input on the top row, the state filter on the right of it,
/// and the ranked rows below. The finder owns the terminal cursor while
/// open. Best-effort geometry: if the screen is too small, it draws
/// nothing rather than panicking.
pub fn draw(app: &mut crate::app::App, frame: &mut Frame) {
    let ratatui_screen = frame.area();
    let screen = Rect {
        x: ratatui_screen.x,
        y: ratatui_screen.y,
        width: ratatui_screen.width,
        height: ratatui_screen.height,
    };
    let Some(rect) = finder_rect(screen) else { return };
    let Some(finder) = app.finder.as_mut() else { return };
    let x = rect.x;
    let width = rect.width;
    let height = rect.height;
    let y = rect.y;
    let base = Style::default().bg(Color::Indexed(236)).fg(Color::Indexed(252));
    let border = base.fg(Color::Indexed(244));
    let title = base.fg(Color::Indexed(255)).add_modifier(Modifier::BOLD);
    let input_style = Style::default().bg(Color::Indexed(233)).fg(Color::Indexed(255));
    let selected = Style::default()
        .bg(Color::Indexed(242))
        .fg(Color::Indexed(255))
        .add_modifier(Modifier::BOLD);
    let filter_label = format!("[{}: B W I D A]", finder.state_filter.label());
    let fw = filter_label.chars().count() as u16;
    let fx = x + width.saturating_sub(fw + 2);
    let input_w = width.saturating_sub(4);
    let (shown, cursor_col) = finder.input.visible_text_and_cursor(input_w as usize);
    let cursor_x = x + 2 + (cursor_col as u16).min(input_w);
    let ranked = finder.ranked();
    let rows_y = y + ROWS_TOP_OFFSET;
    let rows_h = height.saturating_sub((ROWS_TOP_OFFSET + 1) as u16);

    // Background, border, title, filter label, and query input row. Drawn
    // in a scope so the buffer borrow ends before we move the terminal
    // cursor below.
    {
        let buf = frame.buffer_mut();
        for dy in 0..height {
            for dx in 0..width {
                set_cell(buf, x + dx, y + dy, " ", base);
            }
        }
        draw_border(buf, x, y, width, height, border);
        buf.set_stringn(x + 2, y + 1, "Find", 4, title);
        buf.set_stringn(fx, y + 1, &filter_label, fw as usize, base.fg(Color::Indexed(244)));
        for dx in 0..input_w {
            set_cell(buf, x + 2 + dx, y + 2, " ", input_style);
        }
        buf.set_stringn(x + 2, y + 2, &shown, input_w as usize, input_style);
    }
    frame.set_cursor_position(Position::new(cursor_x, y + 2));

    // Ranked rows beneath the query input, scrolled so the cursor row
    // stays inside the visible window. `ranked_i` is the position in the
    // ranked list (the cursor indexes the same list), and the screen row
    // is `ranked_i - scroll`. What's drawn here is exactly what
    // `finder_row_at` maps a click on the same screen row back to.
    {
        let buf = frame.buffer_mut();
        for (ranked_i, &(item_i, _)) in
            ranked.iter().enumerate().skip(finder.scroll).take(rows_h as usize)
        {
            let row_y = rows_y + (ranked_i - finder.scroll) as u16;
            let item = &finder.items[item_i];
            let style = if ranked_i == finder.cursor { selected } else { base };
            let label = truncate(&item.label, (width as usize).saturating_sub(4));
            let prefix = match item.agent_state {
                Some(AgentState::Working) => "W",
                Some(AgentState::Blocked) => "B",
                Some(AgentState::Idle) => "I",
                Some(AgentState::Done) => "D",
                Some(AgentState::Unknown) => "?",
                None => " ",
            };
            for dx in 0..width.saturating_sub(2) {
                set_cell(buf, x + 1 + dx, row_y, " ", style);
            }
            buf.set_stringn(x + 1, row_y, prefix, 1, style.fg(Color::Indexed(244)));
            buf.set_stringn(x + 3, row_y, &label, (width as usize).saturating_sub(4), style);
        }
    }
}

fn set_cell(buf: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    let cell = &mut buf[(x, y)];
    cell.reset();
    cell.set_symbol(symbol).set_style(style);
}

fn draw_border(buf: &mut Buffer, x: u16, y: u16, w: u16, h: u16, style: Style) {
    if w < 2 || h < 2 {
        return;
    }
    let x0 = x;
    let y0 = y;
    let x1 = x + w - 1;
    let y1 = y + h - 1;
    for col in x0 + 1..x1 {
        set_cell(buf, col, y0, "-", style);
        set_cell(buf, col, y1, "-", style);
    }
    for row in y0 + 1..y1 {
        set_cell(buf, x0, row, "|", style);
        set_cell(buf, x1, row, "|", style);
    }
    set_cell(buf, x0, y0, "+", style);
    set_cell(buf, x1, y0, "+", style);
    set_cell(buf, x0, y1, "+", style);
    set_cell(buf, x1, y1, "+", style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mux_core::AgentState;

    fn item(label: &str, state: Option<AgentState>, target: FinderTarget) -> FinderItem {
        FinderItem { target, label: label.to_string(), agent_session: None, agent_state: state }
    }

    /// Same as [`item`] but with an agent session id attached, mirroring a
    /// surface row built from a tab that carries an agent report.
    fn item_with_session(
        label: &str,
        state: Option<AgentState>,
        target: FinderTarget,
        session: &str,
    ) -> FinderItem {
        FinderItem {
            target,
            label: label.to_string(),
            agent_session: Some(session.to_string()),
            agent_state: state,
        }
    }

    /// A throwaway target id; only the label and state matter for the
    /// matcher and filter tests.
    fn surf(id: u64) -> FinderTarget {
        FinderTarget::Surface(id)
    }

    #[test]
    fn fuzzy_score_none_for_no_subsequence() {
        // No 'b' anywhere in "claude-reviewer", so "cb" cannot match.
        assert_eq!(fuzzy_score("cb", "claude-reviewer"), None);
        // A present but non-matching subsequence: 'x' not in haystack.
        assert_eq!(fuzzy_score("x", "claude-builder"), None);
        // Matching subsequence returns Some.
        assert!(fuzzy_score("cb", "claude-builder").is_some());
        // Empty query always matches.
        assert_eq!(fuzzy_score("", "anything"), Some(0));
        assert_eq!(fuzzy_score("", ""), Some(0));
    }

    #[test]
    fn ranks_cb_builder_before_reviewer() {
        let items =
            vec![item("claude-builder", None, surf(1)), item("claude-reviewer", None, surf(2))];
        let mut finder = FinderState::new(items);
        finder.input = TextInput::new("cb".to_string());
        let ranked = finder.ranked();
        // The reviewer has no 'b', so it is excluded entirely; the builder
        // is the only match and appears first.
        assert_eq!(ranked.len(), 1);
        assert_eq!(finder.items[ranked[0].0].label, "claude-builder");
    }

    #[test]
    fn state_filter_excludes_nonmatching() {
        let items = vec![
            item("working-agent", Some(AgentState::Working), surf(1)),
            item("blocked-agent", Some(AgentState::Blocked), surf(2)),
            item("idle-agent", Some(AgentState::Idle), surf(3)),
            item("done-agent", Some(AgentState::Done), surf(4)),
            item("workspace-one", None, FinderTarget::Workspace(1)),
            item("pane-one", None, FinderTarget::Pane(1)),
        ];
        // Working filter: only the working agent, no stateless rows.
        let mut working = FinderState::new(items.clone());
        working.state_filter = StateFilter::Working;
        let ranked = working.ranked();
        assert_eq!(ranked.len(), 1);
        assert_eq!(working.items[ranked[0].0].label, "working-agent");

        // Blocked filter likewise.
        let mut blocked = FinderState::new(items.clone());
        blocked.state_filter = StateFilter::Blocked;
        let ranked = blocked.ranked();
        assert_eq!(ranked.len(), 1);
        assert_eq!(blocked.items[ranked[0].0].label, "blocked-agent");

        // All filter keeps every row.
        let mut all = FinderState::new(items.clone());
        all.state_filter = StateFilter::All;
        assert_eq!(all.ranked().len(), items.len());
    }

    #[test]
    fn empty_query_keeps_all_in_tree_order() {
        let items = vec![
            item("alpha", None, surf(1)),
            item("beta", None, surf(2)),
            item("gamma", None, surf(3)),
        ];
        let finder = FinderState::new(items.clone());
        let ranked = finder.ranked();
        // Every row present, in the original (tree) order.
        assert_eq!(ranked.len(), items.len());
        for (i, (item_i, _)) in ranked.iter().enumerate() {
            assert_eq!(*item_i, i, "tree order not preserved at row {i}");
        }
    }

    #[test]
    fn agent_session_id_is_searchable() {
        // Two surfaces whose labels share no letters with the session id;
        // only the agent session id can match the typed query.
        let items = vec![
            item_with_session("shell", Some(AgentState::Working), surf(1), "agent-7f3a-session"),
            item_with_session("shell", Some(AgentState::Idle), surf(2), "agent-7f3b-session"),
        ];
        // A query fragment present only in the session id surfaces the
        // matching agent row.
        let mut finder = FinderState::new(items.clone());
        finder.input = TextInput::new("7f3b".to_string());
        let ranked = finder.ranked();
        assert_eq!(ranked.len(), 1, "only the surface whose session id contains the query matches");
        assert_eq!(finder.items[ranked[0].0].agent_session.as_deref(), Some("agent-7f3b-session"));
        assert_eq!(finder.items[ranked[0].0].target, surf(2));

        // An empty query still surfaces every row (session id or not).
        finder.input = TextInput::new(String::new());
        assert_eq!(finder.ranked().len(), items.len());
    }

    #[test]
    fn finder_rect_is_centered_and_too_small_yields_none() {
        // On a 100x40 screen the box is 60x14, centred by the screen's own
        // width/height (the overlay positions against the terminal origin,
        // matching the draw path, so `screen.x` does not shift it).
        let screen = Rect { x: 10, y: 0, width: 100, height: 40 };
        let rect = finder_rect(screen).unwrap();
        assert_eq!(rect.width, 60);
        assert_eq!(rect.height, 14);
        assert_eq!(rect.x, (100 - 60) / 2);
        assert_eq!(rect.y, (40 - 14) / 2);

        // A screen just under the minimum row height has no overlay.
        let tiny = Rect { x: 0, y: 0, width: 100, height: 3 };
        assert!(finder_rect(tiny).is_none());
    }

    #[test]
    fn finder_row_at_maps_click_to_results_row() {
        // 100x40 screen: overlay box sits at x=20, y=13, 60x14.
        let screen = Rect { x: 10, y: 0, width: 100, height: 40 };
        let rect = finder_rect(screen).unwrap();
        let rows_y = rect.y + ROWS_TOP_OFFSET;

        // Clicking on the title row (y == rect.y + 1) does NOT select a
        // results row: it is chrome, not a results row.
        assert_eq!(finder_row_at(screen, rect.x + 5, rect.y + 1, 0), None);
        // Clicking on the query input row (y == rect.y + 2) likewise.
        assert_eq!(finder_row_at(screen, rect.x + 5, rect.y + 2, 0), None);
        // The first results row maps to index 0.
        assert_eq!(finder_row_at(screen, rect.x + 5, rows_y, 0), Some(0));
        // The second results row maps to index 1.
        assert_eq!(finder_row_at(screen, rect.x + 5, rows_y + 1, 0), Some(1));
        // A click outside the box entirely selects nothing.
        assert_eq!(finder_row_at(screen, 0, 0, 0), None);
        // A click on the bottom border row (rect.y + height - 1) is past
        // the results region (the last row is reserved for the border) and
        // so maps to no row.
        let bottom_border = rect.y + rect.height - 1;
        assert_eq!(finder_row_at(screen, rect.x + 5, bottom_border, 0), None);
    }

    #[test]
    fn scroll_keeps_cursor_visible_and_clicks_account_for_offset() {
        // A list longer than the overlay window. The 100x40 screen shows
        // `rows_visible` rows at once (height 14 minus chrome); a cursor
        // moved past that window must scroll the viewport, and a click on
        // the first visible screen row must map to the scrolled item, not
        // to ranked index 0 (the off-screen-selection / stale hit-test
        // fix).
        let screen = Rect { x: 0, y: 0, width: 100, height: 40 };
        let rect = finder_rect(screen).unwrap();
        let rows_y = rect.y + ROWS_TOP_OFFSET;
        let rows_visible = FinderState::rows_visible(screen);
        assert!(rows_visible >= 1, "overlay should show at least one row");

        // Build a list strictly longer than the viewport.
        let total = rows_visible + 4;
        let items: Vec<FinderItem> =
            (0..total as u64).map(|i| item(&format!("row-{i}"), None, surf(i))).collect();
        let mut finder = FinderState::new(items);

        // Move the cursor down past the bottom of the first page. After
        // each move the cursor must stay inside `[scroll, scroll +
        // rows_visible)`, so the selection is always drawn.
        for _ in 0..(rows_visible + 1) {
            finder.move_cursor(1, rows_visible);
        }
        assert_eq!(finder.cursor, rows_visible + 1);
        // The viewport followed: the cursor sits on the last visible row.
        assert_eq!(finder.scroll, 2, "viewport scrolls so cursor stays visible");
        assert!(finder.cursor >= finder.scroll);
        assert!(finder.cursor < finder.scroll + rows_visible);

        // A click on the first visible screen row maps to the ranked row at
        // the top of the viewport (scroll), NOT ranked index 0, so what's
        // drawn and what's clickable agree.
        let clicked = finder_row_at(screen, rect.x + 5, rows_y, finder.scroll).unwrap();
        assert_eq!(clicked, finder.scroll, "first visible row maps to top of viewport");
        assert_eq!(finder.items[finder.ranked()[clicked].0].target, surf(2));

        // A click on the cursor's own screen row maps back to the cursor.
        let cursor_screen_row = rows_y + (finder.cursor - finder.scroll) as u16;
        let clicked_cursor =
            finder_row_at(screen, rect.x + 5, cursor_screen_row, finder.scroll).unwrap();
        assert_eq!(clicked_cursor, finder.cursor);

        // Moving back up to the top scrolls the viewport back to 0.
        for _ in 0..(finder.cursor) {
            finder.move_cursor(-1, rows_visible);
        }
        assert_eq!(finder.cursor, 0);
        assert_eq!(finder.scroll, 0);
        assert_eq!(finder_row_at(screen, rect.x + 5, rows_y, finder.scroll), Some(0));
    }

    /// Pressing the same state-filter key twice clears the filter back to
    /// the full list (AC3 same-key toggle, AC6 unit test). A fixture list
    /// spans every agent state plus two stateless rows.
    #[test]
    fn pressing_same_state_key_twice_clears_filter() {
        let items = vec![
            item("working-agent", Some(AgentState::Working), surf(1)),
            item("blocked-agent", Some(AgentState::Blocked), surf(2)),
            item("idle-agent", Some(AgentState::Idle), surf(3)),
            item("done-agent", Some(AgentState::Done), surf(4)),
            item("workspace-one", None, FinderTarget::Workspace(1)),
            item("pane-one", None, FinderTarget::Pane(1)),
        ];
        let mut finder = FinderState::new(items.clone());
        // Start unfiltered: every row shows.
        assert_eq!(finder.state_filter, StateFilter::All);
        assert_eq!(finder.ranked().len(), items.len());

        // Press B: only the blocked agent remains (stateless rows drop out).
        finder.set_state_filter(StateFilter::Blocked);
        let ranked = finder.ranked();
        assert_eq!(ranked.len(), 1);
        assert_eq!(finder.items[ranked[0].0].label, "blocked-agent");

        // Press B again: the same-key toggle reverts to All, restoring the
        // full list including the stateless workspace and pane rows.
        finder.set_state_filter(StateFilter::Blocked);
        assert_eq!(finder.state_filter, StateFilter::All);
        assert_eq!(finder.ranked().len(), items.len());
    }

    /// `set_items` rebuilds the snapshot from a fresh tree without losing
    /// the active query, state filter, or cursor window, so an
    /// `agent-state-changed` event while the finder is open updates the
    /// list in real time (AC4 live refresh).
    #[test]
    fn set_items_refreshes_snapshot_preserving_filter() {
        let items = vec![
            item("blocked-agent", Some(AgentState::Blocked), surf(1)),
            item("working-agent", Some(AgentState::Working), surf(2)),
            item("workspace-one", None, FinderTarget::Workspace(1)),
        ];
        let mut finder = FinderState::new(items.clone());
        finder.set_state_filter(StateFilter::Blocked);
        assert_eq!(finder.ranked().len(), 1);

        // The server reports the blocked agent transitioned to working; the
        // refreshed snapshot drops the blocked row, so the active Blocked
        // filter now yields zero matches while the filter itself is kept.
        let refreshed = vec![
            item("working-agent", Some(AgentState::Working), surf(2)),
            item("workspace-one", None, FinderTarget::Workspace(1)),
        ];
        finder.set_items(refreshed);
        assert_eq!(finder.state_filter, StateFilter::Blocked);
        assert_eq!(finder.ranked().len(), 0);

        // Clearing the filter shows the refreshed full list.
        finder.set_state_filter(StateFilter::Blocked);
        assert_eq!(finder.state_filter, StateFilter::All);
        assert_eq!(finder.ranked().len(), 2);
    }

    /// Esc/Cancel on an active state filter clears the filter and keeps the
    /// finder open; a second Esc/Cancel (filter now `All`) signals the
    /// caller to close it (AC3 Esc path, round-2 blocker).
    #[test]
    fn cancel_clears_active_filter_then_closes_on_second_press() {
        let items = vec![
            item("working-agent", Some(AgentState::Working), surf(1)),
            item("blocked-agent", Some(AgentState::Blocked), surf(2)),
            item("idle-agent", Some(AgentState::Idle), surf(3)),
            item("workspace-one", None, FinderTarget::Workspace(1)),
        ];
        let mut finder = FinderState::new(items);
        finder.set_state_filter(StateFilter::Blocked);
        assert_eq!(finder.state_filter, StateFilter::Blocked);
        // There is an active filter, so the finder must NOT close yet.
        assert_ne!(finder.state_filter, StateFilter::All);

        // First Cancel: clear the filter, finder stays open.
        assert_eq!(finder.cancel(), FinderCancel::ClearedFilter);
        assert_eq!(finder.state_filter, StateFilter::All);
        assert_eq!(finder.cancel(), FinderCancel::CloseFinder);
    }

    /// A snapshot refresh that shrinks the ranked list below the current
    /// scroll offset must re-clamp `scroll` so the viewport is not blank
    /// (round-2 warning: scroll not clamped when the filtered list shrinks).
    #[test]
    fn set_items_clamps_scroll_when_list_shrinks() {
        let screen = Rect { x: 0, y: 0, width: 100, height: 40 };
        let rows_visible = FinderState::rows_visible(screen);
        assert!(rows_visible >= 1, "overlay should show at least one row");

        // Build a list longer than the viewport and scroll toward the bottom.
        let total = rows_visible + 6;
        let items: Vec<FinderItem> =
            (0..total as u64).map(|i| item(&format!("row-{i}"), None, surf(i))).collect();
        let mut finder = FinderState::new(items);
        for _ in 0..(rows_visible + 2) {
            finder.move_cursor(1, rows_visible);
        }
        assert!(finder.scroll > 0, "viewport should have scrolled down");

        // An agent-state-changed refresh shrinks the matching set well below
        // the current scroll offset.
        let shrunk: Vec<FinderItem> =
            (0..3u64).map(|i| item(&format!("shrunk-{i}"), None, surf(i))).collect();
        finder.set_items(shrunk);
        let new_len = finder.ranked().len();
        let new_max = new_len.saturating_sub(1);
        assert!(
            finder.scroll <= new_max,
            "scroll {} must be within [0, {new_max}] after shrink",
            finder.scroll
        );
        // The viewport is non-empty: at least one ranked row falls inside
        // [scroll, scroll + rows_visible).
        let visible_rows = (finder.scroll..new_len).take(rows_visible).count();
        assert!(visible_rows >= 1, "viewport must be non-empty after shrink, got {visible_rows}");
    }

    /// Switching the state filter to a narrower set must likewise re-clamp
    /// `scroll` so a previously scrolled viewport does not go blank when the
    /// ranked list shrinks under the new filter (round-2 warning).
    #[test]
    fn set_state_filter_clamps_scroll_when_list_shrinks() {
        let screen = Rect { x: 0, y: 0, width: 100, height: 40 };
        let rows_visible = FinderState::rows_visible(screen);
        assert!(rows_visible >= 1);

        // A long mix of states so narrowing the filter leaves only a few
        // matching rows down the list.
        let mut items: Vec<FinderItem> = Vec::new();
        for i in 0..(rows_visible + 6) as u64 {
            let state = if i == (rows_visible + 4) as u64 {
                Some(AgentState::Blocked)
            } else {
                Some(AgentState::Working)
            };
            items.push(item(&format!("row-{i}"), state, surf(i)));
        }
        let mut finder = FinderState::new(items);
        // Scroll toward the bottom of the full (Working) list.
        for _ in 0..(rows_visible + 2) {
            finder.move_cursor(1, rows_visible);
        }
        assert!(finder.scroll > 0);

        // Narrow to Blocked: only one row matches, far below the old scroll.
        finder.set_state_filter(StateFilter::Blocked);
        let new_len = finder.ranked().len();
        let new_max = new_len.saturating_sub(1);
        assert!(
            finder.scroll <= new_max,
            "scroll {} must be within [0, {new_max}] after narrowing",
            finder.scroll
        );
        let visible_rows = (finder.scroll..new_len).take(rows_visible).count();
        assert!(
            visible_rows >= 1,
            "viewport must be non-empty after narrowing, got {visible_rows}"
        );
    }
}
