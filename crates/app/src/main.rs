#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod autostart;
mod config;
mod instance;
mod jetbrains;
mod scanner;
mod scrollbar;
mod settings;
mod text_input;
mod tray;
mod update;

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use git_track_core::{Event, PROTOCOL_VERSION, RepoStatus, Severity, WatchCommand};
use gpui::{
    AnyElement, AnyView, App, Application, Bounds, ClipboardItem, Context, Corner, Div, ElementId,
    Entity, FocusHandle, FontWeight, Hsla, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent,
    SharedString, Stateful, Subscription, Task, TitlebarOptions, Window, WindowBounds,
    WindowOptions, actions, anchored, deferred, div, point, prelude::*, px, rgb, size,
};

use config::Config;
use scanner::{Msg, SourceKind};

actions!(git_track, [Quit, OpenSettings]);

mod colors {
    pub const BG: u32 = 0x1e1f22;
    pub const HEADER: u32 = 0x2b2d30;
    pub const PANEL: u32 = 0x27292d;
    pub const BORDER: u32 = 0x3c3f45;
    pub const BUTTON: u32 = 0x34373c;
    pub const BUTTON_HOVER: u32 = 0x43474e;
    pub const TEXT: u32 = 0xdfe1e5;
    pub const DIM: u32 = 0x8c9099;
    pub const DANGER: u32 = 0xf26b7a;
    pub const DANGER_BG: u32 = 0x4a252b;
    pub const WARNING: u32 = 0xe5c07b;
    pub const WARNING_BG: u32 = 0x463c24;
    pub const OK: u32 = 0x73bd79;
    pub const ACCENT: u32 = 0x6b9cf7;
}
use colors::*;

const SOURCES: [SourceKind; 2] = [SourceKind::Windows, SourceKind::Wsl];

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Default)]
enum Phase {
    #[default]
    Idle,
    Scanning {
        checked: usize,
        total: Option<usize>,
    },
}

struct Source {
    kind: SourceKind,
    phase: Phase,
    repos: BTreeMap<String, RepoStatus>,
    /// Paths reported by the running full scan; used to drop vanished repositories.
    seen: HashSet<String>,
    error: Option<String>,
    root_errors: Vec<String>,
    warning: Option<String>,
    distro: Option<String>,
    /// When the last full scan finished, and how long it took.
    last_scan: Option<(Instant, u64)>,
    task: Option<Task<()>>,
    /// Keeps the listed repositories up to date between full scans.
    watcher: Option<Watcher>,
    /// When the watcher last stopped unexpectedly.
    watch_ended: Option<Instant>,
}

struct Watcher {
    id: usize,
    commands: std::sync::mpsc::Sender<WatchCommand>,
    _task: Task<()>,
}

/// A watcher that stopped is restarted no sooner than this (WSL shut down, …).
const WATCH_RETRY: Duration = Duration::from_secs(60);

impl Source {
    fn new(kind: SourceKind) -> Self {
        Self {
            kind,
            phase: Phase::Idle,
            repos: BTreeMap::new(),
            seen: HashSet::new(),
            error: None,
            root_errors: Vec::new(),
            warning: None,
            distro: None,
            last_scan: None,
            task: None,
            watcher: None,
            watch_ended: None,
        }
    }

    fn title(&self) -> String {
        match (self.kind, &self.distro) {
            (SourceKind::Windows, _) => "Windows".into(),
            (SourceKind::Wsl, Some(d)) => format!("WSL · {d}"),
            (SourceKind::Wsl, None) => "WSL".into(),
        }
    }

    fn scanning(&self) -> bool {
        matches!(self.phase, Phase::Scanning { .. })
    }

    fn dirty_count(&self) -> usize {
        self.repos.values().filter(|r| !r.is_clean()).count()
    }

    fn begin_scan(&mut self) {
        self.phase = Phase::Scanning {
            checked: 0,
            total: None,
        };
        self.seen.clear();
        self.error = None;
        self.root_errors.clear();
    }

    fn apply_hello(&mut self, protocol: u32, version: &str, distro: Option<String>) {
        if distro.is_some() {
            self.distro = distro;
        }
        self.warning = (protocol != PROTOCOL_VERSION).then(|| {
            format!(
                "Agent {version} speaks protocol {protocol}, expected {PROTOCOL_VERSION}. Rebuild the agent configured as [wsl] agent, or remove that setting to use the bundled one."
            )
        });
    }

    fn apply_scan(&mut self, msg: Msg) {
        match msg {
            Msg::Failed(message) => {
                self.error = Some(message);
                self.phase = Phase::Idle;
            }
            Msg::Event(event) => match event {
                Event::Hello {
                    version,
                    protocol,
                    distro,
                } => self.apply_hello(protocol, &version, distro),
                Event::Discovered { count } => {
                    if let Phase::Scanning { total, .. } = &mut self.phase {
                        *total = Some(count);
                    }
                }
                Event::Repo(repo) => {
                    if let Phase::Scanning { checked, .. } = &mut self.phase {
                        *checked += 1;
                    }
                    self.seen.insert(repo.path.clone());
                    self.repos.insert(repo.path.clone(), repo);
                }
                // Only sent for explicitly requested repositories (re-checks).
                Event::Gone { path } => {
                    self.repos.remove(&path);
                }
                Event::RootError { root, message } => {
                    self.root_errors.push(format!("{root}: {message}"))
                }
                Event::Fatal { message } => {
                    self.error = Some(message);
                    self.phase = Phase::Idle;
                }
                Event::Done { elapsed_ms } => {
                    if self.error.is_none() {
                        let seen = std::mem::take(&mut self.seen);
                        self.repos.retain(|path, _| seen.contains(path));
                    }
                    self.phase = Phase::Idle;
                    self.last_scan = Some((Instant::now(), elapsed_ms));
                }
            },
        }
    }

    /// Called when the scan's channel closes.
    fn finish_scan(&mut self) {
        if self.scanning() {
            self.error
                .get_or_insert_with(|| "scan ended unexpectedly".into());
            self.phase = Phase::Idle;
        }
    }
}

struct Summary {
    total: usize,
    dirty: usize,
    scanning: bool,
}

impl Summary {
    fn text(&self) -> (String, u32) {
        let dirty = self.dirty;
        if dirty > 0 {
            let noun = if dirty == 1 {
                "repository"
            } else {
                "repositories"
            };
            (format!("{dirty} {noun} with unpushed work"), DANGER)
        } else if self.scanning && self.total == 0 {
            ("scanning…".to_string(), DIM)
        } else {
            ("everything is pushed".to_string(), OK)
        }
    }
}

/// State of the update check.
#[derive(Default)]
enum NewVersion {
    #[default]
    Unknown,
    Checking,
    Available(update::Release),
    Installing(update::Release),
}

/// How often updates are looked for (the first time shortly after the start).
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(12 * 60 * 60);
const UPDATE_CHECK_FIRST: Duration = Duration::from_secs(30);

/// Runs blocking work (network, file system) on a thread of its own.
fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> futures::channel::oneshot::Receiver<T> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx
}

/// The dropdown that is currently open (only one at a time).
#[derive(Clone, PartialEq, Eq)]
enum OpenMenu {
    Main,
    /// IDE chooser of a repository.
    Ide(SourceKind, String),
    /// "Ignore" choices of a repository.
    Ignore(SourceKind, String),
}

struct GitTrack {
    config: Config,
    config_error: Option<String>,
    sources: Vec<Source>,
    show_clean: bool,
    /// Repositories currently being re-checked individually.
    busy: HashSet<(SourceKind, String)>,
    notice: Option<String>,
    last_full_refresh: Instant,
    scrollbar: scrollbar::Scrollbar,
    menu: Option<OpenMenu>,
    jetbrains: jetbrains::JetBrains,
    /// Ids for watchers, to tell a stale one's end from the current one's.
    watchers_started: usize,
    /// When the window last lost the focus.
    deactivated_at: Option<Instant>,
    /// Decided when the tray icon is pressed: the click hides the window.
    tray_click_hides: bool,
    /// Opened hidden (autostart) and not shown since.
    never_shown: bool,
    focus: FocusHandle,
    /// Present while closing the window hides it to the tray.
    tray: Option<tray::Tray>,
    /// The open settings dialog.
    settings: Option<(Entity<settings::Settings>, Subscription)>,
    new_version: NewVersion,
    /// The available version the user postponed ("Later").
    postponed_version: Option<String>,
    _commands_task: Task<()>,
    _ticker: Task<()>,
    _update_ticker: Task<()>,
    _activation: Subscription,
}

impl GitTrack {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        instance_commands: UnboundedReceiver<tray::Command>,
        hidden: bool,
        updated: bool,
    ) -> Self {
        let (config, config_error) = match Config::load() {
            Ok(config) => (config, None),
            Err(e) => (Config::default(), Some(e)),
        };

        let mut notice = updated.then(|| format!("Updated to git-track {}.", update::CURRENT));
        let (tray, tray_commands) = if config.close_to_tray {
            match tray::Tray::new() {
                Ok((tray, commands)) => (Some(tray), Some(commands)),
                Err(e) => {
                    notice = Some(format!("No tray icon, closing the window quits: {e}"));
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        // Commands from the tray icon and from instances started later.
        let mut commands = futures::stream::select(
            tray_commands.unwrap_or_else(|| futures::channel::mpsc::unbounded().1),
            instance_commands,
        );
        let commands_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(command) = commands.next().await {
                let alive = this.update_in(cx, |this, window, cx| match command {
                    tray::Command::Show => this.show_window(window, cx),
                    tray::Command::Press => {
                        // Pressing the tray icon already takes the focus away from
                        // the window, so "was in front" means: active until just now.
                        let recently_active = window.is_window_active()
                            || this
                                .deactivated_at
                                .is_some_and(|t| t.elapsed() < Duration::from_millis(500));
                        this.tray_click_hides = recently_active && tray::is_shown(window);
                    }
                    tray::Command::Toggle => {
                        if std::mem::take(&mut this.tray_click_hides) {
                            this.hide_to_tray(window, cx);
                        } else {
                            this.show_window(window, cx);
                        }
                    }
                    tray::Command::Quit => this.quit(cx),
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        // With a tray icon, closing only hides the window.
        let weak = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let hidden = weak
                .update(cx, |this, cx| this.hide_to_tray(window, cx))
                .unwrap_or(false);
            !hidden
        });

        let focus = cx.focus_handle();
        window.focus(&focus);

        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.on_activated(cx);
            } else {
                this.deactivated_at = Some(Instant::now());
            }
        });

        // Periodic tick: refreshes the relative timestamps and triggers the auto refresh.
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_secs(15))
                    .await;
                let alive = this.update(cx, |this, cx| {
                    let interval = this.config.refresh_interval_secs;
                    if interval > 0
                        && this.last_full_refresh.elapsed() >= Duration::from_secs(interval)
                    {
                        this.refresh_all(cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });

        // Debug builds are being worked on; they only check when asked to.
        let update_ticker = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UPDATE_CHECK_FIRST).await;
            loop {
                let alive = this.update(cx, |this, cx| {
                    if this.config.check_updates && !cfg!(debug_assertions) {
                        this.check_update(false, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(UPDATE_CHECK_EVERY).await;
            }
        });

        let mut this = Self {
            config,
            config_error,
            sources: SOURCES.into_iter().map(Source::new).collect(),
            show_clean: false,
            busy: HashSet::new(),
            notice,
            last_full_refresh: Instant::now(),
            scrollbar: scrollbar::Scrollbar::default(),
            menu: None,
            jetbrains: jetbrains::JetBrains::load(),
            watchers_started: 0,
            deactivated_at: None,
            tray_click_hides: false,
            never_shown: hidden,
            focus,
            tray,
            settings: None,
            new_version: NewVersion::Unknown,
            postponed_version: None,
            _commands_task: commands_task,
            _ticker: ticker,
            _update_ticker: update_ticker,
            _activation: activation,
        };
        if hidden && this.tray.is_none() {
            // Nothing to bring the window back with: show it after all.
            cx.spawn_in(window, async move |this, cx| {
                let _ = this.update_in(cx, |this, window, cx| this.show_window(window, cx));
            })
            .detach();
        }
        this.start_all(cx);
        this
    }

    /// Shows the window (from the tray or a hidden start) and brings it to the front.
    fn show_window(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.never_shown) {
            tray::show_window(window);
            return;
        }
        // A window opened hidden gets its size and position from gpui on
        // the first activation (done in a task); show and focus it after that.
        window.activate_window();
        cx.spawn_in(window, async move |_, cx| {
            let _ = cx.update(|window, _| tray::show_window(window));
        })
        .detach();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            return;
        }
        self.menu = None;
        // Start from the file, so edits made there since the last refresh are kept.
        let (draft, load_error) = match Config::load() {
            Ok(config) => (config, None),
            Err(e) => (self.config.clone(), Some(e)),
        };
        let view = cx.new(|cx| settings::Settings::new(draft, load_error, cx));
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            match event {
                settings::SettingsEvent::Saved => this.apply_settings(cx),
                settings::SettingsEvent::Closed => this.close_settings(window, cx),
            }
        });
        self.settings = Some((view, subscription));
        cx.notify();
    }

    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings = None;
        // The focused input is gone; keep keyboard shortcuts working.
        window.focus(&self.focus);
        cx.notify();
    }

    /// Takes over the config the settings dialog wrote.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let old = self.config.clone();
        match Config::load() {
            Ok(config) => {
                self.config = config;
                self.config_error = None;
            }
            Err(e) => {
                self.config_error = Some(e);
                return;
            }
        }
        if old.close_to_tray != self.config.close_to_tray {
            self.notice = Some("The tray setting takes effect after restarting git-track.".into());
        }
        self.apply_ignores();
        let new = &self.config;
        let rescan = old.track != new.track
            || old.jetbrains != new.jetbrains
            || old.windows != new.windows
            || old.wsl != new.wsl;
        if rescan {
            // Restarted with the new settings (distribution, …) after the scan.
            for source in &mut self.sources {
                source.watcher = None;
            }
            self.start_all(cx);
        }
        self.sync_tray();
        cx.notify();
    }

    /// Adds `path` to the ignored paths of `kind` and saves the config.
    fn ignore_path(&mut self, kind: SourceKind, path: String, cx: &mut Context<Self>) {
        self.menu = None;
        let mut config = match Config::load() {
            Ok(config) => config,
            Err(e) => {
                self.notice = Some(format!("Cannot ignore {path}: {e}"));
                cx.notify();
                return;
            }
        };
        if !config.is_ignored(kind, &path) {
            config.ignored_mut(kind).push(path);
        }
        match config.save() {
            Ok(()) => {
                self.config = config;
                self.config_error = None;
                self.apply_ignores();
                self.sync_watcher(kind, cx);
                self.sync_tray();
            }
            Err(e) => self.notice = Some(e),
        }
        cx.notify();
    }

    /// Drops listed repositories that are ignored now.
    fn apply_ignores(&mut self) {
        for kind in SOURCES {
            let ignored: Vec<String> = self
                .source(kind)
                .repos
                .keys()
                .filter(|path| self.config.is_ignored(kind, path))
                .cloned()
                .collect();
            for path in ignored {
                self.remove_repo(kind, &path);
            }
        }
    }

    /// Looks for a newer release in the background.
    fn check_update(&mut self, manual: bool, cx: &mut Context<Self>) {
        match &self.new_version {
            NewVersion::Checking | NewVersion::Installing(_) => return,
            // Asked again: show the banner again.
            NewVersion::Available(_) if manual => {
                self.postponed_version = None;
                cx.notify();
                return;
            }
            _ => {}
        }
        self.new_version = NewVersion::Checking;
        let result = blocking(update::check);
        cx.spawn(async move |this, cx| {
            let result = result
                .await
                .unwrap_or_else(|_| Err("the update check stopped".into()));
            this.update(cx, |this, cx| {
                this.new_version = NewVersion::Unknown;
                match result {
                    Ok(Some(release)) => this.new_version = NewVersion::Available(release),
                    Ok(None) if manual => {
                        this.notice = Some(format!(
                            "git-track {} is the latest version.",
                            update::CURRENT
                        ))
                    }
                    Err(e) if manual => this.notice = Some(e),
                    // Automatic checks stay quiet (offline, rate limit, …).
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Downloads the available release, replaces this executable and restarts.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let NewVersion::Available(release) = std::mem::take(&mut self.new_version) else {
            return;
        };
        self.new_version = NewVersion::Installing(release.clone());
        let result = blocking({
            let release = release.clone();
            move || update::install(&release)
        });
        cx.spawn(async move |this, cx| {
            let result = result
                .await
                .unwrap_or_else(|_| Err("the installation stopped".into()));
            this.update(cx, |this, cx| {
                match result.map(|()| update::restart()) {
                    Ok(Ok(())) => return this.quit(cx),
                    Ok(Err(e)) => {
                        this.new_version = NewVersion::Unknown;
                        this.notice = Some(format!(
                            "git-track {} is installed, but it could not be started ({e}). Restart git-track.",
                            release.version
                        ));
                    }
                    Err(e) => {
                        this.new_version = NewVersion::Available(release);
                        this.notice = Some(format!("The update failed: {e}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<Div> {
        match &self.new_version {
            NewVersion::Available(release)
                if self.postponed_version.as_ref() != Some(&release.version) =>
            {
                let notes = release.notes_url.clone();
                let version = release.version.clone();
                Some(
                    banner(
                        format!(
                            "git-track {} is available (you have {}).",
                            release.version,
                            update::CURRENT
                        ),
                        ACCENT,
                        HEADER,
                    )
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().flex_1())
                    .child(
                        button("update-notes", "Release notes")
                            .on_click(move |_, _, cx| cx.open_url(&notes)),
                    )
                    .child(button("update-later", "Later").on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.postponed_version = Some(version.clone());
                            cx.notify();
                        },
                    )))
                    .child(
                        button("update-install", "Install and restart")
                            .on_click(cx.listener(|this, _, _, cx| this.install_update(cx))),
                    ),
                )
            }
            NewVersion::Installing(release) => Some(banner(
                format!("Downloading git-track {}…", release.version),
                ACCENT,
                HEADER,
            )),
            _ => None,
        }
    }

    fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        let enable = autostart::state() != autostart::State::On;
        if let Err(e) = autostart::set(enable) {
            self.notice = Some(e);
        }
        cx.notify();
    }

    /// Hides the window if there is a tray icon to bring it back. Returns
    /// whether it was hidden.
    fn hide_to_tray(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        self.menu = None;
        cx.notify();
        self.tray.is_some() && tray::hide_window(window)
    }

    /// Quits the application (instead of hiding to the tray).
    fn quit(&mut self, cx: &mut Context<Self>) {
        // Remove the tray icon explicitly; it would linger until hovered otherwise.
        self.tray = None;
        cx.quit();
    }

    fn summary(&self) -> Summary {
        let enabled = || self.sources.iter().filter(|s| self.config.enabled(s.kind));
        Summary {
            total: enabled().map(|s| s.repos.len()).sum(),
            dirty: enabled().map(|s| s.dirty_count()).sum(),
            scanning: enabled().any(|s| s.scanning()),
        }
    }

    /// Mirrors the summary in the tray tooltip and icon.
    fn sync_tray(&mut self) {
        let summary = self.summary();
        if let Some(tray) = &mut self.tray {
            let tooltip = format!("git-track: {}", summary.text().0);
            tray.update(&tooltip, summary.dirty > 0);
        }
    }

    fn source(&self, kind: SourceKind) -> &Source {
        self.sources
            .iter()
            .find(|s| s.kind == kind)
            .expect("all sources exist")
    }

    fn source_mut(&mut self, kind: SourceKind) -> &mut Source {
        self.sources
            .iter_mut()
            .find(|s| s.kind == kind)
            .expect("all sources exist")
    }

    /// Reloads the config and rescans everything.
    fn refresh_all(&mut self, cx: &mut Context<Self>) {
        match Config::load() {
            Ok(config) => {
                self.config = config;
                self.config_error = None;
            }
            Err(e) => self.config_error = Some(e),
        }
        self.jetbrains = jetbrains::JetBrains::load();
        self.start_all(cx);
    }

    fn start_all(&mut self, cx: &mut Context<Self>) {
        self.last_full_refresh = Instant::now();
        for kind in SOURCES {
            if self.config.enabled(kind) {
                self.start_scan(kind, cx);
            } else {
                let source = self.source_mut(kind);
                source.task = None;
                source.watcher = None;
                source.phase = Phase::Idle;
                source.repos.clear();
            }
        }
        self.sync_tray();
        cx.notify();
    }

    fn start_scan(&mut self, kind: SourceKind, cx: &mut Context<Self>) {
        if self.source(kind).scanning() {
            return;
        }
        let mut rx = scanner::start(kind, self.config.request(kind, Vec::new()), &self.config);
        let task = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    let done = matches!(msg, Msg::Event(Event::Done { .. }));
                    this.source_mut(kind).apply_scan(msg);
                    if done {
                        this.sync_watcher(kind, cx);
                    }
                    this.sync_tray();
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.source_mut(kind).finish_scan();
                this.sync_tray();
                cx.notify();
            })
            .ok();
        });
        let source = self.source_mut(kind);
        source.begin_scan();
        source.task = Some(task);
        cx.notify();
    }

    /// Re-checks a single repository without rediscovering anything.
    fn recheck(&mut self, kind: SourceKind, path: String, cx: &mut Context<Self>) {
        self.recheck_many(kind, vec![path], cx);
    }

    /// Re-checks repositories of one source in a single scan (one agent
    /// call for WSL). Repositories already being re-checked are skipped.
    fn recheck_many(&mut self, kind: SourceKind, paths: Vec<String>, cx: &mut Context<Self>) {
        let paths: Vec<String> = paths
            .into_iter()
            .filter(|path| self.busy.insert((kind, path.clone())))
            .collect();
        if paths.is_empty() {
            return;
        }
        let keys: Vec<_> = paths.iter().map(|p| (kind, p.clone())).collect();
        let mut rx = scanner::start(kind, self.config.request(kind, paths), &self.config);
        cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    this.apply_recheck(kind, msg);
                    this.sync_tray();
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                for key in &keys {
                    this.busy.remove(key);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Drops a deleted repository from the list.
    fn remove_repo(&mut self, kind: SourceKind, path: &str) {
        self.source_mut(kind).repos.remove(path);
    }

    /// Starts the watcher of `kind` unless it runs already (or may not run
    /// yet). Returns whether it runs.
    fn ensure_watcher(&mut self, kind: SourceKind, cx: &mut Context<Self>) -> bool {
        if !self.config.enabled(kind) {
            return false;
        }
        let source = self.source(kind);
        if source.watcher.is_some() {
            return true;
        }
        // Not restarted in a loop while it keeps failing (e.g. WSL shut down).
        if source.watch_ended.is_some_and(|t| t.elapsed() < WATCH_RETRY) {
            return false;
        }
        self.watchers_started += 1;
        let id = self.watchers_started;
        let (commands, mut rx) = scanner::start_watch(kind, &self.config);
        let task = cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    match msg {
                        Msg::Failed(message) => {
                            // A failing scan already explains it.
                            if this.source(kind).error.is_none() {
                                this.notice = Some(format!(
                                    "Watching the {kind:?} repositories stopped: {message}"
                                ));
                            }
                        }
                        msg => this.apply_recheck(kind, msg),
                    }
                    this.sync_tray();
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                let source = this.source_mut(kind);
                if source.watcher.as_ref().is_some_and(|w| w.id == id) {
                    source.watcher = None;
                    source.watch_ended = Some(Instant::now());
                    cx.notify();
                }
            })
            .ok();
        });
        self.source_mut(kind).watcher = Some(Watcher {
            id,
            commands,
            _task: task,
        });
        true
    }

    /// Hands the listed repositories of `kind` to its watcher.
    fn sync_watcher(&mut self, kind: SourceKind, cx: &mut Context<Self>) {
        if !self.ensure_watcher(kind, cx) {
            return;
        }
        let repos = self
            .source(kind)
            .repos
            .values()
            .filter(|r| !self.config.is_ignored(kind, &r.path))
            .cloned()
            .collect();
        let options = self.config.request(kind, Vec::new()).options;
        self.send_watch(kind, WatchCommand::Set { options, repos });
    }

    fn send_watch(&mut self, kind: SourceKind, command: WatchCommand) {
        let source = self.source_mut(kind);
        if let Some(watcher) = &source.watcher
            && watcher.commands.send(command).is_err()
        {
            source.watcher = None;
            source.watch_ended = Some(Instant::now());
        }
    }

    /// The window became active: catch up with what the user did meanwhile.
    fn on_activated(&mut self, cx: &mut Context<Self>) {
        for kind in SOURCES {
            let source = self.source(kind);
            if source.watcher.is_none() {
                // While scanning (or before the first scan) the scan starts it.
                if source.scanning() || source.last_scan.is_none() {
                    continue;
                }
                self.sync_watcher(kind, cx);
            }
            self.send_watch(kind, WatchCommand::Activated);
        }
    }

    /// Expect changes in `path` soon (an action or IDE was launched for it).
    fn touch(&mut self, kind: SourceKind, path: String) {
        self.send_watch(kind, WatchCommand::Touch { paths: vec![path] });
    }

    fn apply_recheck(&mut self, kind: SourceKind, msg: Msg) {
        match msg {
            Msg::Event(Event::Repo(mut repo)) => {
                // Ignored just now; the watcher may not know yet.
                if self.config.is_ignored(kind, &repo.path) {
                    return;
                }
                let source = self.source_mut(kind);
                if repo.root.is_none() {
                    repo.root = source
                        .repos
                        .get(&repo.path)
                        .and_then(|old| old.root.clone());
                }
                source.repos.insert(repo.path.clone(), repo);
            }
            Msg::Event(Event::Gone { path }) => self.remove_repo(kind, &path),
            Msg::Event(Event::Hello {
                version,
                protocol,
                distro,
            }) => self
                .source_mut(kind)
                .apply_hello(protocol, &version, distro),
            Msg::Event(Event::Fatal { message }) | Msg::Failed(message) => {
                self.notice = Some(message)
            }
            Msg::Event(_) => {}
        }
    }

    fn run_action(&mut self, kind: SourceKind, index: usize, path: String, cx: &mut Context<Self>) {
        let Some(action) = self.config.actions_for(kind).nth(index).cloned() else {
            return;
        };
        let distro = self.distro(kind);
        let vars = actions::Vars {
            kind,
            path: &path,
            distro: distro.as_deref(),
        };
        match actions::launch(&action, &vars) {
            Ok(()) => self.touch(kind, path),
            Err(e) => self.notice = Some(e),
        }
        cx.notify();
    }

    /// Opens a JetBrains project; `repo` is re-checked on focus like after an action.
    fn open_ide(
        &mut self,
        code: &str,
        kind: SourceKind,
        project: &str,
        repo: String,
        cx: &mut Context<Self>,
    ) {
        let Some(ide) = self.jetbrains.ides().iter().find(|i| i.code == code) else {
            return;
        };
        match jetbrains::launch(ide, kind, project, self.distro(kind).as_deref()) {
            Ok(()) => self.touch(kind, repo),
            Err(e) => self.notice = Some(e),
        }
        cx.notify();
    }

    /// WSL distribution name as reported by the agent (or configured).
    fn distro(&self, kind: SourceKind) -> Option<String> {
        self.source(kind)
            .distro
            .clone()
            .or_else(|| self.config.wsl.distro.clone())
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct Tooltip(SharedString);

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(520.))
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(HEADER))
            .text_color(rgb(TEXT))
            .text_xs()
            .child(self.0.clone())
    }
}

fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_, cx| cx.new(|_| Tooltip(text.clone())).into()
}

fn button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .px_2()
        .py(px(3.))
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(BUTTON))
        .hover(|s| s.bg(rgb(BUTTON_HOVER)))
        .cursor_pointer()
        .whitespace_nowrap()
        .text_xs()
        .child(label.into())
}

fn chip(id: impl Into<ElementId>, label: String, fg: u32, bg: u32) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .rounded_md()
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .text_xs()
        .whitespace_nowrap()
        .child(label)
}

fn banner(text: String, color: u32, bg: u32) -> Div {
    div()
        .mx_4()
        .mt_3()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(bg))
        .text_color(rgb(color))
        .child(text)
}

fn ago(at: Instant) -> String {
    match at.elapsed().as_secs() {
        0..60 => "just now".into(),
        s @ 60..3600 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

fn id(parts: impl std::fmt::Display) -> ElementId {
    ElementId::Name(SharedString::from(parts.to_string()))
}

fn dim(color: u32) -> Hsla {
    let mut c: Hsla = rgb(color).into();
    c.a = 0.75;
    c
}

fn menu_row(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .px_3()
        .py_1()
        .rounded_sm()
        .whitespace_nowrap()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(BUTTON_HOVER)))
}

fn menu_item(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    shortcut: Option<&'static str>,
) -> Stateful<Div> {
    menu_row(id)
        .gap_6()
        .child(div().flex_1().child(label.into()))
        .children(shortcut.map(|s| div().text_color(rgb(DIM)).child(s)))
}

/// A menu entry with a check mark in front.
fn menu_check(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
) -> Stateful<Div> {
    menu_row(id)
        .child(
            div()
                .w_5()
                .flex_none()
                .child(if checked { "✓" } else { "" }),
        )
        .child(label.into())
}

fn menu_label(text: impl Into<SharedString>) -> Div {
    div()
        .px_3()
        .py_1()
        .whitespace_nowrap()
        .text_color(rgb(DIM))
        .child(text.into())
}

fn menu_separator() -> Div {
    div().my_1().h(px(1.)).bg(rgb(BORDER))
}

/// A dropdown below the bottom-right corner of its parent (which must be
/// `relative`), plus a transparent full-window backdrop that calls `close`
/// when clicked anywhere else.
fn dropdown(
    window: &Window,
    close: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    items: Div,
) -> [AnyElement; 2] {
    let viewport = window.viewport_size();
    [
        deferred(
            anchored().position(point(px(0.), px(0.))).child(
                div()
                    .id("dropdown-backdrop")
                    .w(viewport.width)
                    .h(viewport.height)
                    .occlude()
                    .on_any_mouse_down(close),
            ),
        )
        .with_priority(1)
        .into_any_element(),
        // `anchored` takes its own layout origin as the anchor point, so pin a
        // zero-size box to the parent's bottom-right corner and anchor there.
        div()
            .absolute()
            .right(px(0.))
            .bottom(px(0.))
            .w(px(0.))
            .h(px(0.))
            .child(
                deferred(
                    anchored()
                        .anchor(Corner::TopRight)
                        .offset(point(px(0.), px(4.)))
                        .child(
                            items
                                .occlude()
                                .flex()
                                .flex_col()
                                .p_1()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(BORDER))
                                .bg(rgb(HEADER))
                                .shadow_lg()
                                .text_xs(),
                        ),
                )
                .with_priority(2),
            )
            .into_any_element(),
    ]
}

impl GitTrack {
    fn toggle_menu(&mut self, menu: OpenMenu, cx: &mut Context<Self>) {
        self.menu = if self.menu.as_ref() == Some(&menu) {
            None
        } else {
            Some(menu)
        };
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        cx.notify();
    }

    /// "Menu ▾" button with a small dropdown.
    fn render_menu(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let open = self.menu == Some(OpenMenu::Main);
        // Read while the menu is open, so changes made elsewhere show up.
        let autostart = if open {
            autostart::state()
        } else {
            autostart::State::Off
        };
        let other_autostart = match &autostart {
            autostart::State::Other(path) => Some(path.clone()),
            _ => None,
        };
        div()
            .relative()
            .child(
                button("menu", "Menu ▾")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_menu(OpenMenu::Main, cx))),
            )
            .when(open, |d| {
                d.children(dropdown(
                    window,
                    cx.listener(|this, _, _, cx| this.close_menu(cx)),
                    div()
                        .child(menu_item("settings", "Settings…", Some("Ctrl+,")).on_click(
                            cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                        ))
                        .child(menu_separator())
                        .child(menu_label(Config::path().display().to_string()))
                        .child(
                            menu_item("config-open", "Open config in default editor", None)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.open_with_system(&Config::path());
                                    this.close_menu(cx);
                                })),
                        )
                        .child(
                            menu_item("config-reveal", "Show config in Explorer", None).on_click(
                                cx.listener(|this, _, _, cx| {
                                    cx.reveal_path(&Config::path());
                                    this.close_menu(cx);
                                }),
                            ),
                        )
                        .child(menu_separator())
                        .child(
                            menu_check(
                                "autostart",
                                "Start with Windows",
                                autostart == autostart::State::On,
                            )
                            .when_some(other_autostart, |item, other| {
                                item.tooltip(tooltip(format!(
                                    "Currently starts {other}; enabling switches to this copy"
                                )))
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_autostart(cx))),
                        )
                        .child(
                            menu_item(
                                "check-update",
                                if matches!(self.new_version, NewVersion::Checking) {
                                    "Checking for updates…"
                                } else {
                                    "Check for updates"
                                },
                                None,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.check_update(true, cx);
                                this.close_menu(cx);
                            })),
                        )
                        .child(menu_separator())
                        .child(
                            menu_item("quit", "Quit git-track", Some("Ctrl+Q"))
                                .on_click(cx.listener(|this, _, _, cx| this.quit(cx))),
                        ),
                ))
            })
    }

    /// IDE button for a repository inside a JetBrains project: opens the
    /// project in the detected IDE, or offers the installed IDEs.
    fn render_ide_button(
        &self,
        kind: SourceKind,
        repo: &RepoStatus,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.config.jetbrains.enabled {
            return None;
        }
        let project = repo.idea.clone()?;
        let distro = self.distro(kind);
        let repo_path = repo.path.clone();
        let button_id = id(format_args!("{kind:?}:{}:ide", repo.path));

        if let Some(ide) = self.jetbrains.detect(kind, &project, distro.as_deref()) {
            let code = ide.code.clone();
            return Some(
                button(button_id, ide.name.clone())
                    .tooltip(tooltip(format!("Open {project} in {}", ide.name)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_ide(&code, kind, &project, repo_path.clone(), cx)
                    }))
                    .into_any_element(),
            );
        }

        let ides = self.jetbrains.ides();
        if ides.is_empty() {
            return None;
        }
        let menu = OpenMenu::Ide(kind, repo.path.clone());
        let open = self.menu.as_ref() == Some(&menu);
        let items = ides.iter().fold(
            div().child(menu_label(format!("Open {project} in"))),
            |items, ide| {
                let (code, project, repo_path) =
                    (ide.code.clone(), project.clone(), repo_path.clone());
                items.child(
                    menu_item(
                        id(format_args!("{kind:?}:{}:ide:{code}", repo.path)),
                        ide.name.clone(),
                        None,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_menu(cx);
                        this.open_ide(&code, kind, &project, repo_path.clone(), cx)
                    })),
                )
            },
        );
        Some(
            div()
                .relative()
                .child(
                    button(button_id, "IDE ▾")
                        .tooltip(tooltip(format!(
                            "JetBrains project {project}\n(IDE unknown: not in any recent projects list)"
                        )))
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_menu(menu.clone(), cx)),
                        ),
                )
                .when(open, |d| {
                    d.children(dropdown(
                        window,
                        cx.listener(|this, _, _, cx| this.close_menu(cx)),
                        items,
                    ))
                })
                .into_any_element(),
        )
    }

    /// "Ignore ▾": hides the repository or one of its parent folders (up to,
    /// not including, the root) from now on.
    fn render_ignore_button(
        &self,
        kind: SourceKind,
        repo: &RepoStatus,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let menu = OpenMenu::Ignore(kind, repo.path.clone());
        let open = self.menu.as_ref() == Some(&menu);
        let element = div().relative().child(
            button(
                id(format_args!("{kind:?}:{}:ignore", repo.path)),
                "Ignore ▾",
            )
            .text_color(rgb(DIM))
            .tooltip(tooltip(
                "Hide this repository (or a parent folder) from now on",
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_menu(menu.clone(), cx))),
        );
        if !open {
            return element;
        }

        let windows = kind == SourceKind::Windows;
        let parents: Vec<String> = match &repo.root {
            Some(root) => Path::new(&repo.path)
                .ancestors()
                .skip(1)
                .map(|p| p.to_string_lossy().into_owned())
                .take_while(|p| {
                    git_track_core::discover::path_within(p, root, windows)
                        && !git_track_core::discover::path_within(root, p, windows)
                })
                .collect(),
            None => Vec::new(),
        };
        let entry = |label: String, path: String, index: usize| {
            menu_item(
                id(format_args!("{kind:?}:{}:ignore{index}", repo.path)),
                label,
                None,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.ignore_path(kind, path.clone(), cx)))
        };
        let mut items = div()
            .child(menu_label("Ignore from now on (undo in Settings)"))
            .child(entry("This repository".into(), repo.path.clone(), 0));
        for (i, parent) in parents.into_iter().enumerate() {
            items = items.child(entry(format!("Folder {parent}"), parent, i + 1));
        }
        element.children(dropdown(
            window,
            cx.listener(|this, _, _, cx| this.close_menu(cx)),
            items,
        ))
    }

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let Summary {
            total, scanning, ..
        } = self.summary();
        let (summary, color) = self.summary().text();

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_4()
            .py_3()
            .bg(rgb(HEADER))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .child("git-track"),
            )
            .child(div().text_color(rgb(color)).child(summary))
            .child(
                div()
                    .text_color(rgb(DIM))
                    .child(format!("· {total} checked")),
            )
            .child(div().flex_1())
            .when(scanning, |d| {
                d.child(div().text_color(rgb(ACCENT)).child("scanning…"))
            })
            .child(
                button(
                    "toggle-clean",
                    if self.show_clean {
                        "Hide clean"
                    } else {
                        "Show clean"
                    },
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_clean = !this.show_clean;
                    cx.notify();
                })),
            )
            .child(self.render_menu(window, cx))
            .child(
                button("refresh", "Refresh all")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_all(cx))),
            )
    }

    fn render_source(
        &self,
        source: &Source,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let kind = source.kind;
        let status = match source.phase {
            Phase::Scanning {
                checked,
                total: Some(total),
            } => format!("checking {checked}/{total}…"),
            Phase::Scanning { .. } => "searching for repositories…".to_string(),
            Phase::Idle => match source.last_scan {
                Some((at, ms)) => format!("scanned {} · {:.1}s", ago(at), ms as f64 / 1000.0),
                None => "not scanned yet".to_string(),
            },
        };
        let rows: Vec<&RepoStatus> = source
            .repos
            .values()
            .filter(|r| self.show_clean || !r.is_clean())
            .collect();
        let no_roots = self.config.roots(kind).is_empty();
        let all_pushed = rows.is_empty()
            && !no_roots
            && !source.scanning()
            && source.error.is_none()
            && source.last_scan.is_some();

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .pb_1()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(source.title()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(rgb(DIM))
                            .text_xs()
                            .child(self.config.roots(kind).join("  ·  ")),
                    )
                    .child(div().text_color(rgb(DIM)).text_xs().child(status))
                    .child(
                        button(id(format_args!("rescan-{kind:?}")), "Rescan")
                            .on_click(cx.listener(move |this, _, _, cx| this.start_scan(kind, cx))),
                    ),
            )
            .children(
                source
                    .error
                    .iter()
                    .chain(&source.root_errors)
                    .chain(&source.warning)
                    .map(|e| div().text_color(rgb(DANGER)).text_xs().child(e.clone())),
            )
            .when(no_roots, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .text_color(rgb(DIM))
                        .child("No folders to scan yet.")
                        .child(
                            button(id(format_args!("settings-{kind:?}")), "Add folders…").on_click(
                                cx.listener(|this, _, window, cx| this.open_settings(window, cx)),
                            ),
                        ),
                )
            })
            .when(all_pushed, |d| {
                d.child(div().text_color(rgb(OK)).child(format!(
                    "All {} repositories are pushed.",
                    source.repos.len()
                )))
            })
            .children(
                rows.into_iter()
                    .map(|repo| self.render_repo(kind, repo, window, cx)),
            )
    }

    fn render_repo(
        &self,
        kind: SourceKind,
        repo: &RepoStatus,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let ide_button = self.render_ide_button(kind, repo, window, cx);
        let path = repo.path.clone();
        let busy = self.busy.contains(&(kind, path.clone()));
        let accent = if repo.error.is_some()
            || repo.issues.iter().any(|i| i.severity() == Severity::Danger)
        {
            DANGER
        } else if !repo.issues.is_empty() {
            WARNING
        } else {
            OK
        };
        let branch = match (&repo.branch, &repo.head) {
            (Some(b), _) => b.clone(),
            (None, Some(head)) => format!("detached @ {head}"),
            (None, None) => "no commits".into(),
        };

        let mut chips: Vec<Stateful<Div>> = repo
            .issues
            .iter()
            .enumerate()
            .map(|(i, issue)| {
                let (fg, bg) = match issue.severity() {
                    Severity::Danger => (DANGER, DANGER_BG),
                    Severity::Warning => (WARNING, WARNING_BG),
                };
                chip(
                    id(format_args!("{kind:?}:{path}:issue{i}")),
                    issue.label(),
                    fg,
                    bg,
                )
                .tooltip(tooltip(issue.description()))
            })
            .collect();
        if let Some(error) = &repo.error {
            chips.push(
                chip(
                    id(format_args!("{kind:?}:{path}:error")),
                    "error".into(),
                    DANGER,
                    DANGER_BG,
                )
                .tooltip(tooltip(error.clone())),
            );
        }
        if repo.is_clean() {
            chips.push(chip(
                id(format_args!("{kind:?}:{path}:ok")),
                "pushed".into(),
                OK,
                PANEL,
            ));
        }

        let action_buttons: Vec<_> = self
            .config
            .actions_for(kind)
            .enumerate()
            .map(|(i, action)| {
                let path = path.clone();
                button(
                    id(format_args!("{kind:?}:{path}:action{i}")),
                    action.label.clone(),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.run_action(kind, i, path.clone(), cx)),
                )
            })
            .collect();

        let copy_path = path.clone();
        let recheck_path = path.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .pl_3()
            .pr_2()
            .py_2()
            .rounded_md()
            .bg(rgb(PANEL))
            .border_l_2()
            .border_color(dim(accent))
            .when(repo.is_clean(), |d| d.opacity(0.7))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w(px(0.))
                            .child(
                                div()
                                    .id(id(format_args!("{kind:?}:{path}:name")))
                                    .min_w(px(0.))
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(rgb(ACCENT)))
                                    .child(repo.display_path().to_string())
                                    .tooltip(tooltip(format!("{path}\n(click to copy)")))
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_path.clone(),
                                        ));
                                    })),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(rgb(DIM))
                                    .text_xs()
                                    .child(branch),
                            ),
                    )
                    .child(div().flex().flex_row().flex_wrap().gap_1().children(chips)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .gap_1()
                    .children(ide_button)
                    .children(action_buttons)
                    .child(self.render_ignore_button(kind, repo, window, cx))
                    .child(
                        button(
                            id(format_args!("{kind:?}:{path}:recheck")),
                            if busy { "…" } else { "↻" },
                        )
                        .tooltip(tooltip("Check again"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.recheck(kind, recheck_path.clone(), cx)
                        })),
                    ),
            )
    }
}

impl Render for GitTrack {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sections: Vec<_> = self
            .sources
            .iter()
            .filter(|s| self.config.enabled(s.kind))
            .map(|s| self.render_source(s, window, cx).into_any_element())
            .collect();

        div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_sm()
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Quit, _, cx| this.quit(cx)))
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key != "escape" {
                    return;
                }
                if this.settings.is_some() {
                    this.close_settings(window, cx);
                } else if this.menu.is_some() {
                    this.menu = None;
                    cx.notify();
                }
            }))
            // Window-wide so a scrollbar drag keeps working outside the bar.
            .on_mouse_move(cx.listener(|this, event, _, cx| {
                if this.scrollbar.mouse_move(event) {
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.scrollbar.mouse_up() {
                        cx.notify();
                    }
                }),
            )
            .child(self.render_header(window, cx))
            .children(self.render_update_banner(cx))
            .children(self.config_error.clone().map(|e| {
                banner(
                    format!("Config error (using previous/default settings): {e}"),
                    DANGER,
                    DANGER_BG,
                )
            }))
            .children(self.notice.clone().map(|notice| {
                banner(notice, WARNING, WARNING_BG)
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(div().flex_1())
                    .child(button("dismiss-notice", "Dismiss").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.notice = None;
                            cx.notify();
                        },
                    )))
            }))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .id("body")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scrollbar.handle)
                            // Redraw so the scrollbar thumb follows the wheel.
                            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                            .p_4()
                            .pr_6()
                            .flex()
                            .flex_col()
                            .gap_6()
                            .children(sections),
                    )
                    .children(self.scrollbar.render(cx.listener(
                        |this, event: &MouseDownEvent, _, cx| {
                            this.scrollbar.mouse_down(event);
                            cx.stop_propagation();
                            cx.notify();
                        },
                    ))),
            )
            .children(self.settings.as_ref().map(|(view, _)| view.clone()))
    }
}

fn main() {
    // Before the single-instance check: the previous version may still be exiting.
    let updated = update::after_start();
    // Started at login: stay in the tray (shown anyway if there is no tray icon).
    let hidden = autostart::launched();
    let instance_commands = match instance::acquire() {
        instance::Instance::Primary(commands) => commands,
        // The running instance shows its window instead.
        instance::Instance::Secondary => return,
    };

    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("ctrl-,", OpenSettings, None),
        ]);
        text_input::bind_keys(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1100.), px(760.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("git-track".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(640.), px(320.))),
                show: !hidden,
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| GitTrack::new(window, cx, instance_commands, hidden, updated))
            },
        )
        .expect("failed to open window");
        if !hidden {
            cx.activate(true);
        }
    });
}
