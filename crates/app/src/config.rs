//! User configuration, stored as TOML in `%APPDATA%\git-track\config.toml`.
//!
//! The file is re-read on every full refresh, so edits apply without a restart.

use std::path::PathBuf;

use git_track_core::{ScanOptions, ScanRequest, TrackOptions};
use serde::{Deserialize, Serialize};

use crate::scanner::SourceKind;

/// Written on first start. Must deserialize to the same value as `Config::default()`.
pub const DEFAULT_CONFIG: &str = r#"# git-track configuration
# Changes are picked up on the next refresh (button or automatic).

# Automatic full rescan interval in seconds (0 = manual refresh only).
refresh_interval_secs = 600

# Closing the window only hides it to the system tray (quit via the tray menu,
# "Menu ▾" → Quit or Ctrl+Q). false = no tray icon, closing quits. Needs a restart.
close_to_tray = true

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
roots = ['D:\Sources\Lichtflut']
# git = 'C:\Program Files\Git\cmd\git.exe'

[wsl]
enabled = true
# Distribution to use; the default distribution if not set.
# distro = "Debian"
roots = ["~/projects"]
# git-track-agent inside WSL. A leading "~/" means the WSL home directory.
# Install it from a WSL shell with: cargo install --path crates/agent
agent = "~/.cargo/bin/git-track-agent"

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
    pub git: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WslConfig {
    pub enabled: bool,
    pub distro: Option<String>,
    pub roots: Vec<String>,
    pub agent: String,
    pub git: Option<String>,
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
            roots: vec![r"D:\Sources\Lichtflut".into()],
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
            agent: "~/.cargo/bin/git-track-agent".into(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
