//! The settings dialog: a panel floating over the main view that edits the
//! commonly changed parts of the config. Everything else (actions, exclude
//! patterns, branches, …) stays in the TOML file.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, Context, Entity, EventEmitter, FontWeight, MouseButton,
    MouseDownEvent, PathPromptOptions, SharedString, Stateful, Subscription, Window, div,
    ease_out_quint, prelude::*, px, relative, rgb, rgba,
};

use crate::colors::*;
use crate::config::Config;
use crate::scanner::SourceKind;
use crate::scrollbar::Scrollbar;
use crate::text_input::{Submitted, TextInput};
use crate::{SOURCES, autostart, button, id, tooltip};

/// Labels for the `[track]` toggles, in config order.
const TRACK_LABELS: [(&str, &str); 12] = [
    ("staged", "Staged changes"),
    ("modified", "Modified files"),
    ("untracked", "Untracked files"),
    ("conflicted", "Merge conflicts"),
    ("unpushed", "Unpushed commits"),
    ("ahead_elsewhere", "Ahead, but pushed to another branch"),
    ("no_upstream", "Branch without upstream"),
    ("upstream_gone", "Upstream deleted"),
    ("detached_head", "Commits on a detached HEAD"),
    ("no_remote", "No remote at all"),
    ("stash", "Stashes"),
    ("in_progress", "Unfinished merge / rebase / …"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum List {
    Roots,
    Ignore,
}

/// A tab of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Issues,
    Source(SourceKind),
}

impl Page {
    const ALL: [Page; 4] = [
        Page::General,
        Page::Issues,
        Page::Source(SourceKind::Windows),
        Page::Source(SourceKind::Wsl),
    ];

    fn title(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Issues => "Reported issues",
            Page::Source(SourceKind::Windows) => "Windows",
            Page::Source(SourceKind::Wsl) => "WSL",
        }
    }
}

pub enum SettingsEvent {
    /// The config file was written.
    Saved,
    Closed,
}

pub struct Settings {
    draft: Config,
    autostart: bool,
    /// Another executable is registered for autostart.
    autostart_other: Option<String>,
    refresh: Entity<TextInput>,
    distro: Entity<TextInput>,
    /// "Add" inputs for every (source, list).
    inputs: Vec<((SourceKind, List), Entity<TextInput>)>,
    error: Option<String>,
    /// Generation of the "Saved" message while it is shown.
    saved: Option<usize>,
    saves: usize,
    page: Page,
    scrollbar: Scrollbar,
    _subscriptions: Vec<Subscription>,
}

/// How long the "Saved" message stays.
const SAVED_SHOWN: Duration = Duration::from_secs(3);

impl EventEmitter<SettingsEvent> for Settings {}

impl Settings {
    /// `load_error`: the config file could not be read (`draft` is the last good config).
    pub fn new(draft: Config, load_error: Option<String>, cx: &mut Context<Self>) -> Self {
        let refresh = cx.new(|cx| TextInput::new(cx, draft.refresh_interval_secs.to_string(), ""));
        let distro = cx.new(|cx| {
            TextInput::new(
                cx,
                draft.wsl.distro.clone().unwrap_or_default(),
                "default distribution",
            )
        });
        let mut subscriptions = Vec::new();
        for input in [&refresh, &distro] {
            subscriptions.push(cx.subscribe(input, |this, _, _: &Submitted, cx| this.save(cx)));
        }

        let mut inputs = Vec::new();
        for kind in SOURCES {
            for list in [List::Roots, List::Ignore] {
                let placeholder = match (kind, list) {
                    (SourceKind::Windows, List::Roots) => r"Add a folder, e.g. C:\Users\me\code",
                    (SourceKind::Windows, List::Ignore) => r"Add a repository or folder to skip",
                    (SourceKind::Wsl, List::Roots) => "Add a folder, e.g. ~/projects",
                    (SourceKind::Wsl, List::Ignore) => "Add a repository or folder to skip",
                };
                let input = cx.new(|cx| TextInput::new(cx, "", placeholder));
                subscriptions.push(cx.subscribe(&input, move |this, _, _: &Submitted, cx| {
                    if let Err(e) = this.add_typed(kind, list, cx) {
                        this.error = Some(e);
                    }
                    cx.notify();
                }));
                inputs.push(((kind, list), input));
            }
        }

        let (autostart, autostart_other) = match autostart::state() {
            autostart::State::On => (true, None),
            autostart::State::Off => (false, None),
            autostart::State::Other(path) => (false, Some(path)),
        };

        Self {
            draft,
            autostart,
            autostart_other,
            refresh,
            distro,
            inputs,
            error: load_error.map(|e| format!("The config file has errors, saving may fail: {e}")),
            saved: None,
            saves: 0,
            page: Page::General,
            scrollbar: Scrollbar::default(),
            _subscriptions: subscriptions,
        }
    }

    fn input(&self, kind: SourceKind, list: List) -> &Entity<TextInput> {
        &self
            .inputs
            .iter()
            .find(|(key, _)| *key == (kind, list))
            .expect("an input for every list")
            .1
    }

    fn list(&self, kind: SourceKind, list: List) -> &[String] {
        match list {
            List::Roots => self.draft.roots(kind),
            List::Ignore => self.draft.ignored(kind),
        }
    }

    fn list_mut(&mut self, kind: SourceKind, list: List) -> &mut Vec<String> {
        match (kind, list) {
            (SourceKind::Windows, List::Roots) => &mut self.draft.windows.roots,
            (SourceKind::Wsl, List::Roots) => &mut self.draft.wsl.roots,
            (_, List::Ignore) => self.draft.ignored_mut(kind),
        }
    }

    /// Adds a path to a list (normalized, without duplicates).
    fn add(&mut self, kind: SourceKind, list: List, path: &str) -> Result<(), String> {
        let path = normalize_path(kind, path)?;
        let windows = kind == SourceKind::Windows;
        let entries = self.list_mut(kind, list);
        let exists = entries.iter().any(|p| {
            if windows {
                p.eq_ignore_ascii_case(&path)
            } else {
                *p == path
            }
        });
        if !exists {
            entries.push(path);
        }
        Ok(())
    }

    /// Adds what was typed into the list's input and clears it.
    fn add_typed(
        &mut self,
        kind: SourceKind,
        list: List,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let input = self.input(kind, list).clone();
        let text = input.read(cx).text().trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        self.add(kind, list, &text)?;
        input.update(cx, |input, cx| input.set_text("", cx));
        self.error = None;
        Ok(())
    }

    fn remove(&mut self, kind: SourceKind, list: List, index: usize, cx: &mut Context<Self>) {
        let entries = self.list_mut(kind, list);
        if index < entries.len() {
            entries.remove(index);
        }
        cx.notify();
    }

    /// Picks folders with the system dialog. WSL folders are picked below
    /// `\\wsl.localhost\<distro>` and converted to Linux paths.
    fn browse(&mut self, kind: SourceKind, list: List, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Select".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = prompt.await else {
                return;
            };
            this.update(cx, |this, cx| {
                for path in paths {
                    if let Err(e) = this.add(kind, list, &path.to_string_lossy()) {
                        this.error = Some(e);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        match self.try_save(cx) {
            Ok(()) => {
                self.error = None;
                self.saves += 1;
                let generation = self.saves;
                self.saved = Some(generation);
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SAVED_SHOWN).await;
                    this.update(cx, |this, cx| {
                        if this.saved == Some(generation) {
                            this.saved = None;
                            cx.notify();
                        }
                    })
                    .ok();
                })
                .detach();
                cx.emit(SettingsEvent::Saved);
            }
            Err(e) => {
                self.error = Some(e);
                self.saved = None;
            }
        }
        cx.notify();
    }

    fn try_save(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let refresh = self.refresh.read(cx).text().trim().to_string();
        self.draft.refresh_interval_secs = match refresh.parse() {
            Ok(secs) => secs,
            Err(_) => {
                self.page = Page::General;
                return Err(format!(
                    "Rescan interval: '{refresh}' is not a number of seconds"
                ));
            }
        };
        let distro = self.distro.read(cx).text().trim().to_string();
        self.draft.wsl.distro = (!distro.is_empty()).then_some(distro);
        // Paths typed but not added yet are meant to be saved, too.
        for kind in SOURCES {
            for list in [List::Roots, List::Ignore] {
                self.add_typed(kind, list, cx)?;
            }
        }

        self.draft.save()?;
        if self.autostart != (autostart::state() == autostart::State::On) {
            autostart::set(self.autostart)?;
        }
        Ok(())
    }

    fn render_list(
        &self,
        kind: SourceKind,
        list: List,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (title, hint) = match list {
            List::Roots => ("Folders to scan", "searched recursively for repositories"),
            List::Ignore => (
                "Ignored",
                "repositories at or below these paths are skipped",
            ),
        };
        let entries = self.list(kind, list);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_baseline()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(div().text_xs().text_color(rgb(DIM)).child(hint)),
            )
            .when(entries.is_empty(), |d| {
                d.child(div().text_xs().text_color(rgb(DIM)).child("none"))
            })
            .children(entries.iter().enumerate().map(|(i, path)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py(px(2.))
                    .rounded_md()
                    .bg(rgb(BG))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .child(path.clone()),
                    )
                    .child(
                        button(
                            id(format_args!("settings:{kind:?}:{list:?}:remove{i}")),
                            "Remove",
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.remove(kind, list, i, cx)),
                        ),
                    )
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .child(self.input(kind, list).clone()),
                    )
                    .child(
                        button(id(format_args!("settings:{kind:?}:{list:?}:add")), "Add").on_click(
                            cx.listener(move |this, _, _, cx| {
                                if let Err(e) = this.add_typed(kind, list, cx) {
                                    this.error = Some(e);
                                }
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        button(
                            id(format_args!("settings:{kind:?}:{list:?}:browse")),
                            "Browse…",
                        )
                        .when(kind == SourceKind::Wsl, |b| {
                            b.tooltip(tooltip(r"Pick a folder below \\wsl.localhost\<distro>"))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.browse(kind, list, cx))),
                    ),
            )
    }

    fn render_source(&self, kind: SourceKind, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let enabled = self.draft.enabled(kind);
        let title = match kind {
            SourceKind::Windows => "Windows",
            SourceKind::Wsl => "WSL",
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title(title))
            .child(
                checkbox(
                    id(format_args!("settings:{kind:?}:enabled")),
                    enabled,
                    "Scan",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    match kind {
                        SourceKind::Windows => this.draft.windows.enabled = !enabled,
                        SourceKind::Wsl => this.draft.wsl.enabled = !enabled,
                    }
                    cx.notify();
                })),
            )
            .when(kind == SourceKind::Wsl, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child("Distribution")
                        .child(div().w(px(200.)).child(self.distro.clone())),
                )
            })
            .child(self.render_list(kind, List::Roots, cx))
            .child(self.render_list(kind, List::Ignore, cx))
    }

    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tray = self.draft.close_to_tray;
        let updates = self.draft.check_updates;
        let jetbrains = self.draft.jetbrains.enabled;
        let autostart = self.autostart;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("General"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child("Full rescan every")
                    .child(div().w(px(80.)).child(self.refresh.clone()))
                    .child("seconds")
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child("(0 = only manually)"),
                    ),
            )
            .child(
                checkbox(
                    "settings:tray",
                    tray,
                    "Closing the window hides it in the system tray",
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child("(after a restart)"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.draft.close_to_tray = !tray;
                    cx.notify();
                })),
            )
            .child(
                checkbox(
                    "settings:autostart",
                    autostart,
                    "Start with Windows (hidden in the tray)",
                )
                .children(self.autostart_other.as_ref().map(|other| {
                    div()
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(format!("(currently starts {other})"))
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.autostart = !autostart;
                    cx.notify();
                })),
            )
            .child(
                checkbox(
                    "settings:updates",
                    updates,
                    "Check for new versions (installing always asks first)",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.draft.check_updates = !updates;
                    cx.notify();
                })),
            )
            .child(
                checkbox(
                    "settings:jetbrains",
                    jetbrains,
                    "JetBrains IDE button for repositories inside a .idea project",
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.draft.jetbrains.enabled = !jetbrains;
                    cx.notify();
                })),
            )
    }

    fn render_track(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_title("Reported issues"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_y_2()
                    .children(TRACK_LABELS.iter().map(|&(name, label)| {
                        let on = self.draft.track.get(name).unwrap_or(true);
                        div().w(relative(1. / 3.)).min_w(px(240.)).child(
                            checkbox(id(format_args!("settings:track:{name}")), on, label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let _ = this.draft.track.set(name, !on);
                                    cx.notify();
                                })),
                        )
                    })),
            )
    }
}

impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = div()
            .id("settings-body")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scrollbar.handle)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            .px_5()
            .py_4()
            .child(match self.page {
                Page::General => self.render_general(cx).into_any_element(),
                Page::Issues => self.render_track(cx).into_any_element(),
                Page::Source(kind) => self.render_source(kind, cx).into_any_element(),
            });

        let tabs = div()
            .w(px(180.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_r_1()
            .border_color(rgb(BORDER))
            .children(Page::ALL.map(|page| {
                let selected = page == self.page;
                let off = matches!(page, Page::Source(kind) if !self.draft.enabled(kind));
                div()
                    .id(id(format_args!("settings:tab:{page:?}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .border_l_2()
                    .cursor_pointer()
                    .when(selected, |d| {
                        d.bg(rgb(BUTTON))
                            .border_color(rgb(ACCENT))
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!selected, |d| {
                        d.border_color(gpui::transparent_black())
                            .text_color(rgb(DIM))
                            .hover(|s| s.bg(rgb(BUTTON_HOVER)).text_color(rgb(TEXT)))
                    })
                    .child(div().flex_1().child(page.title()))
                    .when(off, |d| {
                        d.child(div().text_xs().text_color(rgb(DIM)).child("off"))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.page != page {
                            this.page = page;
                            this.scrollbar
                                .handle
                                .set_offset(gpui::point(px(0.), px(0.)));
                            cx.notify();
                        }
                    }))
            }));

        let panel = div()
            .id("settings-panel")
            .w_full()
            .max_w(px(1024.))
            .h_full()
            .max_h(px(600.))
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .shadow_lg()
            // Clicks inside must not reach the backdrop.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_5()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child(Config::path().display().to_string()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .child(tabs)
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_w(px(0.))
                            .child(body)
                            .children(self.scrollbar.render(cx.listener(
                                |this, event: &MouseDownEvent, _, cx| {
                                    this.scrollbar.mouse_down(event);
                                    cx.stop_propagation();
                                    cx.notify();
                                },
                            ))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_5()
                    .py_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_xs()
                            .text_color(rgb(DANGER))
                            .children(self.error.clone()),
                    )
                    .child(
                        div().text_xs().text_color(rgb(DIM)).child(
                            "Actions, excluded folder names and branches: edit the config file",
                        ),
                    )
                    .children(self.saved.map(|generation| {
                        div()
                            .id(id(format_args!("settings:saved{generation}")))
                            .flex_shrink_0()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(OK))
                            .child("✓ Saved")
                            .with_animation(
                                id(format_args!("settings:saved-fade{generation}")),
                                Animation::new(SAVED_SHOWN),
                                // Quick fade-in, hold, fade out at the end.
                                |label, t| label.opacity((t / 0.05).min((1. - t) / 0.15).min(1.)),
                            )
                    }))
                    .child(
                        button("settings:close", "Close")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Closed))),
                    )
                    .child(
                        primary_button("settings:save", "Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            );

        div()
            .id("settings-overlay")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(rgba(0x0000_0099))
            .flex()
            .items_center()
            .justify_center()
            .p_8()
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
            .child(panel)
            .with_animation(
                "settings-fade-in",
                Animation::new(Duration::from_millis(160)).with_easing(ease_out_quint()),
                |overlay, delta| overlay.opacity(delta),
            )
    }
}

fn primary_button(id: &'static str, label: &'static str) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .px_3()
        .py(px(3.))
        .rounded_md()
        .border_1()
        .border_color(rgb(ACCENT))
        .bg(rgb(ACCENT))
        .hover(|s| s.bg(rgb(0x86aef9)))
        .cursor_pointer()
        .whitespace_nowrap()
        .text_xs()
        .text_color(rgb(BG))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label)
}

fn section_title(title: &'static str) -> impl IntoElement {
    div()
        .pb_1()
        .border_b_1()
        .border_color(rgb(BORDER))
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
}

fn checkbox(
    id: impl Into<gpui::ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .child(
            div()
                .size(px(14.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(rgb(if checked { ACCENT } else { BORDER }))
                .bg(rgb(if checked { ACCENT } else { BG }))
                .text_color(rgb(BG))
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .child(if checked { "✓" } else { "" }),
        )
        .child(label.into())
}

/// Cleans up a path for `kind`: trims it, drops trailing separators and turns
/// `\\wsl.localhost\<distro>\…` (or `\\wsl$\…`) into a Linux path for WSL.
fn normalize_path(kind: SourceKind, path: &str) -> Result<String, String> {
    let path = path.trim();
    match kind {
        SourceKind::Windows => {
            let trimmed = path.trim_end_matches(['\\', '/']);
            // Keep the separator of a drive root ("C:\").
            Ok(if trimmed.len() <= 2 && trimmed.ends_with(':') {
                format!("{trimmed}\\")
            } else {
                trimmed.to_string()
            })
        }
        SourceKind::Wsl => {
            let unc = path.replace('/', "\\");
            let lower = unc.to_lowercase();
            let rest = ["\\\\wsl.localhost\\", "\\\\wsl$\\"]
                .iter()
                .find_map(|prefix| lower.starts_with(prefix).then(|| &unc[prefix.len()..]));
            let linux = match rest {
                // Skip the distribution name.
                Some(rest) => format!(
                    "/{}",
                    rest.split_once('\\')
                        .map_or("", |(_, p)| p)
                        .replace('\\', "/")
                ),
                None if path.starts_with('/') || path.starts_with('~') => path.to_string(),
                None => {
                    return Err(format!(
                        r"'{path}' is no WSL path: use /…, ~/… or a folder below \\wsl.localhost\<distro>"
                    ));
                }
            };
            let trimmed = linux.trim_end_matches('/');
            Ok(if trimmed.is_empty() {
                "/".into()
            } else {
                trimmed.to_string()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        let win = |p| normalize_path(SourceKind::Windows, p).unwrap();
        assert_eq!(win(r" D:\Code\ "), r"D:\Code");
        assert_eq!(win(r"C:\"), r"C:\");
        let wsl = |p| normalize_path(SourceKind::Wsl, p);
        assert_eq!(
            wsl(r"\\wsl.localhost\Debian\home\me\p\").unwrap(),
            "/home/me/p"
        );
        assert_eq!(wsl(r"\\WSL$\Ubuntu\srv").unwrap(), "/srv");
        assert_eq!(wsl(r"\\wsl.localhost\Debian").unwrap(), "/");
        assert_eq!(wsl("~/projects/").unwrap(), "~/projects");
        assert!(wsl(r"D:\Code").is_err());
    }
}
