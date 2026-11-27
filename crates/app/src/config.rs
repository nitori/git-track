//! User configuration, stored as TOML in `%APPDATA%\git-track\config.toml`.
//!
//! The file is re-read on every full refresh, so edits apply without a restart.
//! The settings window writes its values back with `toml_edit`, which keeps
//! the comments and everything it does not manage.

use std::path::PathBuf;

use git_track_core::discover::path_within;
use git_track_core::{ScanOptions, ScanRequest, TrackOptions};
use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, TableLike, Value};

use crate::scanner::SourceKind;

/// Written on first start. Must deserialize to the same value as `Config::default()`.
pub const DEFAULT_CONFIG: &str = r#"# git-track configuration
# Changes are picked up on the next refresh (button or automatic).

# Automatic full rescan interval in seconds (0 = manual refresh only).
refresh_interval_secs = 600

# Closing the window only hides it to the system tray (quit via the tray menu,
# "Menu ▾" → Quit or Ctrl+Q). false = no tray icon, closing quits. Needs a restart.
close_to_tray = true

# Look for a new release on GitHub after starting and twice a day. Installing
# an update always asks first.
check_updates = true

# What counts as "not pushed". Set an entry to false to ignore that kind of issue.
[track]
staged = true           # changes added to the index
modified = true         # modified/deleted tracked files that are not staged
untracked = true        # untracked files (false also makes scans faster)
conflicted = true       # merge conflicts
unpushed = true         # branch ahead of its upstream, commits on no remote branch
ahead_elsewhere = true  # branch ahead of its upstream, but the commits exist on another remote branch
no_upstream = true      # branch without upstream that has commits on no remote branch
upstream_gone = true    # upstream deleted on the remote, commits on no remote branch
detached_head = true    # detached HEAD with commits on no remote branch
no_remote = true        # repository has commits but no remote at all
stash = true            # stash entries
in_progress = true      # unfinished merge / rebase / cherry-pick / revert / bisect

[scan]
# Directory names that are never descended into (case-insensitive).
# Do not add "vendor" or similar if you keep git checkouts there.
exclude_dirs = ["node_modules", "__pycache__", ".venv", ".venv-linux", ".pnpm-store", ".yarn", ".cache", ".gradle", ".idea", ".vs"]
# Maximum directory depth below a root.
max_depth = 12
# Branches checked for unpushed commits in addition to the checked-out one.
important_branches = ["main", "master"]
# Check every local branch instead.
check_all_branches = false

# IDE button for repositories that belong to a JetBrains project (a .idea folder
# in the repository or a parent directory up to the root). The IDE is looked up
# in the IDEs' recent projects; if it is unknown, the button lists the installed
# IDEs (found via JetBrains Toolbox or standard install locations).
[jetbrains]
enabled = true

[windows]
enabled = true
# Folders that are searched for repositories, e.g. ['C:\Users\me\projects', 'D:\code']
roots = []
# Repositories and folders to skip (everything at or below these paths).
ignore = []
# git = 'C:\Program Files\Git\cmd\git.exe'

[wsl]
enabled = true
# Distribution to use; the default distribution if not set.
# distro = "Debian"
# Folders that are searched for repositories ("~/" is the WSL home directory).
roots = ["~/projects"]
# Repositories and folders to skip (everything at or below these paths).
ignore = []
# git-track brings its own agent for WSL and installs it automatically (to
# ~/.local/share/git-track). To use your own build instead, set its path
# ("~/" is the WSL home directory). "~/.cargo/bin/git-track-agent", the default
# of older versions, is ignored; spell out the home directory for that one:
# agent = "/home/me/.cargo/bin/git-track-agent"

# Buttons shown next to each repository.
#   target:   "windows" or "wsl"
#   command:  program + arguments, started detached
# Placeholders: {path}   path of the repository (native to the target)
#               {distro} WSL distribution name
#               {unc}    WSL path as \\wsl.localhost\<distro>\... (for Windows programs)
# Console programs (cmd, wsl.exe, …) should be wrapped in wt.exe or `cmd /c start`.

[[actions]]
label = "Git Bash"
target = "windows"
command = ['C:\Program Files\Git\git-bash.exe', '--cd={path}']

[[actions]]
label = "Terminal"
target = "windows"
command = ["wt.exe", "-d", "{path}"]

[[actions]]
label = "Explorer"
target = "windows"
command = ["explorer.exe", "{path}"]

[[actions]]
label = "WSL Terminal"
target = "wsl"
command = ["wt.exe", "wsl.exe", "-d", "{distro}", "--cd", "{path}"]

[[actions]]
label = "Explorer"
target = "wsl"
command = ["explorer.exe", "{unc}"]
"#;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub refresh_interval_secs: u64,
    pub close_to_tray: bool,
    pub check_updates: bool,
    pub track: TrackOptions,
    pub scan: ScanOptions,
    pub jetbrains: JetBrainsConfig,
    pub windows: WindowsConfig,
    pub wsl: WslConfig,
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JetBrainsConfig {
    pub enabled: bool,
}

impl Default for JetBrainsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowsConfig {
    pub enabled: bool,
    pub roots: Vec<String>,
    pub ignore: Vec<String>,
    pub git: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WslConfig {
    pub enabled: bool,
    pub distro: Option<String>,
    pub roots: Vec<String>,
    pub ignore: Vec<String>,
    /// Own agent build; the bundled one is used if unset.
    pub agent: Option<String>,
    pub git: Option<String>,
}

/// Where `cargo install` puts the agent. Older config files contain this as
/// the (then default) value, which now means "bundled" as well.
pub const CARGO_AGENT: &str = "~/.cargo/bin/git-track-agent";

impl WslConfig {
    /// The agent configured explicitly, if any.
    pub fn custom_agent(&self) -> Option<&str> {
        self.agent
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty() && *a != CARGO_AGENT)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub label: String,
    pub target: SourceKind,
    pub command: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let action = |label: &str, target, command: &[&str]| Action {
            label: label.into(),
            target,
            command: command.iter().map(|s| s.to_string()).collect(),
        };
        Self {
            refresh_interval_secs: 600,
            close_to_tray: true,
            check_updates: true,
            track: TrackOptions::default(),
            scan: ScanOptions::default(),
            jetbrains: JetBrainsConfig::default(),
            windows: WindowsConfig::default(),
            wsl: WslConfig::default(),
            actions: vec![
                action(
                    "Git Bash",
                    SourceKind::Windows,
                    &[r"C:\Program Files\Git\git-bash.exe", "--cd={path}"],
                ),
                action("Terminal", SourceKind::Windows, &["wt.exe", "-d", "{path}"]),
                action("Explorer", SourceKind::Windows, &["explorer.exe", "{path}"]),
                action(
                    "WSL Terminal",
                    SourceKind::Wsl,
                    &["wt.exe", "wsl.exe", "-d", "{distro}", "--cd", "{path}"],
                ),
                action("Explorer", SourceKind::Wsl, &["explorer.exe", "{unc}"]),
            ],
        }
    }
}

impl Default for WindowsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            roots: Vec::new(),
            ignore: Vec::new(),
            git: None,
        }
    }
}

impl Default for WslConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            distro: None,
            roots: vec!["~/projects".into()],
            ignore: Vec::new(),
            agent: None,
            git: None,
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("git-track")
            .join("config.toml")
    }

    /// Loads the config file, creating it with defaults if it does not exist.
    pub fn load() -> Result<Config, String> {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                std::fs::write(&path, DEFAULT_CONFIG)
                    .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
                Ok(Config::default())
            }
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn enabled(&self, kind: SourceKind) -> bool {
        match kind {
            SourceKind::Windows => self.windows.enabled,
            SourceKind::Wsl => self.wsl.enabled,
        }
    }

    pub fn roots(&self, kind: SourceKind) -> &[String] {
        match kind {
            SourceKind::Windows => &self.windows.roots,
            SourceKind::Wsl => &self.wsl.roots,
        }
    }

    pub fn ignored(&self, kind: SourceKind) -> &[String] {
        match kind {
            SourceKind::Windows => &self.windows.ignore,
            SourceKind::Wsl => &self.wsl.ignore,
        }
    }

    pub fn ignored_mut(&mut self, kind: SourceKind) -> &mut Vec<String> {
        match kind {
            SourceKind::Windows => &mut self.windows.ignore,
            SourceKind::Wsl => &mut self.wsl.ignore,
        }
    }

    /// True if `path` (native to `kind`) is at or below an ignored path.
    pub fn is_ignored(&self, kind: SourceKind, path: &str) -> bool {
        let windows = kind == SourceKind::Windows;
        self.ignored(kind)
            .iter()
            .any(|prefix| path_within(path, &expand_home(kind, prefix), windows))
    }

    /// Writes the values the settings window manages into the config file,
    /// keeping comments and all other entries as they are.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.to_string(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let mut doc: DocumentMut = text
            .parse()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        self.write_to(&mut doc)?;
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, doc.to_string())
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    fn write_to(&self, doc: &mut DocumentMut) -> Result<(), String> {
        let root = doc.as_table_mut();
        set(
            root,
            "refresh_interval_secs",
            (self.refresh_interval_secs as i64).into(),
        );
        set(root, "close_to_tray", self.close_to_tray.into());
        set(root, "check_updates", self.check_updates.into());

        let track = table(root, "track")?;
        for name in TrackOptions::NAMES {
            let on = self.track.get(name).unwrap_or(true);
            set(track, name, on.into());
        }

        set(
            table(root, "jetbrains")?,
            "enabled",
            self.jetbrains.enabled.into(),
        );

        let windows = table(root, "windows")?;
        set(windows, "enabled", self.windows.enabled.into());
        set(windows, "roots", strings(&self.windows.roots));
        set(windows, "ignore", strings(&self.windows.ignore));

        let wsl = table(root, "wsl")?;
        set(wsl, "enabled", self.wsl.enabled.into());
        match &self.wsl.distro {
            Some(distro) => set(wsl, "distro", distro.as_str().into()),
            None => {
                wsl.remove("distro");
            }
        }
        set(wsl, "roots", strings(&self.wsl.roots));
        set(wsl, "ignore", strings(&self.wsl.ignore));
        Ok(())
    }

    /// Request for a full scan (`repos` empty) or a re-check of specific repositories.
    pub fn request(&self, kind: SourceKind, repos: Vec<String>) -> ScanRequest {
        let mut options = self.scan.clone();
        options.track = self.track.clone();
        options.idea = self.jetbrains.enabled;
        let git = match kind {
            SourceKind::Windows => &self.windows.git,
            SourceKind::Wsl => &self.wsl.git,
        };
        if git.is_some() {
            options.git = git.clone();
        }
        ScanRequest {
            roots: self.roots(kind).to_vec(),
            ignore: self.ignored(kind).to_vec(),
            // Re-checks skip discovery; the roots only attribute the repository.
            skip_discovery: !repos.is_empty(),
            repos,
            options,
        }
    }

    pub fn actions_for(&self, kind: SourceKind) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(move |a| a.target == kind)
    }
}

/// Replaces a value but keeps its formatting (e.g. a trailing comment).
fn set(table: &mut dyn TableLike, key: &str, value: Value) {
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            let mut decor = old.decor().clone();
            // Keep aligned trailing comments aligned: adjust the whitespace
            // between value and comment by the change in length.
            let len = |v: &Value| {
                let mut bare = v.clone();
                bare.decor_mut().clear();
                bare.to_string().chars().count()
            };
            let (old_len, new_len) = (len(old), len(&value));
            let suffix = decor.suffix().and_then(|s| s.as_str()).map(str::to_owned);
            if let Some(suffix) = suffix {
                let spaces = suffix.len() - suffix.trim_start_matches(' ').len();
                if spaces > 0 && suffix[spaces..].starts_with('#') {
                    let spaces = (spaces + old_len).saturating_sub(new_len).max(1);
                    decor.set_suffix(format!(
                        "{}{}",
                        " ".repeat(spaces),
                        suffix.trim_start_matches(' ')
                    ));
                }
            }
            *old = value;
            *old.decor_mut() = decor;
        }
        _ => {
            table.insert(key, Item::Value(value));
        }
    }
}

fn table<'a>(root: &'a mut toml_edit::Table, name: &str) -> Result<&'a mut dyn TableLike, String> {
    root.entry(name)
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .ok_or_else(|| format!("'{name}' in the config file is not a table"))
}

fn strings(items: &[String]) -> Value {
    Value::Array(items.iter().map(String::as_str).collect::<Array>())
}

/// Expands `~` in Windows paths. WSL paths stay as they are: only the agent
/// knows the WSL home directory (it expands them when scanning).
fn expand_home(kind: SourceKind, path: &str) -> String {
    match kind {
        SourceKind::Windows => git_track_core::discover::expand_tilde(path)
            .to_string_lossy()
            .into_owned(),
        SourceKind::Wsl => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_keeps_comments_and_round_trips() {
        let mut config = Config {
            refresh_interval_secs: 120,
            ..Default::default()
        };
        config.track.stash = false;
        config.windows.roots = vec![r"C:\Users\me\code".into()];
        config.windows.ignore = vec![r"C:\Users\me\code\old".into()];
        config.wsl.distro = Some("Debian".into());
        config.jetbrains.enabled = false;

        let mut doc: DocumentMut = DEFAULT_CONFIG.parse().unwrap();
        config.write_to(&mut doc).unwrap();
        let text = doc.to_string();

        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
        // Comments survive, Windows paths stay readable.
        assert!(
            text.contains("stash = false           # stash entries"),
            "{text}"
        );
        assert!(text.contains("# Buttons shown next to each repository."));
        assert!(text.contains(r"'C:\Users\me\code'"), "{text}");

        // Clearing the distro removes the key again.
        config.wsl.distro = None;
        config.write_to(&mut doc).unwrap();
        assert_eq!(toml::from_str::<Config>(&doc.to_string()).unwrap(), config);
    }

    #[test]
    fn ignored_paths() {
        let mut config = Config::default();
        config.windows.ignore = vec![r"D:\Code\old".into()];
        config.wsl.ignore = vec!["/home/me/projects/tmp".into()];
        assert!(config.is_ignored(SourceKind::Windows, r"d:\code\old\repo"));
        assert!(!config.is_ignored(SourceKind::Windows, r"D:\Code\older"));
        assert!(config.is_ignored(SourceKind::Wsl, "/home/me/projects/tmp"));
        assert!(!config.is_ignored(SourceKind::Wsl, "/home/me/projects/tmp2"));
        let request = config.request(SourceKind::Wsl, Vec::new());
        assert_eq!(request.ignore, ["/home/me/projects/tmp"]);
    }

    #[test]
    fn default_template_matches_default() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn track_toggles() {
        let config: Config = toml::from_str("[track]\nstash = false\nuntracked = false\n").unwrap();
        assert!(!config.track.stash && !config.track.untracked && config.track.modified);
        let request = config.request(SourceKind::Wsl, Vec::new());
        assert!(!request.options.track.stash);

        // Typos are reported instead of silently ignored.
        let err = toml::from_str::<Config>("[track]\nmodifed = false\n").unwrap_err();
        assert!(err.to_string().contains("modified"), "{err}");
    }
}
