//! Data types shared between the GUI and the agent.
//!
//! The agent streams [`Event`]s as newline-delimited JSON on stdout; the GUI
//! sends a single [`ScanRequest`] as JSON on the agent's stdin.

use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 3;

pub fn default_exclude_dirs() -> Vec<String> {
    [
        "node_modules",
        "__pycache__",
        ".venv",
        ".venv-linux",
        ".pnpm-store",
        ".yarn",
        ".cache",
        ".gradle",
        ".idea",
        ".vs",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub fn default_important_branches() -> Vec<String> {
    vec!["main".into(), "master".into()]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanOptions {
    /// Directory names that are never descended into (matched against the
    /// last path component, case-insensitive). `.git` is always skipped.
    pub exclude_dirs: Vec<String>,
    /// Maximum directory depth below a root. `None` = unlimited.
    pub max_depth: Option<usize>,
    /// Branches checked for unpushed commits in addition to the checked-out one.
    pub important_branches: Vec<String>,
    /// Check every local branch, not only the important + checked-out ones.
    pub check_all_branches: bool,
    /// Path to the git executable (default: `git` from PATH).
    pub git: Option<String>,
    /// Number of repositories checked in parallel (default: derived from CPU count).
    pub threads: Option<usize>,
    /// Which kinds of issues are reported.
    pub track: TrackOptions,
    /// Look for a JetBrains project (`.idea`) in the repository or a parent
    /// directory up to the root.
    pub idea: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            exclude_dirs: default_exclude_dirs(),
            max_depth: Some(12),
            important_branches: default_important_branches(),
            check_all_branches: false,
            git: None,
            threads: None,
            track: TrackOptions::default(),
            idea: true,
        }
    }
}

/// Toggles for every kind of issue. Everything is reported by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrackOptions {
    /// Changes added to the index.
    pub staged: bool,
    /// Modified (or deleted) tracked files that are not staged.
    pub modified: bool,
    /// Untracked files (disabling this also makes scans faster).
    pub untracked: bool,
    /// Merge conflicts.
    pub conflicted: bool,
    /// Branch ahead of its upstream with commits that are on no remote branch.
    pub unpushed: bool,
    /// Branch ahead of its upstream, but the commits exist on another remote branch.
    pub ahead_elsewhere: bool,
    /// Branch without upstream that has commits on no remote branch.
    pub no_upstream: bool,
    /// Upstream deleted on the remote, local commits on no remote branch.
    pub upstream_gone: bool,
    /// Detached HEAD with commits on no remote branch.
    pub detached_head: bool,
    /// Repository with commits but no remote.
    pub no_remote: bool,
    /// Stash entries.
    pub stash: bool,
    /// Unfinished merge / rebase / cherry-pick / revert / bisect.
    pub in_progress: bool,
}

impl Default for TrackOptions {
    fn default() -> Self {
        Self {
            staged: true,
            modified: true,
            untracked: true,
            conflicted: true,
            unpushed: true,
            ahead_elsewhere: true,
            no_upstream: true,
            upstream_gone: true,
            detached_head: true,
            no_remote: true,
            stash: true,
            in_progress: true,
        }
    }
}

impl TrackOptions {
    pub const NAMES: [&str; 12] = [
        "staged",
        "modified",
        "untracked",
        "conflicted",
        "unpushed",
        "ahead_elsewhere",
        "no_upstream",
        "upstream_gone",
        "detached_head",
        "no_remote",
        "stash",
        "in_progress",
    ];

    /// Sets a toggle by its name (as used in the config file).
    pub fn set(&mut self, name: &str, value: bool) -> Result<(), String> {
        let field = match name {
            "staged" => &mut self.staged,
            "modified" => &mut self.modified,
            "untracked" => &mut self.untracked,
            "conflicted" => &mut self.conflicted,
            "unpushed" => &mut self.unpushed,
            "ahead_elsewhere" => &mut self.ahead_elsewhere,
            "no_upstream" => &mut self.no_upstream,
            "upstream_gone" => &mut self.upstream_gone,
            "detached_head" => &mut self.detached_head,
            "no_remote" => &mut self.no_remote,
            "stash" => &mut self.stash,
            "in_progress" => &mut self.in_progress,
            _ => {
                return Err(format!(
                    "unknown issue kind '{name}' (valid: {})",
                    Self::NAMES.join(", ")
                ));
            }
        };
        *field = value;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanRequest {
    /// Directories that are searched recursively for repositories.
    /// A leading `~` is expanded to the home directory.
    pub roots: Vec<String>,
    /// Repositories that are checked directly, without discovery.
    pub repos: Vec<String>,
    /// Only check `repos`; `roots` then just tell which root a repository
    /// belongs to (used for re-checks).
    pub skip_discovery: bool,
    pub options: ScanOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Hello {
        version: String,
        protocol: u32,
        /// WSL distribution name, if running inside WSL.
        #[serde(default)]
        distro: Option<String>,
    },
    /// Discovery finished; `count` repositories will be checked.
    Discovered {
        count: usize,
    },
    Repo(RepoStatus),
    RootError {
        root: String,
        message: String,
    },
    Done {
        elapsed_ms: u64,
    },
    Fatal {
        message: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoStatus {
    /// Absolute, native path of the working tree.
    pub path: String,
    /// The root this repository was discovered under (if any).
    #[serde(default)]
    pub root: Option<String>,
    /// Checked-out branch, `None` when HEAD is detached or unborn.
    #[serde(default)]
    pub branch: Option<String>,
    /// Abbreviated commit id of HEAD.
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub issues: Vec<Issue>,
    /// Set when git could not be run or failed for this repository.
    #[serde(default)]
    pub error: Option<String>,
    /// Directory containing the JetBrains `.idea` folder this repository
    /// belongs to (the repository itself or a parent up to the root).
    #[serde(default)]
    pub idea: Option<String>,
}

impl RepoStatus {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty() && self.error.is_none()
    }

    /// Path relative to the root it was found under (or the full path).
    pub fn display_path(&self) -> &str {
        if let Some(root) = &self.root
            && let Some(rest) = self.path.strip_prefix(root.as_str())
        {
            let rest = rest.trim_start_matches(['/', '\\']);
            if !rest.is_empty() {
                return rest;
            }
        }
        &self.path
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Something exists only locally.
    Danger,
    /// Worth a look, but nothing is at risk of being lost.
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Issue {
    /// Working tree or index has changes.
    Uncommitted {
        staged: u32,
        unstaged: u32,
        untracked: u32,
        conflicted: u32,
    },
    /// Branch is ahead of its upstream.
    Ahead {
        branch: String,
        upstream: String,
        ahead: u32,
        /// Of those, commits not contained in *any* remote-tracking branch.
        not_on_remote: u32,
    },
    /// Branch has no upstream and contains commits not on any remote.
    NoUpstream { branch: String, not_on_remote: u32 },
    /// Upstream branch was deleted on the remote and local commits are not on any remote.
    UpstreamGone {
        branch: String,
        upstream: String,
        not_on_remote: u32,
    },
    /// Detached HEAD with commits not on any remote.
    DetachedHead { not_on_remote: u32 },
    /// Repository has commits but no remote at all.
    NoRemote { branches: u32 },
    /// Stash entries exist (stashes are never pushed).
    Stash { count: u32 },
    /// A merge / rebase / cherry-pick / revert / bisect is in progress.
    InProgress { operation: String },
}

impl Issue {
    pub fn severity(&self) -> Severity {
        match self {
            Issue::Ahead {
                not_on_remote: 0, ..
            } => Severity::Warning,
            Issue::InProgress { .. } => Severity::Warning,
            _ => Severity::Danger,
        }
    }

    /// Short label, e.g. for a chip in the UI.
    pub fn label(&self) -> String {
        match self {
            Issue::Uncommitted {
                staged,
                unstaged,
                untracked,
                conflicted,
            } => {
                let mut parts = Vec::new();
                if *conflicted > 0 {
                    parts.push(format!("{conflicted} conflicted"));
                }
                if *staged > 0 {
                    parts.push(format!("{staged} staged"));
                }
                if *unstaged > 0 {
                    parts.push(format!("{unstaged} modified"));
                }
                if *untracked > 0 {
                    parts.push(format!("{untracked} untracked"));
                }
                parts.join(", ")
            }
            Issue::Ahead {
                branch,
                ahead,
                not_on_remote,
                ..
            } => {
                if *not_on_remote > 0 {
                    format!("{branch}: {ahead} unpushed")
                } else {
                    format!("{branch}: {ahead} ahead (on other remote branch)")
                }
            }
            Issue::NoUpstream {
                branch,
                not_on_remote,
            } => format!("{branch}: no upstream, {not_on_remote} unpushed"),
            Issue::UpstreamGone {
                branch,
                not_on_remote,
                ..
            } => format!("{branch}: upstream gone, {not_on_remote} unpushed"),
            Issue::DetachedHead { not_on_remote } => {
                format!("detached HEAD: {not_on_remote} unpushed")
            }
            Issue::NoRemote { .. } => "no remote".into(),
            Issue::Stash { count } => {
                format!("{count} stash{}", if *count == 1 { "" } else { "es" })
            }
            Issue::InProgress { operation } => format!("{operation} in progress"),
        }
    }

    /// Longer explanation, e.g. for a tooltip or text output.
    pub fn description(&self) -> String {
        match self {
            Issue::Ahead {
                branch,
                upstream,
                ahead,
                not_on_remote,
            } => format!(
                "'{branch}' is {ahead} commit(s) ahead of '{upstream}'; {not_on_remote} of them are not on any remote branch"
            ),
            Issue::UpstreamGone {
                branch, upstream, ..
            } => format!("upstream '{upstream}' of '{branch}' no longer exists"),
            Issue::NoRemote { branches } => {
                format!("no remote configured; {branches} local branch(es) exist only here")
            }
            other => other.label(),
        }
    }
}
