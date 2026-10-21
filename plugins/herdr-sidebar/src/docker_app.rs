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
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph};

use herdr_sidebar::docker::{self, Container, DockerError, Group};
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

type ListResult = Result<Vec<Container>, DockerError>;

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
    list_rx: Option<Receiver<ListResult>>,
    /// Refresh on the next tick regardless of focus and interval.
    refresh_due: bool,
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
            list_rx: None,
            // The view was just opened: list right away.
            refresh_due: true,
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
    /// interval elapsed and the pane is focused.
    pub fn tick(&mut self) {
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

    /// A list is in flight; the event loop polls faster to show it promptly.
    pub fn is_loading(&self) -> bool {
        self.list_rx.is_some()
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

    fn apply_list(&mut self, result: ListResult) {
        match result {
            Ok(containers) => {
                let keep = self.selected_key();
                self.groups = docker::group(containers);
                self.rebuild(keep);
                self.screen = Some(Ok(()));
                self.notice = None;
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
            _ => {}
        }
        None
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit> {
        let (x, y) = (mouse.column, mouse.row);
        self.mouse_pos = Some((x, y));
        match mouse.kind {
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
                    if let RowKind::Group(g) = self.rows[index] {
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
            None if self.notice.is_none() => message(frame, body, " Loading…"),
            None => {}
            Some(Err(DockerError::NotInstalled)) => message(frame, body, " Docker CLI not found"),
            Some(Err(_)) => self.draw_daemon_down(frame, body),
            Some(Ok(())) if self.rows.is_empty() => message(frame, body, " No containers"),
            Some(Ok(())) => self.draw_list(frame, body),
        }
        if let Some(text) = &self.notice {
            let line = truncate_to(format!(" {text}"), usize::from(notice.width));
            frame.render_widget(Paragraph::new(line.fg(palette().modified)), notice);
        }
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
                        group_line(group, !self.collapsed.contains(group.key()), width)
                    }
                    RowKind::Container(g, c) => {
                        container_line(&self.groups[g].containers[c], width)
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

fn message(frame: &mut Frame, area: Rect, text: &'static str) {
    frame.render_widget(Paragraph::new(text.dim()), area);
}

/// ` ▾ project            1/3 `: chevron, label, right-aligned running/total.
fn group_line(group: &Group, expanded: bool, width: u16) -> Line<'static> {
    let chevron = if expanded { " ▾ " } else { " ▸ " };
    let count = format!("{}/{} ", group.running(), group.containers.len());
    let avail = usize::from(width).saturating_sub(Span::raw(chevron).width() + count.len() + 1);
    let label = truncate_to(group.label().to_string(), avail);
    let pad = usize::from(width).saturating_sub(
        Span::raw(chevron).width() + Span::raw(label.as_str()).width() + count.len(),
    );
    Line::from(vec![
        Span::raw(chevron),
        Span::styled(label, Style::default().bold()),
        Span::raw(" ".repeat(pad)),
        Span::styled(count, Style::default().dim()),
    ])
}

/// `   ● name image`: state glyph, name, dimmed short image. The image yields
/// first when the row is narrow.
fn container_line(container: &Container, width: u16) -> Line<'static> {
    let (glyph, color) = if container.state.is_running() {
        ("●", palette().added)
    } else {
        ("■", palette().deleted)
    };
    let fixed = CONTAINER_INDENT.len() + 2;
    let avail = usize::from(width).saturating_sub(fixed + 1);
    let name = truncate_to(container.name.clone(), avail);
    let left = avail.saturating_sub(Span::raw(name.as_str()).width() + 1);
    let image = truncate_to(docker::short_image(&container.image), left);
    let mut spans = vec![
        Span::raw(CONTAINER_INDENT),
        Span::styled(glyph, Style::default().fg(color)),
        Span::raw(" "),
        Span::raw(name),
    ];
    if !image.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(image, Style::default().dim()));
    }
    Line::from(spans)
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
    fn docker_down_screen_replaces_the_list() {
        let mut app = App::for_test(Err(DockerError::DaemonDown));
        let text = render_to_string(&mut app, 32, 20);
        assert!(text.contains("Docker isn't running"));
        assert!(text.contains("Start Docker Desktop"));
    }
}
