//! Determines whether a repository contains work that is not pushed.
//!
//! Uses the `git` executable rather than a library so that the user's git
//! configuration (safe.directory, fsmonitor, includeIf, …) is respected and
//! the results match what `git status` shows on the command line.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::protocol::{Issue, RepoStatus, ScanOptions};

pub struct Git {
    bin: String,
}

impl Git {
    pub fn new(options: &ScanOptions) -> Self {
        Self {
            bin: options.git.clone().unwrap_or_else(|| "git".into()),
        }
    }

    fn run(&self, repo: &Path, args: &[&str]) -> Result<String, String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-C")
            .arg(repo)
            .args(args)
            // Never take the index lock in the background: the user may be
            // committing in that very repository right now.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let out = cmd
            .output()
            .map_err(|e| format!("failed to run '{}': {e}", self.bin))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.trim();
            return Err(if msg.is_empty() {
                format!("git {} failed ({})", args.join(" "), out.status)
            } else {
                msg.lines().take(3).collect::<Vec<_>>().join(" / ")
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Number of commits reachable from `rev` that are not on any remote-tracking branch.
    fn count_not_on_remote(&self, repo: &Path, rev: &str) -> Result<u32, String> {
        let out = self.run(repo, &["rev-list", "--count", rev, "--not", "--remotes"])?;
        Ok(out.trim().parse().unwrap_or(0))
    }
}

pub fn check_repo(
    git: &Git,
    path: &Path,
    root: Option<&Path>,
    options: &ScanOptions,
) -> RepoStatus {
    let mut status = RepoStatus {
        path: path.to_string_lossy().into_owned(),
        root: root.map(|r| r.to_string_lossy().into_owned()),
        ..Default::default()
    };
    if let Err(e) = fill_status(git, path, options, &mut status) {
        status.error = Some(e);
    }
    status
}

fn fill_status(
    git: &Git,
    path: &Path,
    options: &ScanOptions,
    status: &mut RepoStatus,
) -> Result<(), String> {
    let track = &options.track;

    // --- working tree -----------------------------------------------------
    let porcelain = git.run(
        path,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            // Searching for untracked files is the expensive part of `git status`.
            if track.untracked {
                "--untracked-files=normal"
            } else {
                "--untracked-files=no"
            },
            // Changes *inside* a submodule are reported for the submodule
            // itself (it is discovered as a repository of its own).
            "--ignore-submodules=dirty",
        ],
    )?;
    let parsed = parse_status(&porcelain);
    status.branch = parsed.branch.clone();
    status.head = parsed.oid.as_ref().map(|o| o.chars().take(8).collect());
    let enabled = |on: bool, n: u32| if on { n } else { 0 };
    let staged = enabled(track.staged, parsed.staged);
    let unstaged = enabled(track.modified, parsed.unstaged);
    let untracked = enabled(track.untracked, parsed.untracked);
    let conflicted = enabled(track.conflicted, parsed.conflicted);
    if staged + unstaged + untracked + conflicted > 0 {
        status.issues.push(Issue::Uncommitted {
            staged,
            unstaged,
            untracked,
            conflicted,
        });
    }

    if track.in_progress
        && let Some(op) = in_progress_operation(path)
    {
        status.issues.push(Issue::InProgress {
            operation: op.into(),
        });
    }

    // --- refs ---------------------------------------------------------------
    let refs_out = git.run(
        path,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(upstream:short)%00%(upstream:track)",
            "refs/heads",
            "refs/stash",
        ],
    )?;
    let mut branches = Vec::new();
    let mut has_stash = false;
    for line in refs_out.lines() {
        let mut fields = line.split('\0');
        let refname = fields.next().unwrap_or_default();
        let upstream = fields.next().unwrap_or_default();
        let track = fields.next().unwrap_or_default();
        if refname == "refs/stash" {
            has_stash = true;
        } else if let Some(name) = refname.strip_prefix("refs/heads/") {
            branches.push(BranchRef {
                name: name.to_string(),
                upstream: upstream.to_string(),
                track: parse_track(track),
            });
        }
    }

    if has_stash && track.stash {
        let list = git.run(path, &["stash", "list", "--format=%H"])?;
        let count = list.lines().filter(|l| !l.is_empty()).count() as u32;
        if count > 0 {
            status.issues.push(Issue::Stash { count });
        }
    }

    // A repository without commits has nothing to push.
    if branches.is_empty() && parsed.oid.is_none() {
        return Ok(());
    }

    let remotes = git.run(path, &["remote"])?;
    if remotes.trim().is_empty() {
        // Without remotes every commit would count as unpushed, so the
        // branch checks below are meaningless; this is the only issue.
        if track.no_remote && !branches.is_empty() {
            status.issues.push(Issue::NoRemote {
                branches: branches.len() as u32,
            });
        }
        return Ok(());
    }

    // --- branches -------------------------------------------------------------
    let important: BTreeSet<&str> = options
        .important_branches
        .iter()
        .map(String::as_str)
        .collect();
    for branch in &branches {
        let relevant = options.check_all_branches
            || important.contains(branch.name.as_str())
            || parsed.branch.as_deref() == Some(branch.name.as_str());
        if !relevant {
            continue;
        }
        let full_ref = format!("refs/heads/{}", branch.name);
        if branch.upstream.is_empty() {
            if !track.no_upstream {
                continue;
            }
            let n = git.count_not_on_remote(path, &full_ref)?;
            if n > 0 {
                status.issues.push(Issue::NoUpstream {
                    branch: branch.name.clone(),
                    not_on_remote: n,
                });
            }
        } else if branch.track.gone {
            if !track.upstream_gone {
                continue;
            }
            let n = git.count_not_on_remote(path, &full_ref)?;
            if n > 0 {
                status.issues.push(Issue::UpstreamGone {
                    branch: branch.name.clone(),
                    upstream: branch.upstream.clone(),
                    not_on_remote: n,
                });
            }
        } else if branch.track.ahead > 0 {
            if !track.unpushed && !track.ahead_elsewhere {
                continue;
            }
            let n = git.count_not_on_remote(path, &full_ref)?;
            if (n > 0 && track.unpushed) || (n == 0 && track.ahead_elsewhere) {
                status.issues.push(Issue::Ahead {
                    branch: branch.name.clone(),
                    upstream: branch.upstream.clone(),
                    ahead: branch.track.ahead,
                    not_on_remote: n,
                });
            }
        }
    }

    if track.detached_head && parsed.branch.is_none() && parsed.oid.is_some() {
        let n = git.count_not_on_remote(path, "HEAD")?;
        if n > 0 {
            status.issues.push(Issue::DetachedHead { not_on_remote: n });
        }
    }

    Ok(())
}

struct BranchRef {
    name: String,
    upstream: String,
    track: Track,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Track {
    ahead: u32,
    gone: bool,
}

/// Parses `%(upstream:track)`: `""`, `"[gone]"`, `"[ahead 1]"`, `"[ahead 1, behind 2]"`.
fn parse_track(s: &str) -> Track {
    let mut t = Track::default();
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    for part in inner.split(',') {
        let mut words = part.split_whitespace();
        match (words.next(), words.next()) {
            (Some("gone"), _) => t.gone = true,
            (Some("ahead"), Some(n)) => t.ahead = n.parse().unwrap_or(0),
            _ => {}
        }
    }
    t
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedStatus {
    /// `None` when detached or unknown.
    branch: Option<String>,
    /// `None` for an unborn branch (no commits yet).
    oid: Option<String>,
    staged: u32,
    unstaged: u32,
    untracked: u32,
    conflicted: u32,
}

/// Parses `git status --porcelain=v2 --branch -z`.
fn parse_status(out: &str) -> ParsedStatus {
    let mut p = ParsedStatus::default();
    let mut fields = out.split('\0');
    while let Some(entry) = fields.next() {
        if let Some(header) = entry.strip_prefix("# ") {
            if let Some(oid) = header.strip_prefix("branch.oid ") {
                if oid != "(initial)" {
                    p.oid = Some(oid.to_string());
                }
            } else if let Some(head) = header.strip_prefix("branch.head ")
                && head != "(detached)"
            {
                p.branch = Some(head.to_string());
            }
            continue;
        }
        match entry.chars().next() {
            Some(kind @ ('1' | '2')) => {
                let xy: Vec<char> = entry.chars().skip(2).take(2).collect();
                if xy.first().is_some_and(|c| *c != '.') {
                    p.staged += 1;
                }
                if xy.get(1).is_some_and(|c| *c != '.') {
                    p.unstaged += 1;
                }
                if kind == '2' {
                    // Renames/copies carry the original path as an extra field.
                    fields.next();
                }
            }
            Some('u') => p.conflicted += 1,
            Some('?') => p.untracked += 1,
            _ => {}
        }
    }
    p
}

/// Locates the git directory of a working tree (handles `.git` files).
fn git_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let content = std::fs::read_to_string(&dot_git).ok()?;
    let target = content
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))?
        .trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        worktree.join(target)
    })
}

fn in_progress_operation(worktree: &Path) -> Option<&'static str> {
    let dir = git_dir(worktree)?;
    [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase/am"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ]
    .into_iter()
    .find(|(file, _)| dir.join(file).exists())
    .map(|(_, op)| op)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_parsing() {
        assert_eq!(parse_track(""), Track::default());
        assert_eq!(
            parse_track("[gone]"),
            Track {
                gone: true,
                ..Default::default()
            }
        );
        assert_eq!(
            parse_track("[ahead 3]"),
            Track {
                ahead: 3,
                ..Default::default()
            }
        );
        assert_eq!(parse_track("[behind 12]"), Track::default());
        assert_eq!(
            parse_track("[ahead 3, behind 12]"),
            Track {
                ahead: 3,
                gone: false
            }
        );
    }

    #[test]
    fn status_parsing() {
        let out = "# branch.oid 0123456789abcdef\0# branch.head feature/x\0# branch.upstream origin/feature/x\0# branch.ab +1 -0\0\
1 M. N... 100644 100644 100644 aaa bbb staged.txt\0\
1 .M N... 100644 100644 100644 aaa bbb modified.txt\0\
1 MM N... 100644 100644 100644 aaa bbb both.txt\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.txt\0old.txt\0\
u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt\0\
? untracked.txt\0? dir/\0";
        let p = parse_status(out);
        assert_eq!(p.branch.as_deref(), Some("feature/x"));
        assert_eq!(p.oid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(p.staged, 3);
        assert_eq!(p.unstaged, 2);
        assert_eq!(p.conflicted, 1);
        assert_eq!(p.untracked, 2);

        let p = parse_status("# branch.oid (initial)\0# branch.head main\0");
        assert_eq!(p.oid, None);
        assert_eq!(p.branch.as_deref(), Some("main"));

        let p = parse_status("# branch.oid abc\0# branch.head (detached)\0");
        assert_eq!(p.branch, None);
    }
}
