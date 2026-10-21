# Docker View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A fourth activity-bar view, Docker, in the Unified sidebar that lists all containers grouped by
Compose project and starts/stops them with one click or a right-click menu.

**Architecture:** Docker follows the Search precedent rather than becoming a third `View`: the two-view
`View` enum (and the 81 places that rely on it for separated-pane management) stays untouched. A
`docker_active` flag in shared state (like `search_active`) plus two new `Exit` variants let `main.rs` run a
third app, `docker_app::App`, in the same pane. Pure Docker logic (CLI parsing, grouping, commands) lives in
a new library module `docker.rs`; the activity bar, today duplicated in both apps, becomes one shared
`ui::draw_activity_bar` with four tabs.

**Tech Stack:** Rust (stable), ratatui 0.30, crossterm 0.29, serde_json; Docker CLI.

**Spec:** `docs/superpowers/specs/2026-10-21-docker-view-design.md`

All paths below are relative to `plugins/herdr-sidebar/` unless they start with `docs/` or `~`. Run every
`cargo` command from `plugins/herdr-sidebar/`.

## Global Constraints

- Docker is queried only while the Docker view is on screen and the sidebar pane is focused; at most one
  `docker ps` per 3 s (`DOCKER_REFRESH = 3s`). Nothing Docker-related runs in Explorer/Search/Source Control.
- No `docker` command ever runs on the UI thread: spawn a thread, return results over `std::sync::mpsc`,
  collect with `try_recv` in `tick()`. The heartbeat keeps stamping every 5 s.
- Only in Unified sidebar mode (`state.merged`): in separated mode the Docker icon is not drawn and `4`
  does nothing.
- Activity-bar order = number keys = VS Code order: 1 Explorer, 2 Search, 3 Source Control, 4 Docker.
  `Ctrl+4` works from any focus, bare `4` under the same conditions as bare `1`/`2`/`3`.
- Nothing highlighted until the user selects; mouse wheel scrolls the view only; scrollbar on overflow;
  Esc never exits (only `q`); `m` and right-click open the same menu; settings writes only through
  `state::update_state`; UI copy says "Unified sidebar", never "merge".
- Colors only from `ui::palette()`: running = `palette().added`, stopped = `palette().deleted`.
- No confirmation before stop. Commands: `docker ps -a --format <TSV>`, `docker start|stop|restart <ids…>`,
  `open -a Docker`.
- `cargo test` and `cargo clippy --all-targets -- -D warnings` pass at the end of every task.
- Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; branch `feature/docker-view`.

## Review Focus

- **Docker Desktop quit mid-session** → the view swaps to "Docker isn't running" + Start Docker Desktop
  instead of an error loop or a frozen list (test: `daemon_errors_are_classified_as_daemon_down` in Task 2,
  `docker_down_screen_replaces_the_list` in Task 4).
- **`docker` not on the pane's PATH** (herdr panes may not inherit the login PATH; the binary is
  `~/.docker/bin/docker`) → still found (test: `docker_bin_falls_back_to_known_locations`, Task 2).
- **A long name in a 32-column sidebar** → the name is truncated, the ▶/■ button stays at the right edge
  and clickable (test: `container_line_keeps_the_button_at_the_right_edge`, Task 5).
- **Double click on ▶ while the start is still running** → one `docker start`, not two (test:
  `busy_container_ignores_a_second_click`, Task 5).
- **Containers appearing/disappearing between refreshes** (a project brought `down` elsewhere) → selection
  and collapsed state survive by key, never index out of bounds (test:
  `selection_follows_the_container_across_refreshes`, Task 4).

---

### Task 1: Toolchain, then the Docker model

**Files:**
- Create: `src/docker.rs`
- Modify: `src/lib.rs` (add `pub mod docker;`)

**Interfaces:**
- Produces:
  - `pub enum ContainerState { Running, Paused, Restarting, Exited, Created, Dead, Other }` with
    `pub fn is_running(self) -> bool` (Running, Paused, Restarting → true).
  - `pub struct Container { pub id: String, pub name: String, pub image: String, pub state: ContainerState, pub status: String, pub project: Option<String> }`
  - `pub const PS_FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Label \"com.docker.compose.project\"}}";`
  - `pub fn parse_ps(output: &str) -> Vec<Container>` — one container per non-empty line; empty project
    label → `None`; lines with fewer than 5 fields are skipped.
  - `pub struct Group { pub project: Option<String>, pub containers: Vec<Container> }` with
    `pub fn running(&self) -> usize`, `pub fn key(&self) -> &str` (project name, or `""` for individuals),
    `pub fn label(&self) -> &str` (project name, or `"Individual containers"`).
  - `pub fn group(containers: Vec<Container>) -> Vec<Group>` — projects sorted by name, the individual
    group last (omitted when empty); inside a group running first, then by name.
  - `pub fn short_image(image: &str) -> String`

- [ ] **Step 1: Install Rust and check the baseline**

Run: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --no-modify-path && source ~/.cargo/env && cargo --version`
Expected: `cargo 1.x`. (`--no-modify-path`: every later command runs `source ~/.cargo/env` first; don't edit the user's shell files.) Then from `plugins/herdr-sidebar/`: `cargo test` and `cargo clippy --all-targets -- -D warnings` pass on the untouched branch. If the baseline fails, stop and report; do not fix upstream code.

- [ ] **Step 2: Write the failing tests** (inline `#[cfg(test)] mod tests` in `src/docker.rs`)

```rust
const PS: &str = "a1\tda-mongo\tmongo:7.0.43\texited\tExited (255) 4 weeks ago\ts3pweb-border-engine\n\
b2\ts3pweb-redis-stack\tredis/redis-stack:latest\trunning\tUp 50 minutes\t\n\
c3\tda-kafka\tapache/kafka:3.7.0\texited\tExited (255) 4 weeks ago\ts3pweb-border-engine\n\
d4\tda-redis\tredis/redis-stack-server:7.4.0-v3\trunning\tUp 2 minutes\ts3pweb-border-engine\n";

#[test]
fn parse_ps_reads_every_field() {
    let c = parse_ps(PS);
    assert_eq!(c.len(), 4);
    assert_eq!(c[0].name, "da-mongo");
    assert_eq!(c[0].state, ContainerState::Exited);
    assert_eq!(c[0].project.as_deref(), Some("s3pweb-border-engine"));
    assert_eq!(c[1].project, None);
    assert!(c[1].state.is_running());
}

#[test]
fn parse_ps_skips_blank_and_short_lines() {
    assert!(parse_ps("\n\nonly\ttwo\n").is_empty());
}

#[test]
fn group_puts_projects_first_and_running_first_inside() {
    let g = group(parse_ps(PS));
    assert_eq!(g.len(), 2);
    assert_eq!(g[0].label(), "s3pweb-border-engine");
    assert_eq!(g[0].running(), 1);
    assert_eq!(g[0].containers.len(), 3);
    assert_eq!(g[0].containers[0].name, "da-redis"); // the running one
    assert_eq!(g[0].containers[1].name, "da-kafka"); // then by name
    assert_eq!(g[1].label(), "Individual containers");
    assert_eq!(g[1].key(), "");
}

#[test]
fn short_image_drops_registry_and_namespace() {
    assert_eq!(short_image("apache/kafka:3.7.0"), "kafka:3.7.0");
    assert_eq!(short_image("docker.io/redis/redis-stack-server:7.4.0-v3"), "redis-stack-server:7.4.0-v3");
    assert_eq!(short_image("localhost:5000/app:dev"), "app:dev");
    assert_eq!(short_image("mongo@sha256:0123456789abcdef"), "mongo");
    assert_eq!(short_image("mongo"), "mongo");
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test --lib docker::`
Expected: compile errors (`parse_ps`, `group`, … not found).

- [ ] **Step 4: Implement the Interfaces above in `src/docker.rs`; add `pub mod docker;` to `src/lib.rs`**

`short_image`: strip any `@digest`, then keep the last `/`-separated segment (a registry `host:port/`
prefix disappears with the namespace). `ContainerState` parses Docker's lowercase `.State` words; anything
else → `Other`.

- [ ] **Step 5: Run the tests, then the whole suite and clippy**

Run: `cargo test --lib docker:: && cargo test && cargo clippy --all-targets -- -D warnings`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/docker.rs src/lib.rs
git commit -m "feat(docker): parse and group docker ps output"
```

---

### Task 2: Running Docker commands

**Files:**
- Modify: `src/docker.rs`

**Interfaces:**
- Consumes: `Container`, `parse_ps`, `PS_FORMAT` (Task 1).
- Produces:
  - `pub enum DockerError { NotInstalled, DaemonDown, Failed(String) }`
  - `pub fn classify_failure(stderr: &str) -> DockerError` — `DaemonDown` when stderr contains (case
    insensitive) `cannot connect to the docker daemon`, `is the docker daemon running`, or
    `error during connect`; else `Failed(first non-empty stderr line, trimmed)`.
  - `pub fn docker_bin_in(path_var: Option<&str>, home: Option<&Path>, exists: impl Fn(&Path) -> bool) -> Option<PathBuf>`
    — first `docker` found in `path_var` entries, then `~/.docker/bin/docker`, `/usr/local/bin/docker`,
    `/opt/homebrew/bin/docker`. `pub fn docker_bin() -> Option<PathBuf>` calls it with the real env and
    `Path::is_file`.
  - `pub enum Action { Start, Stop, Restart }` with `pub fn verb(self) -> &'static str` (`"start"`,
    `"stop"`, `"restart"`) and `pub fn label(self) -> &'static str` (`"Start"`, `"Stop"`, `"Restart"`).
  - `pub fn action_args(action: Action, ids: &[String]) -> Vec<String>` — `[verb, ids…]`.
  - `pub fn list() -> Result<Vec<Container>, DockerError>` — runs `docker ps -a --format PS_FORMAT`.
  - `pub fn run(action: Action, ids: &[String]) -> Result<(), DockerError>`
  - `pub fn start_desktop() -> std::io::Result<()>` — spawns `open -a Docker` without waiting.
  These block; callers run them on a worker thread.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn daemon_errors_are_classified_as_daemon_down() {
    for s in [
        "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?",
        "error during connect: Get \"http://%2Fvar%2Frun%2Fdocker.sock/v1.47/containers/json\": EOF",
    ] {
        assert!(matches!(classify_failure(s), DockerError::DaemonDown), "{s}");
    }
    assert!(matches!(
        classify_failure("\nError response from daemon: No such container: x\n"),
        DockerError::Failed(m) if m == "Error response from daemon: No such container: x"
    ));
}

#[test]
fn docker_bin_falls_back_to_known_locations() {
    let home = Path::new("/Users/me");
    let only = |want: &'static str| move |p: &Path| p == Path::new(want);
    assert_eq!(
        docker_bin_in(Some("/usr/bin:/bin"), Some(home), only("/Users/me/.docker/bin/docker")),
        Some(PathBuf::from("/Users/me/.docker/bin/docker"))
    );
    assert_eq!(
        docker_bin_in(Some("/x:/y"), Some(home), only("/y/docker")),
        Some(PathBuf::from("/y/docker"))
    );
    assert_eq!(docker_bin_in(None, None, |_| false), None);
}

#[test]
fn action_args_put_the_verb_first() {
    let ids = vec!["a1".to_string(), "c3".to_string()];
    assert_eq!(action_args(Action::Stop, &ids), ["stop", "a1", "c3"]);
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib docker::`
Expected: compile errors for the new names.

- [ ] **Step 3: Implement the Interfaces above**

`list`/`run`: `docker_bin()` → `NotInstalled` when `None`; non-zero exit → `classify_failure(stderr)`;
spawn failure → `Failed(error text)`.

- [ ] **Step 4: Run tests and clippy**

Run: `cargo test --lib docker:: && cargo clippy --all-targets -- -D warnings`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add src/docker.rs
git commit -m "feat(docker): run docker list/start/stop/restart off a resolved binary"
```

---

### Task 3: One shared activity bar with four tabs

**Files:**
- Modify: `src/ui.rs` (`activity_icons` :450, new types, tests near :788)
- Modify: `src/explorer_app.rs` (`ActivityZones` :408-426, `draw_activity_bar` :3506-3596, activity click
  handling :1171-1185, gear hit-test using `self.gear`)
- Modify: `src/scm_app.rs` (`zones.explorer/search/source_control/activity_row/gear`, `draw_activity_bar`
  :4151-4241, its click handling :1811-1817)

**Interfaces:**
- Produces (in `ui.rs`):
  - `pub enum ActivityTab { Explorer, Search, SourceControl, Docker }` (`Clone, Copy, PartialEq, Eq, Debug`)
  - `pub fn activity_icons(theme: IconTheme) -> [&'static str; 4]` — Material:
    `["\u{f07b}", "\u{f002}", "\u{f126}", "\u{f308}"]` (f308 = nf-linux-docker); Emoji:
    `["📁", "🔍", "🔀", "🐳"]`.
  - `pub struct ActivityHits { pub row: u16, pub tabs: Vec<(ActivityTab, (u16, u16))>, pub gear: Rect }`
    with `Default` (`row: u16::MAX`, empty tabs) and
    `pub fn tab_at(&self, x: u16, y: u16) -> Option<ActivityTab>` (uses `hits_activity_button`) and
    `pub fn gear_hit(&self, x: u16, y: u16) -> bool`.
  - `pub fn activity_layout(area: Rect, theme: IconTheme, tabs: &[ActivityTab], show_gear: bool) -> ActivityHits`
    — the pure geometry (span widths, gear at the right edge, 3 rows tall from `area.y`); without the gear,
    `gear` is `Rect::default()` and `gear_hit` is always false.
  - `pub fn draw_activity_bar(frame: &mut Frame, area: Rect, theme: IconTheme, tabs: &[ActivityTab], active: ActivityTab, show_gear: bool, mouse_pos: Option<(u16, u16)>) -> ActivityHits`
    — exactly today's rendering (chips, half-block caps, hover, gear), generalized to `tabs`.
- Explorer and Source Control pass `show_gear: true`. The Docker view (Task 4) passes `false`: its first
  version has no settings, and a gear that does nothing would be a dead button.
- Callers pass `tabs` = all four when `merged()`, else the three without `Docker`.

- [ ] **Step 1: Write the failing tests** (in `ui.rs` tests)

```rust
#[test]
fn activity_layout_places_four_tabs_in_order_and_the_gear_at_the_edge() {
    use ActivityTab::*;
    let hits = activity_layout(Rect::new(0, 0, 32, 3), IconTheme::Material,
        &[Explorer, Search, SourceControl, Docker], true);
    assert_eq!(hits.row, 1);
    let order: Vec<_> = hits.tabs.iter().map(|(t, _)| *t).collect();
    assert_eq!(order, [Explorer, Search, SourceControl, Docker]);
    let (start, end) = hits.tabs[3].1;
    assert!(end <= 32 - hits.gear.width, "docker chip overlaps the gear");
    assert_eq!(hits.tab_at(start, 1), Some(Docker));
    assert_eq!(hits.tab_at(start, 0), Some(Docker)); // all three highlight rows
    assert_eq!(hits.gear.x + hits.gear.width, 32);
    assert!(hits.gear_hit(31, 1));
}

#[test]
fn activity_layout_without_docker_has_three_tabs() {
    use ActivityTab::*;
    let hits = activity_layout(Rect::new(0, 0, 32, 3), IconTheme::Emoji, &[Explorer, Search, SourceControl], false);
    assert_eq!(hits.tabs.len(), 3);
    assert_eq!(hits.tab_at(30, 1), None);
    assert!(!hits.gear_hit(31, 1));
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --lib ui::`
Expected: compile errors for `ActivityTab`/`activity_layout`.

- [ ] **Step 3: Implement in `ui.rs`, then replace both apps' `draw_activity_bar` bodies with a call to
  `ui::draw_activity_bar`**

Explorer passes `active = Search` when its content-search overlay is open, else `Explorer`; SCM passes
`SourceControl`. Each app stores the returned `ActivityHits` (replacing `ActivityZones` / the `zones`
activity fields and `gear`) and its click handler matches on `hits.tab_at(...)`. A click on `Docker` is a
no-op in this task (wired in Task 4).

- [ ] **Step 4: Run the whole suite and clippy**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: pass, including the existing activity-bar and `unified_no_repo_layout_keeps_the_activity_bar_visible` tests.

- [ ] **Step 5: Commit**

```bash
git add src/ui.rs src/explorer_app.rs src/scm_app.rs
git commit -m "refactor(ui): one shared activity bar, with a Docker tab in the Unified sidebar"
```

---

### Task 4: The Docker view: switching, list, refresh

**Files:**
- Modify: `src/state.rs` (`State.docker_active` near :277, default :335, serialize :516, parse :914, `Exit` :88, round-trip test JSON :1128)
- Create: `src/docker_app.rs` (binary module, `mod docker_app;` in `src/main.rs`)
- Modify: `src/main.rs` (startup screen choice near :255-285, the loop :271-307, new `run_docker`)
- Modify: `src/explorer_app.rs`, `src/scm_app.rs` (`switch_to` also clears `docker_active`; new
  `open_docker`; key `4`/`Ctrl+4`/Docker-tab click)

**Interfaces:**
- Consumes: `docker::{list, group, Group, Container, DockerError, short_image}` (Tasks 1-2);
  `ui::{draw_activity_bar, ActivityTab, ActivityHits}` (Task 3).
- Produces:
  - `State.docker_active: bool`, JSON key `"docker_active"`, default `false`.
  - `Exit::Docker` (show the Docker view) and `Exit::Show(View)` (leave Docker for Explorer or Source Control).
  - In both apps: `fn open_docker(&mut self) -> Option<Exit>` — `None` unless `merged()`; else
    `update_state(|s| { s.docker_active = true; s.search_active = false; })` and `Some(Exit::Docker)`.
  - `docker_app::App` with `pub fn new() -> Self`, `pub fn draw(&mut self, frame: &mut Frame)`,
    `pub fn on_key(&mut self, key: KeyEvent) -> Option<Exit>`, `pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<Exit>`,
    `pub fn on_resize(&mut self, width: u16)`, `pub fn heartbeat(&mut self)`, `pub fn tick(&mut self)`,
    `pub fn clear_identity(&self)`.
  - In `docker_app.rs`: `enum RowKind { Group(usize), Container(usize, usize) }` and
    `fn visible_rows(groups: &[Group], collapsed: &BTreeSet<String>) -> Vec<RowKind>` (group row, then its
    containers unless `collapsed` contains `group.key()`); `fn row_key(groups: &[Group], row: RowKind) -> String`
    (`"g:<key>"` / `"c:<id>"`), used to restore selection after a refresh.

Behavior pinned for this task:
- `main.rs`: start in Docker when `pinned.is_none() && persisted.merged && persisted.docker_active`.
  `Exit::Docker` → run Docker; `Exit::Show(v)` → `view = v`; `Exit::Search{..}`/`QuickOpen` from Docker
  land in Explorer as today. `run_docker` polls every 500 ms (heartbeat cadence like `run_explorer`).
- Leaving Docker (`1`/`3`/`Ctrl+1`/`Ctrl+3`/tab click → `Show(Explorer|SourceControl)`, `2`/Search tab →
  `Search { focus_query: false }`) writes `docker_active = false` (and `active`/`search_active` like
  `switch_to`/`open_search` do) before returning. `q` → `Exit::Quit`.
- `heartbeat`: every 5 s, `herdr_sidebar::ipc::report_identity(&pane_id, View::Explorer, true)` (merged
  stamps both views' tokens), pane id from `HERDR_PANE_ID`.
- `tick`: collect a finished `list()` from the worker's channel; start a new one when none is in flight and
  either a refresh was forced (view just opened, ⟳ clicked, `F5`) or `DOCKER_REFRESH` (3 s) elapsed **and**
  the pane is focused (`pane.list` → `ui::pane_focused_in`, the Explorer's `pane_is_focused` logic).
  `pane_focused_in(pane_list_json: &str, pane_id: &str) -> bool` is today a private copy in both
  `explorer_app.rs:4558` and `scm_app.rs:5458`: move it once into `ui.rs` as `pub`, and have all three apps
  use it (its existing tests move with it).
- Layout top to bottom: activity bar (active `Docker`, 3 rows), header line ` DOCKER` with the
  `title_action_icon(theme, TitleAction::Refresh)` button at the right, the list (scrollbar on overflow),
  a one-line notice at the bottom when set.
- Group row: `▾`/`▸`, label, right-aligned `running/total`. Container row (indented 3): state glyph
  (`●` in `palette().added` when running, `■` in `palette().deleted` otherwise), name, dimmed
  `short_image`. (The ▶/■ button arrives in Task 5.)
- Keys: Up/Down/`j`/`k` select (first press selects row 0), Enter/Space/Left/Right/`h`/`l` toggle a group,
  `F5` refresh, wheel scrolls without moving the selection. Left click on a group row toggles it; on a
  container row selects it.
- `Err(NotInstalled)` → body says `Docker CLI not found`; `Err(DaemonDown)` → `Docker isn't running` and a
  `[ Start Docker Desktop ]` button (click or Enter → `docker::start_desktop()` on a thread, notice
  `Starting Docker Desktop…`; refreshing continues); `Err(Failed(m))` → notice `m`, keep the last list.

- [ ] **Step 1: Write the failing tests**

In `state.rs` tests: in the round-trip test ending at :1129, add `docker_active: true` to the expected
`State` literal and `"docker_active":true` to its JSON; add

```rust
#[test]
fn docker_active_defaults_to_false_when_absent() {
    let s = parse_state("{\"merged\":true,\"active\":\"explorer\"}");
    assert!(!s.docker_active);
}
```

In `docker_app.rs` tests:

```rust
fn groups() -> Vec<Group> { group(parse_ps(PS)) } // PS: the Task 1 fixture, copied

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
    assert_eq!(rows.iter().position(|r| row_key(&after, *r) == "c:d4"), None);
}

#[test]
fn docker_down_screen_replaces_the_list() {
    let mut app = App::for_test(Err(DockerError::DaemonDown));
    let text = render_to_string(&mut app, 32, 20);
    assert!(text.contains("Docker isn't running"));
    assert!(text.contains("Start Docker Desktop"));
}
```

`App::for_test(result)` builds an App with no pane id and that list result already collected;
`render_to_string` draws into a `ratatui::backend::TestBackend` and joins the buffer's rows (both
`#[cfg(test)]` helpers in `docker_app.rs`).

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test docker_active && cargo test --bin herdr-sidebar docker_app::`
Expected: compile errors / failures.

- [ ] **Step 3: Implement: `State.docker_active`, `Exit::Docker`/`Exit::Show`, `open_docker` + key/click
  wiring in both apps, `docker_app.rs`, and the `main.rs` loop**

- [ ] **Step 4: Run the whole suite and clippy**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs src/docker_app.rs src/main.rs src/explorer_app.rs src/scm_app.rs
git commit -m "feat(docker): Docker view in the Unified sidebar, listing containers by project"
```

---

### Task 5: Start and stop: row buttons, menu, keys

**Files:**
- Modify: `src/docker_app.rs`

**Interfaces:**
- Consumes: `docker::{run, Action, Group, Container}`; `App`, `RowKind`, `visible_rows` (Task 4).
- Produces (in `docker_app.rs`):
  - `fn container_line(c: &Container, width: u16, busy: bool) -> Vec<Span<'static>>` — indent, state glyph,
    name and image truncated to fit, then the button in the last 3 cells: ` ▶ ` when stopped, ` ■ ` when
    running, ` … ` when busy.
  - `fn button_hit(width: u16, column: u16) -> bool` — `column >= width - 3` (the button's 3 cells).
  - `fn group_action(g: &Group) -> Action` — `Stop` if any container running, else `Start`.
  - `fn menu_actions(row: RowKind, groups: &[Group]) -> Vec<Action>` — running container: `[Stop, Restart]`;
    stopped: `[Start]`; group: `[Start]` if any stopped, then `[Stop, Restart]` if any running.
  - `fn targets(row: RowKind, groups: &[Group]) -> Vec<String>` — the container's id, or every id in the group.
  - `busy: BTreeSet<String>` of container ids with a command in flight.

Behavior pinned for this task:
- Left click on a row's button runs the row's action (container: its state's opposite; group:
  `group_action`) on a worker thread; its targets go into `busy` until the result arrives, then a refresh is
  forced. A click on a busy row's button is ignored. `Err` → notice with the message.
- Right click on a row, or `m` on the selection, opens a popup menu of `menu_actions` anchored at the click
  / under the selected row, clamped inside the pane; Up/Down/Enter or click to run, Esc or a click outside
  closes it (Esc never exits the view).
- `s` / `x` start / stop the selected row (container or whole group).
- The group row also shows the button (from `group_action`) in its last 3 cells, before `running/total`
  is drawn left of it.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn container_line_keeps_the_button_at_the_right_edge() {
    let c = &groups()[1].containers[0]; // s3pweb-redis-stack, running
    let line = Line::from(container_line(c, 32, false));
    assert_eq!(line.width(), 32);
    assert!(line.to_string().ends_with(" ■ "));
    let narrow = Line::from(container_line(c, 16, false));
    assert_eq!(narrow.width(), 16);
    assert!(narrow.to_string().ends_with(" ■ "));
    assert!(Line::from(container_line(c, 32, true)).to_string().ends_with(" … "));
}

#[test]
fn button_hit_is_the_last_three_cells() {
    assert!(button_hit(32, 29) && button_hit(32, 31));
    assert!(!button_hit(32, 28));
}

#[test]
fn menus_offer_what_applies() {
    let g = groups();
    assert_eq!(menu_actions(RowKind::Container(1, 0), &g), [Action::Stop, Action::Restart]);
    assert_eq!(menu_actions(RowKind::Container(0, 2), &g), [Action::Start]);
    assert_eq!(menu_actions(RowKind::Group(0), &g), [Action::Start, Action::Stop, Action::Restart]);
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
```

`click_for_test` returns the queued `(Action, Vec<String>)` instead of spawning (the test build swaps the
worker spawn for a recorder); `button_cell_for_test` finds the button cell from the last draw.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --bin herdr-sidebar docker_app::`
Expected: compile errors for the new functions.

- [ ] **Step 3: Implement the Interfaces and behavior above**

- [ ] **Step 4: Run the whole suite and clippy**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add src/docker_app.rs
git commit -m "feat(docker): start/stop from row buttons, the context menu, and s/x"
```

---

### Task 6: Run the fork in herdr and verify for real

**Files:**
- Modify: `Cargo.toml` and `herdr-plugin.toml` (`version = "0.15.1-docker.1"`), `Cargo.lock` (via build)
- Modify: `AGENTS.md` (repo root: a short "Docker view" section — the `docker_active`/`Exit::Docker`
  design, the shared activity bar, the refresh rule)

- [ ] **Step 1: Bump the version so the build script cannot fetch an upstream binary**

Set both versions to `0.15.1-docker.1`; run `cargo build --release`.
Expected: `target/release/herdr-sidebar` built; `Cargo.lock` updated.

- [ ] **Step 2: Back up the sidebar's state and config**

Run: `cp -R ~/.local/state/herdr/plugins/herdr-sidebar ~/.local/state/herdr/plugins/herdr-sidebar.bak-docker && cp -R ~/.config/herdr/plugins/config/herdr-sidebar ~/.config/herdr/plugins/config/herdr-sidebar.bak-docker`
Expected: both copies exist.

- [ ] **Step 3: Swap the GitHub install for the fork**

Run: `herdr plugin uninstall herdr-sidebar && herdr plugin link ~/code/herdr-sidebar/plugins/herdr-sidebar && herdr plugin list`
Expected: `herdr-sidebar` listed with a local/link source pointing at the fork. If uninstall removed the
state or config folders, restore them from the backups. Note in `AGENTS.md` whether `link` ran the build script.

- [ ] **Step 4: Redeploy and check the running binary is the fork's**

Run: `herdr plugin action invoke herdr-sidebar.redeploy`, then confirm the sidebar process's executable is
`~/code/herdr-sidebar/plugins/herdr-sidebar/target/release/herdr-sidebar`
(`ps -o args= -p $(pgrep -f 'herdr-sidebar/target/release/herdr-sidebar' | head -1)`).
Expected: the fork's path. `~/.local/state/herdr/plugins/herdr-sidebar/state.json` still has the user's
settings (`custom_editor_on_click: true`, `colors: terminal`), and `editor-command.txt` is unchanged.

- [ ] **Step 5: Manual check with the user** (they click; you read state and `docker ps`)

Cmd+B → Docker icon visible as the 4th; click it → containers grouped as
`s3pweb-border-engine 1/3`-style and `Individual containers`; ■ on `s3pweb-redis-stack` stops it
(`docker ps` confirms), ▶ starts it again; right-click shows Stop/Restart; `1` returns to Explorer; quit
Docker Desktop → "Docker isn't running" + button. Fix anything found with a test first.

- [ ] **Step 6: Commit and push the branch to the fork**

```bash
git add Cargo.toml Cargo.lock herdr-plugin.toml ../../AGENTS.md
git commit -m "chore: run the fork as 0.15.1-docker.1; document the Docker view"
git push -u origin feature/docker-view
```
