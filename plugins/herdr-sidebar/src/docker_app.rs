//! The Docker view of the Unified sidebar: every container, running or not,
//! grouped by Compose project. Like Search, it is an activity of the unified
//! pane rather than a `View`, and it has no root folder.
//!
//! `docker ps` never runs on the event loop: one `list()` at a time goes to a
//! worker thread and `tick()` collects it with `try_recv`, so the heartbeat
//! keeps stamping however slow the daemon is. Nothing is polled while another
//! view is shown (this App only exists while Docker is on screen), and the
//! periodic refresh also waits for the pane to be focused.

use std::collections::BTreeSet;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use herdr_sidebar::docker::{self, Action, Container, DockerError, Group};
use herdr_sidebar::icons::IconTheme;
use herdr_sidebar::state::{self as sidebar, Exit, View};
use herdr_sidebar::ui::{
    ActivityHits, ActivityTab, TitleAction, draw_activity_bar, draw_scrollbar, hits, hover_style,
    palette, pane_focused_in, selection_style, title_action_spans, title_actions_width,
    truncate_to,
};

/// Periodic `docker ps` interval while the view is shown and focused.
const DOCKER_REFRESH: Duration = Duration::from_secs(3);
/// Identity re-stamp interval (the launcher's stale limit is 20 s).
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
const TABS: [ActivityTab; 4] = [
    ActivityTab::Explorer,
    ActivityTab::Search,
    ActivityTab::SourceControl,
    ActivityTab::Docker,
];
const START_DESKTOP: &str = "[ Start Docker Desktop ]";
/// Container rows sit under their group's chevron.
const CONTAINER_INDENT: &str = "   ";

/// How long a failed action's message stays up (refreshes don't clear it).
const NOTICE_FOR: Duration = Duration::from_secs(8);
/// The button in the last cells of every row.
const BUTTON_W: u16 = 3;

type ListResult = Result<Vec<Container>, DockerError>;
/// A finished start/stop/restart: the ids it covered, and how it went.
type ActionDone = (Vec<String>, Result<(), DockerError>);

/// One visible list row: a group header, or a container inside a group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowKind {
    Group(usize),
    Container(usize, usize),
}

/// Each group row, followed by its containers unless the group is collapsed.
fn visible_rows(groups: &[Group], collapsed: &BTreeSet<String>) -> Vec<RowKind> {
    let mut rows = Vec::new();
    for (g, group) in groups.iter().enumerate() {
        rows.push(RowKind::Group(g));
        if !collapsed.contains(group.key()) {
            rows.extend((0..group.containers.len()).map(|c| RowKind::Container(g, c)));
        }
    }
    rows
}

/// The action a group's button runs: stop when anything in it runs.
fn group_action(group: &Group) -> Action {
    if group.running() > 0 {
        Action::Stop
    } else {
        Action::Start
    }
}

/// What the menu of a row offers.
fn menu_actions(row: RowKind, groups: &[Group]) -> Vec<Action> {
    match row {
        RowKind::Container(g, c) => {
            if groups[g].containers[c].state.is_running() {
                vec![Action::Stop, Action::Restart]
            } else {
                vec![Action::Start]
            }
        }
        RowKind::Group(g) => {
            let running = groups[g].running();
            let mut actions = Vec::new();
            if running < groups[g].containers.len() {
                actions.push(Action::Start);
            }
            if running > 0 {
                actions.extend([Action::Stop, Action::Restart]);
            }
            actions
        }
    }
}

/// The container ids an action on this row covers.
fn targets(row: RowKind, groups: &[Group]) -> Vec<String> {
    match row {
        RowKind::Container(g, c) => vec![groups[g].containers[c].id.clone()],
        RowKind::Group(g) => groups[g].containers.iter().map(|c| c.id.clone()).collect(),
    }
}

/// Whether a click at row column `column` lands on the button.
fn button_hit(width: u16, column: u16) -> bool {
    column >= width.saturating_sub(BUTTON_W)
}

/// A popup of actions for one row (or group), opened by `m` or a right click.
struct Menu {
    actions: Vec<Action>,
    ids: Vec<String>,
    /// Top-left wanted by the opener; `draw` clamps it inside the pane.
    anchor: (u16, u16),
    selected: usize,
    rect: Rect,
}

/// A row's identity across refreshes (indices shift when containers come and
/// go): `g:<project>` for a group, `c:<container id>` for a container.
fn row_key(groups: &[Group], row: RowKind) -> String {
    match row {
        RowKind::Group(g) => format!("g:{}", groups[g].key()),
        RowKind::Container(g, c) => format!("c:{}", groups[g].containers[c].id),
    }
}

pub struct App {
    pane_id: Option<String>,
    theme: IconTheme,
    groups: Vec<Group>,
    /// Collapsed groups by `Group::key`, kept for this view's lifetime only.
    collapsed: BTreeSet<String>,
    rows: Vec<RowKind>,
    /// `None` until the first list arrives; `Some(Err)` when Docker is
    /// missing or down, which replaces the list.
    screen: Option<Result<(), DockerError>>,
    selected: Option<usize>,
    hovered: Option<usize>,
    scroll: usize,
    /// Scroll the selection into view on the next draw (keyboard moves only).
    snap: bool,
    page: usize,
    notice: Option<String>,
    /// While set and in the future, a list refresh must not clear `notice`.
    notice_until: Option<Instant>,
    list_rx: Option<Receiver<ListResult>>,
    /// Refresh on the next tick regardless of focus and interval.
    refresh_due: bool,
    /// Container ids with a start/stop/restart in flight.
    busy: BTreeSet<String>,
    action_tx: Sender<ActionDone>,
    action_rx: Receiver<ActionDone>,
    menu: Option<Menu>,
    /// Width of a list row (the pane minus the scrollbar column), from the last draw.
    row_width: u16,
    area: Rect,
    #[cfg(test)]
    recorded: Vec<(Action, Vec<String>)>,
    last_refresh: Instant,
    last_beat: Option<Instant>,
    mouse_pos: Option<(u16, u16)>,
    activity: ActivityHits,
    refresh_button: Rect,
    start_button: Rect,
    body: Rect,
}

impl App {
    pub fn new() -> Self {
        let pane_id = std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|id| !id.is_empty());
        let theme = IconTheme::resolve(
            std::env::var("HERDR_SIDEBAR_ICONS")
                .or_else(|_| std::env::var("HERDR_AA_FILETREE_ICONS"))
                .ok()
                .as_deref(),
            sidebar::load_state().icons,
        );
        // Docker only exists in the Unified sidebar; a sidebar that starts
        // here must still carry the Sidebar label the Explorer would set.
        if let Some(id) = &pane_id {
            let _ = herdr_sidebar::ipc::call_text(
                "pane.rename",
                serde_json::json!({ "pane_id": id, "label": sidebar::SIDEBAR_LABEL }),
            );
        }
        Self::with(pane_id, theme)
    }

    fn with(pane_id: Option<String>, theme: IconTheme) -> Self {
        let (action_tx, action_rx) = std::sync::mpsc::channel();
        Self {
            pane_id,
            theme,
            groups: Vec::new(),
            collapsed: BTreeSet::new(),
            rows: Vec::new(),
            screen: None,
            selected: None,
            hovered: None,
            scroll: 0,
            snap: false,
            page: 10,
            notice: None,
            notice_until: None,
            list_rx: None,
            // The view was just opened: list right away.
            refresh_due: true,
            busy: BTreeSet::new(),
            action_tx,
            action_rx,
            menu: None,
            row_width: 0,
            area: Rect::default(),
            #[cfg(test)]
            recorded: Vec::new(),
            last_refresh: Instant::now(),
            last_beat: None,
            mouse_pos: None,
            activity: ActivityHits::default(),
            refresh_button: Rect::default(),
            start_button: Rect::default(),
            body: Rect::default(),
        }
    }

    /// No pane id (so no herdr IPC), no worker thread, and `result` already
    /// collected.
    #[cfg(test)]
    fn for_test(result: ListResult) -> Self {
        let mut app = Self::with(None, IconTheme::Emoji);
        app.refresh_due = false;
        app.apply_list(result);
        app
    }

    // ---- Lifecycle ----

    /// Re-stamp the identity tokens. Self-throttled to every 5 s, so the
    /// event loop calls it on every iteration. The unified pane stamps both
    /// views' tokens, exactly as the Explorer does.
    pub fn heartbeat(&mut self) {
        if self
            .last_beat
            .is_some_and(|at| at.elapsed() < HEARTBEAT_EVERY)
        {
            return;
        }
        self.last_beat = Some(Instant::now());
        if let Some(id) = &self.pane_id {
            herdr_sidebar::ipc::report_identity(id, View::Explorer, true);
        }
    }

    pub fn clear_identity(&self) {
        if let Some(id) = &self.pane_id {
            herdr_sidebar::ipc::clear_identity(id);
        }
    }

    /// Collect a finished list; start the next one when forced, or when the
    /// interval elapsed and the pane is focused. A refresh forced while a list
    /// is in flight stays pending and runs as soon as that list lands.
    pub fn tick(&mut self) {
        self.collect_actions();
        if self.notice_until.is_some_and(|t| Instant::now() >= t) {
            self.notice_until = None;
            self.notice = None;
        }
        self.collect_list();
        if self.list_rx.is_some() {
            return;
        }
        let forced = std::mem::take(&mut self.refresh_due);
        if !forced && self.last_refresh.elapsed() < DOCKER_REFRESH {
            return;
        }
        // Restart the clock before the focus check: an unfocused pane must not
        // ask herdr for `pane.list` on every loop iteration.
        self.last_refresh = Instant::now();
        if forced || self.pane_is_focused() {
            self.request_list();
        }
    }

    /// The view draws from the frame size; nothing to re-layout here.
    pub fn on_resize(&mut self, _width: u16) {}

    fn request_list(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(docker::list());
        });
        self.list_rx = Some(rx);
    }

    fn collect_list(&mut self) {
        let Some(rx) = &self.list_rx else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.list_rx = None;
                self.apply_list(result);
            }
            Err(TryRecvError::Disconnected) => self.list_rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Collect finished actions: free their rows, list again at once, and
    /// report a failure.
    fn collect_actions(&mut self) {
        while let Ok((ids, result)) = self.action_rx.try_recv() {
            for id in &ids {
                self.busy.remove(id);
            }
            self.refresh_due = true;
            if let Err(error) = result {
                self.notice = Some(match error {
                    DockerError::Failed(message) => message,
                    DockerError::DaemonDown => "Docker isn't running".into(),
                    DockerError::NotInstalled => "Docker CLI not found".into(),
                });
                self.notice_until = Some(Instant::now() + NOTICE_FOR);
            }
        }
    }

    /// Run `action` on `ids` (minus those already busy) on a worker thread.
    /// Never blocks: `docker::run` waits up to 60 s.
    fn dispatch(&mut self, action: Action, ids: Vec<String>) {
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| !self.busy.contains(id))
            .collect();
        if ids.is_empty() {
            return;
        }
        self.busy.extend(ids.iter().cloned());
        #[cfg(test)]
        self.recorded.push((action, ids));
        #[cfg(not(test))]
        {
            let tx = self.action_tx.clone();
            std::thread::spawn(move || {
                let result = docker::run(action, &ids);
                let _ = tx.send((ids, result));
            });
        }
    }

    fn apply_list(&mut self, result: ListResult) {
        match result {
            Ok(containers) => {
                let keep = self.selected_key();
                self.groups = docker::group(containers);
                self.rebuild(keep);
                self.screen = Some(Ok(()));
                if self.notice_until.is_none() {
                    self.notice = None;
                }
            }
            // A transient failure keeps the last good list on screen.
            Err(DockerError::Failed(message)) => self.notice = Some(message),
            Err(error) => {
                self.groups.clear();
                self.rebuild(None);
                self.screen = Some(Err(error));
            }
        }
    }

    fn pane_is_focused(&self) -> bool {
        let Some(id) = &self.pane_id else { return true };
        herdr_sidebar::ipc::call_text("pane.list", serde_json::json!({}))
            .ok()
            .is_some_and(|json| pane_focused_in(&json, id))
    }

    /// Ctrl+Q (the toggle's graceful close): close our own pane.
    fn close(&self) {
        crate::flush_pending_root();
        if let Some(id) = &self.pane_id {
            let _ =
                herdr_sidebar::ipc::call_text("pane.close", serde_json::json!({ "pane_id": id }));
        }
    }

    fn start_desktop(&mut self) {
        // Spawns `open -a Docker` and returns at once; refreshing continues
        // and the list appears when the daemon answers.
        self.notice = Some(match docker::start_desktop() {
            Ok(()) => "Starting Docker Desktop…".into(),
            Err(e) => format!("Couldn't start Docker Desktop: {e}"),
        });
        self.refresh_due = true;
    }

    // ---- Leaving the view ----

    fn activate(&mut self, tab: ActivityTab) -> Option<Exit> {
        let exit = match tab {
            ActivityTab::Explorer => Exit::Show(View::Explorer),
            ActivityTab::Search => Exit::Search { focus_query: false },
            ActivityTab::SourceControl => Exit::Show(View::SourceControl),
            ActivityTab::Docker => return None,
        };
        Some(leave(exit))
    }

    // ---- Rows and selection ----

    fn selected_key(&self) -> Option<String> {
        let row = self.rows.get(self.selected?)?;
        Some(row_key(&self.groups, *row))
    }

    /// Recompute the visible rows, keeping the selection on the same group or
    /// container when it is still there (and dropping it otherwise).
    fn rebuild(&mut self, keep: Option<String>) {
        self.rows = visible_rows(&self.groups, &self.collapsed);
        self.selected = keep.and_then(|key| {
            self.rows
                .iter()
                .position(|row| row_key(&self.groups, *row) == key)
        });
        // Hover tracks the mouse, not a row: re-resolve it for the new rows.
        self.hovered = self.mouse_pos.and_then(|(x, y)| self.row_at(x, y));
    }

    fn selected_row(&self) -> Option<RowKind> {
        self.rows.get(self.selected?).copied()
    }

    /// The button's action for a row: a container's opposite state, a group's
    /// `group_action`.
    fn row_action(&self, row: RowKind) -> Action {
        match row {
            RowKind::Container(g, c) => {
                if self.groups[g].containers[c].state.is_running() {
                    Action::Stop
                } else {
                    Action::Start
                }
            }
            RowKind::Group(g) => group_action(&self.groups[g]),
        }
    }

    /// Whether `row` is a group with a member in flight (its button shows `…`).
    fn group_busy(&self, row: RowKind) -> bool {
        match row {
            RowKind::Group(g) => self.groups[g]
                .containers
                .iter()
                .any(|c| self.busy.contains(&c.id)),
            RowKind::Container(..) => false,
        }
    }

    fn run_on_row(&mut self, row: RowKind, action: Action) {
        let ids = targets(row, &self.groups);
        self.dispatch(action, ids);
    }

    fn run_selected(&mut self, action: Action) {
        if let Some(row) = self.selected_row() {
            self.run_on_row(row, action);
        }
    }

    fn open_menu(&mut self, row: RowKind, anchor: (u16, u16)) {
        let actions = menu_actions(row, &self.groups);
        if actions.is_empty() {
            return;
        }
        self.menu = Some(Menu {
            actions,
            ids: targets(row, &self.groups),
            anchor,
            selected: 0,
            rect: Rect::default(),
        });
    }

    /// Keys while the menu is open: it owns the keyboard (Esc closes the
    /// menu, never the view).
    fn menu_key(&mut self, code: KeyCode) {
        let Some(menu) = &mut self.menu else { return };
        match code {
            KeyCode::Esc => self.menu = None,
            KeyCode::Up | KeyCode::Char('k') => menu.selected = menu.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                menu.selected = (menu.selected + 1).min(menu.actions.len() - 1);
            }
            KeyCode::Enter => self.pick_menu(None),
            _ => {}
        }
    }

    /// Run the menu entry `index` (default: the highlighted one), then close.
    fn pick_menu(&mut self, index: Option<usize>) {
        if let Some(menu) = self.menu.take() {
            let last = menu.actions.len() - 1;
            let action = menu.actions[index.unwrap_or(menu.selected).min(last)];
            self.dispatch(action, menu.ids);
        }
    }

    /// The menu entry under a pane position, if it is inside the frame.
    fn menu_entry_at(&self, x: u16, y: u16) -> Option<usize> {
        let menu = self.menu.as_ref()?;
        if !hits(menu.rect, x, y) {
            return None;
        }
        let i = usize::from(y - menu.rect.y).checked_sub(1)?;
        (i < menu.actions.len()).then_some(i)
    }

    fn select(&mut self, index: usize) {
        if !self.rows.is_empty() {
            self.selected = Some(index.min(self.rows.len() - 1));
            self.snap = true;
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        // The first keyboard step on a selection-less list picks the first row.
        let Some(current) = self.selected else {
            self.select(0);
            return;
        };
        let last = self.rows.len().saturating_sub(1) as isize;
        self.select((current as isize + delta).clamp(0, last) as usize);
    }

    /// Wheel: move the VIEW only — the selection stays where it is.
    fn scroll_view(&mut self, delta: isize) {
        let max = self.rows.len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    fn set_collapsed(&mut self, group: usize, collapsed: bool) {
        let Some(key) = self.groups.get(group).map(|g| g.key().to_string()) else {
            return;
        };
        let keep = self.selected_key();
        if collapsed {
            self.collapsed.insert(key);
        } else {
            self.collapsed.remove(&key);
        }
        self.rebuild(keep);
    }

    fn toggle_group(&mut self, group: usize) {
        let collapsed = self
            .groups
            .get(group)
            .is_some_and(|g| self.collapsed.contains(g.key()));
        self.set_collapsed(group, !collapsed);
    }

    /// The row under a pane-local mouse position, if any.
    fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        if !hits(self.body, x, y) {
            return None;
        }
        let index = self.scroll + usize::from(y - self.body.y);
        (index < self.rows.len()).then_some(index)
    }

    // ---- Input ----

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('q') {
            self.close();
            return None;
        }
        if self.menu.is_some() {
            // The host's injected F9-F12 and the view keys 1-4 still leave the
            // view: they close the menu and fall through to the normal handling
            // (state is already written for the injections).
            let view_key = !key.modifiers.contains(KeyModifiers::ALT)
                && matches!(key.code, KeyCode::F(9..=12) | KeyCode::Char('1'..='4'));
            if !view_key {
                self.menu_key(key.code);
                return None;
            }
            self.menu = None;
        }
        if (ctrl && key.code == KeyCode::Char('p')) || key.code == KeyCode::F(12) {
            return Some(leave(Exit::QuickOpen));
        }
        if ctrl && matches!(key.code, KeyCode::Char('f' | 'F')) {
            return Some(leave(Exit::Search { focus_query: true }));
        }
        // 1-4 (bare or Ctrl) follow the activity bar, like the other views;
        // F9-F11 are the host's show-explorer/search/git injections.
        let tab = match key.code {
            KeyCode::F(9) | KeyCode::Char('1') => Some(ActivityTab::Explorer),
            KeyCode::F(10) | KeyCode::Char('2') => Some(ActivityTab::Search),
            KeyCode::F(11) | KeyCode::Char('3') => Some(ActivityTab::SourceControl),
            KeyCode::Char('4') => Some(ActivityTab::Docker),
            _ => None,
        };
        if let Some(tab) = tab
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            return self.activate(tab);
        }
        match key.code {
            KeyCode::Char('q') => return Some(Exit::Quit),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-(self.page as isize)),
            KeyCode::PageDown => self.move_by(self.page as isize),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(self.rows.len().saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char(' ') => {
                if matches!(self.screen, Some(Err(DockerError::DaemonDown))) {
                    self.start_desktop();
                } else if let Some(RowKind::Group(g)) = self.selected_row() {
                    self.toggle_group(g);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(RowKind::Group(g)) = self.selected_row() {
                    self.set_collapsed(g, false);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => match self.selected_row() {
                Some(RowKind::Group(g)) => self.set_collapsed(g, true),
                Some(RowKind::Container(g, _)) => {
                    if let Some(i) = self.rows.iter().position(|r| *r == RowKind::Group(g)) {
                        self.select(i);
                    }
                }
                None => {}
            },
            KeyCode::F(5) | KeyCode::Char('r') => self.refresh_due = true,
            KeyCode::Char('s') => self.run_selected(Action::Start),
            KeyCode::Char('x') => self.run_selected(Action::Stop),
            KeyCode::Char('m') => {
                if let (Some(row), Some(sel)) = (self.selected_row(), self.selected) {
                    // Just under the selected row.
                    let line = sel
                        .saturating_sub(self.scroll)
                        .min(usize::from(self.body.height));
                    self.open_menu(row, (self.body.x + 2, self.body.y + line as u16 + 1));
                }
            }
            _ => {}
        }
        None
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        let (x, y) = (mouse.column, mouse.row);
        self.mouse_pos = Some((x, y));
        if self.menu.is_some() {
            match mouse.kind {
                MouseEventKind::Moved => {
                    if let Some(i) = self.menu_entry_at(x, y)
                        && let Some(menu) = &mut self.menu
                    {
                        menu.selected = i;
                    }
                    return None;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // A click outside only closes the menu; it doesn't also act.
                    match self.menu_entry_at(x, y) {
                        Some(i) => self.pick_menu(Some(i)),
                        None => {
                            if !self.menu.as_ref().is_some_and(|m| hits(m.rect, x, y)) {
                                self.menu = None;
                            }
                        }
                    }
                    return None;
                }
                // A right click re-opens the menu on the row under it below.
                MouseEventKind::Down(MouseButton::Right) => self.menu = None,
                _ => {}
            }
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(index) = self.row_at(x, y) {
                    self.select(index);
                    self.snap = false;
                    self.open_menu(self.rows[index], (x, y));
                }
            }
            MouseEventKind::Moved => self.hovered = self.row_at(x, y),
            MouseEventKind::ScrollUp => self.scroll_view(-3),
            MouseEventKind::ScrollDown => self.scroll_view(3),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(tab) = self.activity.tab_at(x, y) {
                    return self.activate(tab);
                }
                if hits(self.refresh_button, x, y) {
                    self.refresh_due = true;
                } else if hits(self.start_button, x, y) {
                    self.start_desktop();
                } else if let Some(index) = self.row_at(x, y) {
                    self.select(index);
                    self.snap = false;
                    let row = self.rows[index];
                    let column = x - self.body.x;
                    // The scrollbar column is not part of any row.
                    if column >= self.row_width {
                        return None;
                    }
                    if button_hit(self.row_width, column) {
                        // A group button showing `…` is inert (the menu still acts).
                        if !self.group_busy(row) {
                            let action = self.row_action(row);
                            self.run_on_row(row, action);
                        }
                    } else if let RowKind::Group(g) = row {
                        self.toggle_group(g);
                    }
                } else if hits(self.body, x, y) {
                    self.selected = None;
                }
            }
            _ => {}
        }
        None
    }

    // ---- Drawing ----

    pub fn draw(&mut self, frame: &mut Frame) {
        self.area = frame.area();
        let notice_height = u16::from(self.notice.is_some());
        let [activity, header, body, notice] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(notice_height),
        ])
        .areas(frame.area());
        self.activity = draw_activity_bar(
            frame,
            activity,
            self.theme,
            &TABS,
            ActivityTab::Docker,
            false,
            self.mouse_pos,
        );
        self.draw_header(frame, header);
        self.body = body;
        self.page = usize::from(body.height.saturating_sub(1).max(1));
        self.start_button = Rect::default();
        match &self.screen {
            None => match &self.notice {
                None => message(frame, body, " Loading…"),
                // The first list failed: say why in the body, not only below.
                Some(text) => {
                    let text = truncate_to(format!(" {text}"), usize::from(body.width));
                    message(frame, body, &text);
                }
            },
            Some(Err(DockerError::NotInstalled)) => message(frame, body, " Docker CLI not found"),
            Some(Err(_)) => self.draw_daemon_down(frame, body),
            Some(Ok(())) if self.rows.is_empty() => message(frame, body, " No containers"),
            Some(Ok(())) => self.draw_list(frame, body),
        }
        if let Some(text) = &self.notice {
            let line = truncate_to(format!(" {text}"), usize::from(notice.width));
            frame.render_widget(Paragraph::new(line.fg(palette().modified)), notice);
        }
        self.draw_menu(frame);
    }

    /// The popup, clamped inside the pane.
    fn draw_menu(&mut self, frame: &mut Frame) {
        let area = self.area;
        let Some(menu) = &mut self.menu else { return };
        let label_w = menu
            .actions
            .iter()
            .map(|a| a.label().len())
            .max()
            .unwrap_or(0) as u16;
        let w = (label_w + 4).min(area.width);
        let h = (menu.actions.len() as u16 + 2).min(area.height);
        let x = menu
            .anchor
            .0
            .min(area.right().saturating_sub(w))
            .max(area.x);
        let y = menu
            .anchor
            .1
            .min(area.bottom().saturating_sub(h))
            .max(area.y);
        let rect = Rect::new(x, y, w, h);
        menu.rect = rect;
        frame.render_widget(Clear, rect);
        let block = Block::bordered();
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let lines: Vec<Line> = menu
            .actions
            .iter()
            .enumerate()
            .map(|(i, action)| {
                let pad = usize::from(inner.width).saturating_sub(1);
                let text = format!(" {:<pad$}", action.label());
                let style = if i == menu.selected {
                    selection_style(true)
                } else {
                    Style::default()
                };
                Line::styled(text, style)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        let actions = [TitleAction::Refresh];
        let button_w = title_actions_width(self.theme, &actions);
        let x = area.x + area.width.saturating_sub(button_w);
        let (button, zones) = title_action_spans(self.theme, &actions, x, area.y, self.mouse_pos);
        self.refresh_button = zones.first().map_or(Rect::default(), |(rect, _)| *rect);
        let title = Span::styled(
            truncate_to(
                " DOCKER".to_string(),
                usize::from(area.width.saturating_sub(button_w)),
            ),
            Style::default().bold().fg(palette().header_accent),
        );
        let pad = usize::from(area.width).saturating_sub(title.width() + usize::from(button_w));
        let mut spans = vec![title, Span::raw(" ".repeat(pad))];
        spans.extend(button);
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_daemon_down(&mut self, frame: &mut Frame, area: Rect) {
        message(frame, area, " Docker isn't running");
        if area.height < 3 {
            return;
        }
        let width = (Span::raw(START_DESKTOP).width() as u16).min(area.width.saturating_sub(1));
        let button = Rect::new(area.x + 1, area.y + 2, width, 1);
        let hovered = self.mouse_pos.is_some_and(|(x, y)| hits(button, x, y));
        let colors = palette();
        let bg = if hovered {
            colors.button_focus_bg
        } else {
            colors.button_bg
        };
        frame.render_widget(
            Paragraph::new(START_DESKTOP).style(Style::default().bg(bg).fg(colors.button_fg)),
            button,
        );
        self.start_button = button;
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect) {
        let h = usize::from(area.height).max(1);
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(h));
        if std::mem::take(&mut self.snap)
            && let Some(sel) = self.selected
        {
            if sel < self.scroll {
                self.scroll = sel;
            } else if sel >= self.scroll + h {
                self.scroll = sel + 1 - h;
            }
        }
        // Keep the last column for the scrollbar when the list overflows.
        let width = if self.rows.len() > h {
            area.width.saturating_sub(1)
        } else {
            area.width
        };
        self.row_width = width;
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(h)
            .map(|(i, row)| {
                let line = match *row {
                    RowKind::Group(g) => {
                        let group = &self.groups[g];
                        let busy = self.group_busy(*row);
                        group_line(group, !self.collapsed.contains(group.key()), width, busy)
                    }
                    RowKind::Container(g, c) => {
                        let container = &self.groups[g].containers[c];
                        let busy = self.busy.contains(&container.id);
                        Line::from(container_line(container, width, busy))
                    }
                };
                let item = ListItem::new(line);
                if self.selected == Some(i) {
                    item.style(selection_style(true))
                } else if self.hovered == Some(i) {
                    item.style(hover_style())
                } else {
                    item
                }
            })
            .collect();
        frame.render_widget(List::new(items), area);
        draw_scrollbar(frame, area, self.rows.len(), h, self.scroll);
    }
}

/// Persist where the user is going, then hand the exit to main. Leaving
/// Docker (rather than quitting) is what clears `docker_active`.
fn leave(exit: Exit) -> Exit {
    sidebar::update_state(|state| {
        state.docker_active = false;
        state.active = match exit {
            Exit::Show(View::SourceControl) => View::SourceControl,
            _ => View::Explorer,
        };
        state.search_active = matches!(exit, Exit::Search { .. });
    });
    exit
}

fn message(frame: &mut Frame, area: Rect, text: &str) {
    frame.render_widget(Paragraph::new(text.dim()), area);
}

/// The button's 3 cells: stop when running, start when stopped, `…` while a
/// command is in flight.
fn button_span(running: bool, busy: bool) -> Span<'static> {
    if busy {
        Span::styled(" … ", Style::default().dim())
    } else if running {
        Span::styled(" ■ ", Style::default().fg(palette().deleted))
    } else {
        Span::styled(" ▶ ", Style::default().fg(palette().added))
    }
}

/// ` ▾ project          1/3  ■ `: chevron, label, right-aligned
/// running/total, then the group's button.
fn group_line(group: &Group, expanded: bool, width: u16, busy: bool) -> Line<'static> {
    let chevron = if expanded { " ▾ " } else { " ▸ " };
    let count = format!("{}/{}", group.running(), group.containers.len());
    let right = count.len() + usize::from(BUTTON_W);
    let avail = usize::from(width).saturating_sub(Span::raw(chevron).width() + right + 1);
    let label = truncate_to(group.label().to_string(), avail);
    let pad = usize::from(width)
        .saturating_sub(Span::raw(chevron).width() + Span::raw(label.as_str()).width() + right);
    Line::from(vec![
        Span::raw(chevron),
        Span::styled(label, Style::default().bold()),
        Span::raw(" ".repeat(pad)),
        Span::styled(count, Style::default().dim()),
        button_span(group_action(group) == Action::Stop, busy),
    ])
}

/// `   ● name image    ■ `: state glyph, name, dimmed short image, then the
/// button in the last 3 cells. The image yields first when the row is narrow.
fn container_line(container: &Container, width: u16, busy: bool) -> Vec<Span<'static>> {
    let running = container.state.is_running();
    let (glyph, color) = if running {
        ("●", palette().added)
    } else {
        ("■", palette().deleted)
    };
    let fixed = CONTAINER_INDENT.len() + 2;
    let avail = usize::from(width).saturating_sub(fixed + usize::from(BUTTON_W) + 1);
    let name = truncate_to(container.name.clone(), avail);
    let name_w = Span::raw(name.as_str()).width();
    let left = avail.saturating_sub(name_w + 1);
    let image = truncate_to(docker::short_image(&container.image), left);
    let mut used = fixed + name_w;
    let mut spans = vec![
        Span::raw(CONTAINER_INDENT),
        Span::styled(glyph, Style::default().fg(color)),
        Span::raw(" "),
        Span::raw(name),
    ];
    if !image.is_empty() {
        used += 1 + Span::raw(image.as_str()).width();
        spans.push(Span::raw(" "));
        spans.push(Span::styled(image, Style::default().dim()));
    }
    let pad = usize::from(width).saturating_sub(used + usize::from(BUTTON_W));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(button_span(running, busy));
    spans
}

#[cfg(test)]
impl App {
    /// The (column, row) of the button of the container named `name`, from the
    /// last draw.
    fn button_cell_for_test(&self, name: &str) -> (u16, u16) {
        let index = self
            .rows
            .iter()
            .position(|r| {
                matches!(*r, RowKind::Container(g, c) if self.groups[g].containers[c].name == name)
            })
            .expect("container is listed");
        (
            self.body.x + self.row_width - 1,
            self.body.y + (index - self.scroll) as u16,
        )
    }

    /// A left click; returns what it queued instead of spawning.
    fn click_for_test(&mut self, x: u16, y: u16) -> Option<(Action, Vec<String>)> {
        self.recorded.clear();
        self.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
        self.recorded.pop()
    }
}

#[cfg(test)]
fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_sidebar::docker::{group, parse_ps};

    const PS: &str = "a1\tda-mongo\tmongo:7.0.43\texited\tExited (255) 4 weeks ago\ts3pweb-border-engine\n\
b2\ts3pweb-redis-stack\tredis/redis-stack:latest\trunning\tUp 50 minutes\t\n\
c3\tda-kafka\tapache/kafka:3.7.0\texited\tExited (255) 4 weeks ago\ts3pweb-border-engine\n\
d4\tda-redis\tredis/redis-stack-server:7.4.0-v3\trunning\tUp 2 minutes\ts3pweb-border-engine\n";

    fn groups() -> Vec<Group> {
        group(parse_ps(PS))
    }

    #[test]
    fn collapsed_groups_hide_their_containers() {
        let g = groups();
        assert_eq!(visible_rows(&g, &BTreeSet::new()).len(), 2 + 4);
        let collapsed = BTreeSet::from(["s3pweb-border-engine".to_string()]);
        let rows = visible_rows(&g, &collapsed);
        assert_eq!(rows.len(), 2 + 1);
        assert!(matches!(rows[1], RowKind::Group(1)));
    }

    #[test]
    fn selection_follows_the_container_across_refreshes() {
        let before = groups();
        let key = row_key(&before, RowKind::Container(1, 0)); // s3pweb-redis-stack
        let after = group(parse_ps(&PS.replace("d4\tda-redis", "zz\tda-redis-new")));
        let rows = visible_rows(&after, &BTreeSet::new());
        let found = rows.iter().position(|r| row_key(&after, *r) == key);
        assert_eq!(found.map(|i| rows[i]), Some(RowKind::Container(1, 0)));
        // a vanished row yields None, never a stale index
        assert_eq!(
            rows.iter().position(|r| row_key(&after, *r) == "c:d4"),
            None
        );
    }

    #[test]
    fn rows_show_counts_state_names_and_short_images() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        let text = render_to_string(&mut app, 32, 12);
        assert!(text.contains(" DOCKER"));
        assert!(text.contains("▾ s3pweb-border-engine"));
        assert!(text.contains("1/3"));
        assert!(text.contains("● da-redis"));
        assert!(text.contains("■ da-mongo mongo:7.0.43"));
        assert!(text.contains("Individual containers"));
        assert_eq!(app.selected, None, "nothing selected until the user picks");
    }

    #[test]
    fn a_refresh_asked_for_while_listing_runs_after_it() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        let (_tx, rx) = std::sync::mpsc::channel();
        app.list_rx = Some(rx);
        app.on_key(KeyEvent::from(KeyCode::F(5)));
        app.tick();
        assert!(app.list_rx.is_some(), "still the first list");
        assert!(app.refresh_due, "F5 is remembered, not dropped");
    }

    #[test]
    fn docker_down_screen_replaces_the_list() {
        let mut app = App::for_test(Err(DockerError::DaemonDown));
        let text = render_to_string(&mut app, 32, 20);
        assert!(text.contains("Docker isn't running"));
        assert!(text.contains("Start Docker Desktop"));
    }

    #[test]
    fn container_line_keeps_the_button_at_the_right_edge() {
        let c = &groups()[1].containers[0]; // s3pweb-redis-stack, running
        let line = Line::from(container_line(c, 32, false));
        assert_eq!(line.width(), 32);
        assert!(line.to_string().ends_with(" ■ "));
        let narrow = Line::from(container_line(c, 16, false));
        assert_eq!(narrow.width(), 16);
        assert!(narrow.to_string().ends_with(" ■ "));
        assert!(
            Line::from(container_line(c, 32, true))
                .to_string()
                .ends_with(" … ")
        );
    }

    #[test]
    fn button_hit_is_the_last_three_cells() {
        assert!(button_hit(32, 29) && button_hit(32, 31));
        assert!(!button_hit(32, 28));
    }

    #[test]
    fn menus_offer_what_applies() {
        let g = groups();
        assert_eq!(
            menu_actions(RowKind::Container(1, 0), &g),
            [Action::Stop, Action::Restart]
        );
        assert_eq!(menu_actions(RowKind::Container(0, 2), &g), [Action::Start]);
        assert_eq!(
            menu_actions(RowKind::Group(0), &g),
            [Action::Start, Action::Stop, Action::Restart]
        );
        assert_eq!(group_action(&g[0]), Action::Stop);
        assert_eq!(targets(RowKind::Group(0), &g).len(), 3);
    }

    #[test]
    fn busy_container_ignores_a_second_click() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 20);
        let (col, row) = app.button_cell_for_test("s3pweb-redis-stack");
        assert!(app.click_for_test(col, row).is_some()); // first click queues Stop
        assert!(app.click_for_test(col, row).is_none()); // second is ignored while busy
    }

    #[test]
    fn a_finished_action_frees_the_row_and_reports_failure() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 20);
        let (col, row) = app.button_cell_for_test("s3pweb-redis-stack");
        let (action, ids) = app.click_for_test(col, row).unwrap();
        assert_eq!(
            (action, ids.clone()),
            (Action::Stop, vec!["b2".to_string()])
        );
        app.action_tx
            .send((ids, Err(DockerError::Failed("boom".into()))))
            .unwrap();
        app.collect_actions(); // not tick(): that would start a real `docker ps`
        assert!(app.busy.is_empty());
        assert!(app.refresh_due);
        assert_eq!(app.notice.as_deref(), Some("boom"));
        // the forced refresh landing must not wipe the message
        app.apply_list(Ok(parse_ps(PS)));
        assert_eq!(app.notice.as_deref(), Some("boom"));
    }

    #[test]
    fn right_click_menu_runs_an_entry_and_esc_only_closes_it() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 20);
        let (_, row) = app.button_cell_for_test("s3pweb-redis-stack");
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 6,
            row,
            modifiers: KeyModifiers::NONE,
        });
        let text = render_to_string(&mut app, 32, 20);
        assert!(text.contains("Restart"));
        assert!(app.on_key(KeyEvent::from(KeyCode::Esc)).is_none());
        assert!(app.menu.is_none());
        // `m` on the selection opens the same menu; Down + Enter picks Restart
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        render_to_string(&mut app, 32, 20);
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            app.recorded.pop(),
            Some((Action::Restart, vec!["b2".to_string()]))
        );
        assert!(app.menu.is_none());
    }

    #[test]
    fn click_outside_closes_the_menu_without_acting() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 20);
        let (col, row) = app.button_cell_for_test("s3pweb-redis-stack");
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 6,
            row,
            modifiers: KeyModifiers::NONE,
        });
        render_to_string(&mut app, 32, 20);
        assert!(app.menu.is_some());
        assert!(app.click_for_test(col, 19).is_none());
        assert!(app.menu.is_none());
    }

    #[test]
    fn s_and_x_act_on_the_selected_group() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 20);
        app.on_key(KeyEvent::from(KeyCode::Down)); // first group
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        assert_eq!(
            app.recorded.pop().map(|(a, ids)| (a, ids.len())),
            Some((Action::Stop, 3))
        );
    }

    /// `leave` persists state: point it at a scratch dir, never the user's.
    fn isolate_state() {
        let dir =
            std::env::temp_dir().join(format!("herdr-docker-app-test-{}", std::process::id()));
        // SAFETY: only tests read this variable, and every writer stores the same value.
        unsafe { std::env::set_var("HERDR_PLUGIN_STATE_DIR", dir) };
    }

    #[test]
    fn a_group_button_showing_busy_ignores_clicks() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 20);
        let button = (app.body.x + app.row_width - 1, app.body.y); // group 0, first row
        assert!(
            app.click_for_test(button.0, button.1).is_some(),
            "idle group acts"
        );
        app.busy.clear();
        app.dispatch(Action::Start, vec!["a1".to_string()]);
        render_to_string(&mut app, 32, 20);
        assert!(app.click_for_test(button.0, button.1).is_none());
        assert!(
            app.collapsed.is_empty(),
            "and doesn't toggle the group either"
        );
        // the group is still selected; `x` acts on the members that are free
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        assert_eq!(
            app.recorded.pop().map(|(a, ids)| (a, ids.len())),
            Some((Action::Stop, 2))
        );
    }

    #[test]
    fn clicking_the_scrollbar_column_does_not_toggle_a_group() {
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 9); // 6 rows in a 5-row body: overflows
        assert_eq!(app.row_width, 31);
        assert!(app.click_for_test(app.body.x + 31, app.body.y).is_none());
        assert!(app.collapsed.is_empty());
        // a click on the group's label still toggles it
        app.click_for_test(app.body.x + 5, app.body.y);
        assert_eq!(app.collapsed.len(), 1);
    }

    #[test]
    fn host_view_keys_pass_through_an_open_menu() {
        isolate_state();
        let mut app = App::for_test(Ok(parse_ps(PS)));
        render_to_string(&mut app, 32, 20);
        app.on_key(KeyEvent::from(KeyCode::Down));
        let plain = app.on_key(KeyEvent::from(KeyCode::F(9)));
        assert_eq!(plain, Some(Exit::Show(View::Explorer)));
        for (code, expected) in [
            (KeyCode::F(9), Exit::Show(View::Explorer)),
            (KeyCode::F(11), Exit::Show(View::SourceControl)),
            (KeyCode::Char('3'), Exit::Show(View::SourceControl)),
            (KeyCode::F(12), Exit::QuickOpen),
        ] {
            app.on_key(KeyEvent::from(KeyCode::Char('m')));
            assert!(app.menu.is_some());
            assert_eq!(app.on_key(KeyEvent::from(code)), Some(expected));
            assert!(app.menu.is_none(), "{code:?} closes the menu");
        }
        // other keys stay with the menu
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        assert_eq!(app.on_key(KeyEvent::from(KeyCode::Char('q'))), None);
        assert!(app.menu.is_some());
    }

    #[test]
    fn a_failed_first_list_shows_its_message_in_the_body() {
        let mut app = App::for_test(Err(DockerError::Failed("docker ps timed out".into())));
        let text = render_to_string(&mut app, 32, 12);
        assert!(!text.contains("Loading"));
        let body: Vec<&str> = text.lines().skip(4).take(7).collect();
        assert!(
            body.iter().any(|l| l.contains("docker ps timed out")),
            "message is in the body, not only the notice: {body:?}"
        );
    }
}
