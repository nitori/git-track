#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod config;
mod scanner;
mod scrollbar;

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use futures::StreamExt;
use git_track_core::{Event, PROTOCOL_VERSION, RepoStatus, Severity};
use gpui::{
    AnyView, App, Application, Bounds, ClipboardItem, Context, Corner, Div, ElementId, FontWeight,
    Hsla, MouseButton, MouseDownEvent, SharedString, Stateful, Subscription, Task, TitlebarOptions,
    Window, WindowBounds, WindowOptions, anchored, deferred, div, point, prelude::*, px, rgb, size,
};

use config::Config;
use scanner::{Msg, SourceKind};

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
}

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
                "Agent {version} speaks protocol {protocol}, expected {PROTOCOL_VERSION}. Reinstall the agent inside WSL."
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

struct GitTrack {
    config: Config,
    config_error: Option<String>,
    sources: Vec<Source>,
    show_clean: bool,
    /// Repositories currently being re-checked individually.
    busy: HashSet<(SourceKind, String)>,
    /// Repositories an action was launched for; re-checked whenever the window
    /// regains focus until they are clean.
    touched: HashSet<(SourceKind, String)>,
    notice: Option<String>,
    last_full_refresh: Instant,
    scrollbar: scrollbar::Scrollbar,
    config_menu_open: bool,
    _ticker: Task<()>,
    _activation: Subscription,
}

impl GitTrack {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (config, config_error) = match Config::load() {
            Ok(config) => (config, None),
            Err(e) => (Config::default(), Some(e)),
        };

        let activation = cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.recheck_touched(cx);
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

        let mut this = Self {
            config,
            config_error,
            sources: SOURCES.into_iter().map(Source::new).collect(),
            show_clean: false,
            busy: HashSet::new(),
            touched: HashSet::new(),
            notice: None,
            last_full_refresh: Instant::now(),
            scrollbar: scrollbar::Scrollbar::default(),
            config_menu_open: false,
            _ticker: ticker,
            _activation: activation,
        };
        this.start_all(cx);
        this
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
                source.phase = Phase::Idle;
                source.repos.clear();
            }
        }
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
                    this.source_mut(kind).apply_scan(msg);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.source_mut(kind).finish_scan();
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
        let key = (kind, path.clone());
        if !self.busy.insert(key.clone()) {
            return;
        }
        let mut rx = scanner::start(kind, self.config.request(kind, vec![path]), &self.config);
        cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.next().await {
                let alive = this.update(cx, |this, cx| {
                    this.apply_recheck(kind, msg);
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.busy.remove(&key);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn apply_recheck(&mut self, kind: SourceKind, msg: Msg) {
        match msg {
            Msg::Event(Event::Repo(mut repo)) => {
                let source = self.source_mut(kind);
                if repo.root.is_none() {
                    repo.root = source
                        .repos
                        .get(&repo.path)
                        .and_then(|old| old.root.clone());
                }
                let key = (kind, repo.path.clone());
                let clean = repo.is_clean();
                source.repos.insert(repo.path.clone(), repo);
                if clean {
                    self.touched.remove(&key);
                }
            }
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

    fn recheck_touched(&mut self, cx: &mut Context<Self>) {
        let touched: Vec<_> = self.touched.iter().cloned().collect();
        for (kind, path) in touched {
            self.recheck(kind, path, cx);
        }
    }

    fn run_action(&mut self, kind: SourceKind, index: usize, path: String, cx: &mut Context<Self>) {
        let Some(action) = self.config.actions_for(kind).nth(index).cloned() else {
            return;
        };
        let distro = self
            .source(kind)
            .distro
            .clone()
            .or_else(|| self.config.wsl.distro.clone());
        let vars = actions::Vars {
            kind,
            path: &path,
            distro: distro.as_deref(),
        };
        match actions::launch(&action, &vars) {
            Ok(()) => {
                self.touched.insert((kind, path));
            }
            Err(e) => self.notice = Some(e),
        }
        cx.notify();
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

impl GitTrack {
    /// "Config ▾" button with a small dropdown.
    fn render_config_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let item = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_sm()
                .whitespace_nowrap()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(BUTTON_HOVER)))
                .child(label)
        };
        let close = |this: &mut Self, cx: &mut Context<Self>| {
            this.config_menu_open = false;
            cx.notify();
        };

        div()
            .relative()
            .child(
                button("config", "Config ▾")
                    .tooltip(tooltip(Config::path().display().to_string()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.config_menu_open = !this.config_menu_open;
                        cx.notify();
                    })),
            )
            .when(self.config_menu_open, |d| {
                d.child(
                    // Transparent full-window backdrop: a click anywhere else closes the menu.
                    deferred(
                        anchored().position(point(px(0.), px(0.))).child(
                            div()
                                .id("config-menu-backdrop")
                                .w(window.viewport_size().width)
                                .h(window.viewport_size().height)
                                .occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, _, cx| close(this, cx)),
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, _, _, cx| close(this, cx)),
                                ),
                        ),
                    )
                    .with_priority(1),
                )
                .child(
                    // `anchored` takes its own layout origin as the anchor point, so pin a
                    // zero-size box to the button's bottom-right corner and anchor there.
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
                                        div()
                                            .occlude()
                                            .flex()
                                            .flex_col()
                                            .p_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(BORDER))
                                            .bg(rgb(HEADER))
                                            .shadow_lg()
                                            .text_xs()
                                            .child(
                                                item("config-open", "Open in default editor")
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            cx.open_with_system(&Config::path());
                                                            close(this, cx);
                                                        },
                                                    )),
                                            )
                                            .child(
                                                item("config-reveal", "Show in Explorer").on_click(
                                                    cx.listener(move |this, _, _, cx| {
                                                        cx.reveal_path(&Config::path());
                                                        close(this, cx);
                                                    }),
                                                ),
                                            ),
                                    ),
                            )
                            .with_priority(2),
                        ),
                )
            })
    }

    fn render_header(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let enabled: Vec<&Source> = self
            .sources
            .iter()
            .filter(|s| self.config.enabled(s.kind))
            .collect();
        let total: usize = enabled.iter().map(|s| s.repos.len()).sum();
        let dirty: usize = enabled.iter().map(|s| s.dirty_count()).sum();
        let scanning = enabled.iter().any(|s| s.scanning());

        let (summary, color) = if dirty > 0 {
            (
                format!(
                    "{dirty} {} with unpushed work",
                    if dirty == 1 {
                        "repository"
                    } else {
                        "repositories"
                    }
                ),
                DANGER,
            )
        } else if scanning && total == 0 {
            ("scanning…".to_string(), DIM)
        } else {
            ("everything is pushed".to_string(), OK)
        };

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
            .child(self.render_config_menu(window, cx))
            .child(
                button("refresh", "Refresh all")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_all(cx))),
            )
    }

    fn render_source(&self, source: &Source, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
        let all_pushed = rows.is_empty()
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
            .when(all_pushed, |d| {
                d.child(div().text_color(rgb(OK)).child(format!(
                    "All {} repositories are pushed.",
                    source.repos.len()
                )))
            })
            .children(
                rows.into_iter()
                    .map(|repo| self.render_repo(kind, repo, cx)),
            )
    }

    fn render_repo(
        &self,
        kind: SourceKind,
        repo: &RepoStatus,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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
                    .children(action_buttons)
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
            .map(|s| self.render_source(s, cx).into_any_element())
            .collect();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_sm()
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
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
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
                ..Default::default()
            },
            |window, cx| cx.new(|cx| GitTrack::new(window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
