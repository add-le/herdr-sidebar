# Docker view for the herdr sidebar — design

Date: 2026-10-21. Fork: `add-le/herdr-sidebar` (from `alexarthurs/herdr-sidebar` v0.15.1, commit `081ed13`).

## Intent

The user moved from VS Code to herdr. In VS Code they used the Docker extension's Containers panel for one
job: glance at which containers are running and stop one they forgot (or start one they need). Forgotten
containers also cost battery, which the user cares about (they recently lost 60% of a charge in under two
hours). They want that panel inside the existing Cmd+B sidebar, not as a separate column, and accepted
running a personal fork of the sidebar (updated from upstream by hand) to get it.

Success: press Cmd+B, click the Docker icon, see every container with its state at a glance, start or stop
one with one click.

## Goals

- A fourth view, **Docker**, in the Unified sidebar's activity bar, after Explorer, Search and Source
  Control. Reached by clicking its icon, `4`, or `Ctrl+4`, the same way as the other three.
- Lists **all containers, running and stopped**, grouped by Compose project
  (`com.docker.compose.project` label). Containers without that label go under **Individual containers**.
  Projects are collapsible. A project row shows `running/total` (e.g. `0/3`).
- A container row shows: state icon (running: green `●`; stopped/exited/created: red `■`), container name,
  a short image name (e.g. `mongo:7.0.43`, `kafka:3.7.0` from `apache/kafka:3.7.0`), and an action button at
  the right edge of the row: `▶` when stopped, `■` when running.
- **Start/stop**, two ways (the user asked for both):
  - One click on the row's button starts or stops that container. The button on a project row starts or
    stops every container in the project.
  - Right-click (Ctrl+right-click if that is the sidebar's convention) or `m` on a row opens a menu with
    Start, Stop and Restart, whichever apply. Built with the same menu builder the other views use.
- **No confirmation** before stopping: starting it again undoes it.
- While a start/stop/restart runs, the row shows a busy state and ignores further clicks on its button until
  the command finishes; then the list refreshes. A failed command shows its error briefly (toast or status
  line, whichever the sidebar already uses for errors).
- **Docker not running** (CLI missing, or daemon unreachable): the view says so in one line and offers a
  **Start Docker Desktop** button (`open -a Docker`); the list appears once the daemon answers.
- A header refresh button `⟳` forces an immediate refresh.
- Colors come from the sidebar's own `palette()`, so the view follows whichever theme is active. The user's
  `state.json` currently has `"colors":"terminal"` (herdr runs Dracula) and `"sidebar_width":32`: check the
  view looks right in that theme at about 32 columns.

## Non-goals (first version)

Images, volumes, networks, registries, Docker contexts, logs, shells, pull/push, inspecting a container,
creating containers, Compose `up` from files on disk. They may come later; the menu is where they would go.

## Anti-goals

- **No background battery drain.** Docker is queried only while the Docker view is on screen **and** the
  sidebar pane is focused (the existing `pane_is_focused` backoff). Nothing Docker-related runs while
  Explorer, Search or Source Control is shown. No `docker` polling every couple of seconds forever: either a
  slow throttle while visible (builder's call, target ≤ one `docker ps` per ~3 s while focused) or a
  `docker events` stream that lives only while the view is visible and focused.
- **Never block the event loop.** All `docker` commands run off the UI thread (mirror the Explorer's
  `request_decorations`/`collect_decorations` thread + `mpsc` + `tick()` `try_recv` pattern,
  `src/explorer_app.rs:626-700`). The heartbeat must keep stamping (AGENTS.md: ≤ 20 s or the pane is
  replaced). Do not copy SCM's synchronous `refresh()` from `tick()`.
- **Don't break the existing views** or their settings, state file, or behavior. Existing tests keep passing.
- **Don't leak secrets.** First version shows no env vars or inspect output.

## Constraints

- Code: Rust, ratatui 0.30 + crossterm 0.29, plugin crate in `plugins/herdr-sidebar`. Follow AGENTS.md
  (repo root) conventions, including: activity-bar order = number keys = VS Code order; nothing highlighted
  until the user selects; mouse wheel scrolls the view only; scrollbar on overflow; `m` and the context-menu
  click share one builder; Esc never exits, only `q`; settings writes only via `state::update_state`; UI
  says "Unified sidebar", never "merge". Record new findings in AGENTS.md as it asks.
- The view enum is currently two-way (`View { Explorer, SourceControl }`, `src/state.rs:103`) and
  `Exit::Switch` uses `view.other()`; the activity bar is drawn by both apps (`explorer_app.rs:3506`,
  `scm_app.rs:4151`) from a fixed 3-tuple (`ui::activity_icons`, `src/ui.rs:447`); host F9–F11 inject the
  `show-*` actions and F12 is taken by Quick Open. Adding a third app/view means generalizing these, not
  special-casing.
- The Docker view only needs to work in the **Unified sidebar** (merged) mode the user runs. Separate-pane
  mode (`--view`, extra `[[panes]]`, identity token) is not required; if the builder skips it, the Docker
  icon must simply not appear there rather than break it.
- macOS only is required. Windows twins of any new herdr actions (AGENTS.md rule) are the builder's call;
  don't break the Windows build.
- The `docker` binary on this Mac is `~/.docker/bin/docker` (Docker Desktop); resolve it via `PATH`, then
  that fallback. Panes spawned by herdr may not have the login shell's `PATH`.
- Commands: `docker ps -a --format '{{json .}}'` (or equivalent) for the list; `docker start|stop|restart
  <id>`; for a project button, act on the project's containers by ID (Compose CLI not required).
- Tests: `cargo test` and `cargo clippy -- -D warnings` from `plugins/herdr-sidebar` must pass. Unit-test
  the pure parts (parsing `docker ps` output, grouping/sorting, `running/total`, short image names, row
  hit-testing for the button) in inline `#[cfg(test)]` modules like the rest of the crate.

## Install and run (part of the work)

- The Mac has no Rust toolchain: install one (rustup, stable).
- Today the sidebar is a GitHub-managed install (`herdr plugin list`: `source.kind=github`), whose updates
  replace the folder. Switch to the fork: uninstall the GitHub copy, then `herdr plugin link
  ~/code/herdr-sidebar/plugins/herdr-sidebar`. Same plugin id, so the existing state
  (`~/.local/state/herdr/plugins/herdr-sidebar/`: `state.json`, `editor-command.txt`) and the Cmd+B
  binding in `~/.config/herdr/config.toml` keep working; verify that.
- **Watch out:** `scripts/fetch-or-build.sh` downloads the upstream prebuilt binary for the version in
  `Cargo.toml` and only falls back to `cargo build`. The fork must end up running its own build (e.g. a
  distinct version suffix so no upstream asset matches, or forcing a source build); confirm the running
  binary has the Docker view.
- Before uninstalling, back up `~/.local/state/herdr/plugins/herdr-sidebar/` and
  `~/.config/herdr/plugins/config/herdr-sidebar/`; check whether uninstall removes them and restore if so.
  Check whether `herdr plugin link` runs the `[[build]]` script or only registers the path.
- Saving `"active":"docker"` in `state.json` is safe: the state parser falls back to Explorer on an unknown
  value, so an upstream build reading the same file won't break.
- No `show-docker` host action is needed (the user binds only Cmd+B; F9–F12 are all taken).
- After rebuilding, redeploy with the `herdr-sidebar.redeploy` action.
- Commit on a branch in the fork and push to `origin` (`add-le/herdr-sidebar`). Nothing goes to upstream.

## Left to the builder

Exact icon glyphs (Nerd Font is available: MesloLGS NF), refresh mechanism within the battery rule above,
sort order within a project (suggest: running first, then name), how the busy state looks, error display,
whether the project row's button is shown when the project is mixed (suggest: `■` if any are running),
keyboard equivalents for the row button (e.g. `s`/`x` like the selected-row actions elsewhere), and how to
generalize the view plumbing.
