//! Watches known repositories for changes between full scans.
//!
//! The [`fingerprint`] of a repository is taken from the directory listings
//! of its git directory and `refs/` (no git process, no file is opened). A
//! change means git did something (commit, checkout, push, fetch, stash, an
//! IDE refreshing the index). The repository is then checked right away and
//! stays "hot" for [`HOT_FOR`], during which it is checked every
//! [`HOT_EVERY`] to also catch edits of working-tree files (which leave
//! `.git` alone). Repositories with issues are checked every
//! [`DIRTY_EVERY`] regardless.
//!
//! Fingerprints of hot repositories and those with issues are compared every
//! [`POLL`], the (usually many) quiet clean ones every [`COLD_POLL`], and all
//! of them when the window is activated.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, UNIX_EPOCH};

use rayon::prelude::*;

use crate::check::{self, Git};
use crate::discover::{self, GitDirs};
use crate::protocol::{Event, RepoStatus, ScanOptions, WatchCommand};

/// How often the fingerprints of hot repositories and those with issues are compared.
pub const POLL: Duration = Duration::from_secs(3);
/// How often the fingerprints of all other repositories are compared.
pub const COLD_POLL: Duration = Duration::from_secs(30);
/// How long a repository stays hot after its last change.
pub const HOT_FOR: Duration = Duration::from_secs(5 * 60);
/// Check interval of hot repositories.
pub const HOT_EVERY: Duration = Duration::from_secs(8);
/// Check interval of repositories with issues.
pub const DIRTY_EVERY: Duration = Duration::from_secs(60);
/// On activation, hot and dirty repositories not checked this recently are checked.
const ACTIVATED_MIN_AGE: Duration = Duration::from_secs(5);
/// Parallel checks; a watcher should stay in the background.
const THREADS: usize = 4;

/// A hash over the directory listings (names, sizes, modification times) of
/// the git directory and of `refs/` (without tags). On Windows a listing
/// carries size and time of every entry, so no file is opened at all, which
/// keeps this cheap for hundreds of repositories. Git writes `HEAD`, the
/// index and refs through a lock file that is renamed into place, which also
/// changes the containing directory.
///
/// `Ok(None)`: the git directory is gone.
pub fn fingerprint(dirs: &GitDirs) -> io::Result<Option<u64>> {
    let mut hasher = DefaultHasher::new();
    match hash_listing(&dirs.git_dir, false, &mut hasher) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => result?,
    }
    if dirs.common_dir != dirs.git_dir {
        hash_listing(&dirs.common_dir, false, &mut hasher)?;
    }
    // Missing in reftable repositories, whose tables live in `reftable/`.
    for (dir, recurse) in [("refs", true), ("reftable", false)] {
        let _ = hash_listing(&dirs.common_dir.join(dir), recurse, &mut hasher);
    }
    Ok(Some(hasher.finish()))
}

fn hash_listing(dir: &Path, recurse: bool, hasher: &mut DefaultHasher) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            let time = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
            Some((entry.file_name(), meta.is_dir(), meta.len(), time))
        })
        .collect();
    entries.sort();
    for (name, is_dir, len, time) in &entries {
        name.hash(hasher);
        if *is_dir {
            // Only the name: the time of e.g. `objects/` changes with every
            // new object and with git's background maintenance.
            // Tags are irrelevant and can be many.
            if recurse && name != "tags" {
                let _ = hash_listing(&dir.join(name), true, hasher);
            }
        } else {
            len.hash(hasher);
            time.hash(hasher);
        }
    }
    Ok(())
}

/// Fingerprints `repo`, using (and refreshing) the cached location of its
/// git directory. Returns the new location if it had to be looked up.
fn probe(repo: &Path, cached: Option<&GitDirs>) -> (Option<GitDirs>, io::Result<Option<u64>>) {
    if let Some(dirs) = cached {
        match fingerprint(dirs) {
            // Gone, or moved (e.g. `.git` became a worktree file): look again.
            Ok(None) => {}
            print => return (None, print),
        }
    }
    match discover::git_dirs(repo) {
        Ok(Some(dirs)) => {
            let print = fingerprint(&dirs);
            (Some(dirs), print)
        }
        Ok(None) => (None, Ok(None)),
        Err(e) => (None, Err(e)),
    }
}

struct Watched {
    status: RepoStatus,
    /// Looked up on the first probe.
    dirs: Option<GitDirs>,
    /// `None` until the first probe after it was added.
    fingerprint: Option<u64>,
    probed: Option<Instant>,
    checked: Instant,
    hot_until: Option<Instant>,
}

impl Watched {
    fn hot(&self, now: Instant) -> bool {
        self.hot_until.is_some_and(|t| t > now)
    }

    fn dirty(&self) -> bool {
        !self.status.is_clean()
    }
}

/// What a command asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// Nothing; wait for the next regular pass.
    Later,
    /// A regular pass right away.
    Now,
    /// Right away, and also check hot and dirty repositories early.
    Forced,
}

/// The watcher state, driven by [`watch`] (separate for testing).
pub struct Watcher {
    options: ScanOptions,
    git: Git,
    repos: BTreeMap<String, Watched>,
}

impl Default for Watcher {
    fn default() -> Self {
        let options = ScanOptions::default();
        Self {
            git: Git::new(&options),
            options,
            repos: BTreeMap::new(),
        }
    }
}

impl Watcher {
    pub fn command(&mut self, command: WatchCommand, now: Instant) -> Pass {
        match command {
            WatchCommand::Set { options, repos } => {
                self.git = Git::new(&options);
                self.options = options;
                let mut old = std::mem::take(&mut self.repos);
                self.repos = repos
                    .into_iter()
                    .map(|status| {
                        let watched = match old.remove(&status.path) {
                            // Keep the known fingerprint: a change since then is real.
                            Some(previous) => Watched {
                                status,
                                checked: now,
                                ..previous
                            },
                            None => Watched {
                                status,
                                dirs: None,
                                fingerprint: None,
                                probed: None,
                                checked: now,
                                hot_until: None,
                            },
                        };
                        (watched.status.path.clone(), watched)
                    })
                    .collect();
                // Take the fingerprints of new repositories soon.
                Pass::Now
            }
            WatchCommand::Touch { paths } => {
                for path in paths {
                    if let Some(watched) = self.repos.get_mut(&path) {
                        watched.hot_until = Some(now + HOT_FOR);
                    }
                }
                Pass::Later
            }
            WatchCommand::Activated => Pass::Forced,
        }
    }

    /// Compares the fingerprints that are due (all of them when `forced`)
    /// and checks the repositories that need it. Call it inside a rayon pool.
    pub fn pass(&mut self, forced: bool, emit: &(dyn Fn(Event) + Sync)) {
        let now = Instant::now();
        let probes: Vec<_> = self
            .repos
            .par_iter()
            .filter(|(_, w)| {
                forced
                    || w.hot(now)
                    || w.dirty()
                    || w.probed
                        .is_none_or(|t| now.saturating_duration_since(t) >= COLD_POLL)
            })
            .map(|(path, w)| {
                let (dirs, print) = probe(Path::new(path), w.dirs.as_ref());
                (path.clone(), dirs, print)
            })
            .collect();

        let mut due = Vec::new();
        for (path, dirs, print) in probes {
            let Some(watched) = self.repos.get_mut(&path) else {
                continue;
            };
            watched.probed = Some(now);
            if dirs.is_some() {
                watched.dirs = dirs;
            }
            match print {
                // Cannot tell (unreachable drive, permissions): wait.
                Err(_) => {}
                Ok(None) => {
                    // A missing root (e.g. an unplugged drive) is left to the next scan.
                    let root_present = watched
                        .status
                        .root
                        .as_deref()
                        .is_none_or(|root| Path::new(root).is_dir());
                    if root_present {
                        self.repos.remove(&path);
                        emit(Event::Gone { path });
                    }
                }
                Ok(Some(print)) => {
                    let changed = watched.fingerprint.is_some_and(|old| old != print);
                    watched.fingerprint = Some(print);
                    if changed {
                        watched.hot_until = Some(now + HOT_FOR);
                    }
                    let age = now.saturating_duration_since(watched.checked);
                    let hot = watched.hot(now);
                    let dirty = watched.dirty();
                    if changed
                        || (hot && age >= HOT_EVERY)
                        || (dirty && age >= DIRTY_EVERY)
                        || (forced && (hot || dirty) && age >= ACTIVATED_MIN_AGE)
                    {
                        due.push(path);
                    }
                }
            }
        }
        if due.is_empty() {
            return;
        }

        let results: Vec<RepoStatus> = due
            .par_iter()
            .map(|path| {
                let root = self.repos[path].status.root.as_deref().map(Path::new);
                let status = check::check_repo(&self.git, Path::new(path), root, &self.options);
                emit(Event::Repo(status.clone()));
                status
            })
            .collect();

        let now = Instant::now();
        for status in results {
            if let Some(watched) = self.repos.get_mut(&status.path) {
                if watched.status != status {
                    watched.hot_until = Some(now + HOT_FOR);
                }
                watched.status = status;
                watched.checked = now;
            }
        }
    }
}

/// Runs a watcher until `commands` is closed. Blocking: call it from a
/// dedicated thread. Emits `Repo` and `Gone` events (from several threads).
pub fn watch(commands: Receiver<WatchCommand>, emit: &(dyn Fn(Event) + Sync)) {
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(THREADS)
        .thread_name(|i| format!("git-track-watch-{i}"))
        .build()
    {
        Ok(pool) => pool,
        Err(e) => {
            emit(Event::Fatal {
                message: format!("cannot create thread pool: {e}"),
            });
            return;
        }
    };
    let mut watcher = Watcher::default();
    let mut next = Instant::now() + POLL;
    loop {
        let now = Instant::now();
        let pass = match commands.recv_timeout(next.saturating_duration_since(now)) {
            Ok(command) => watcher.command(command, now),
            Err(RecvTimeoutError::Timeout) => Pass::Now,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if pass == Pass::Later {
            continue;
        }
        pool.install(|| watcher.pass(pass == Pass::Forced, emit));
        next = Instant::now() + POLL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Mutex;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", if cfg!(windows) { "NUL" } else { "/dev/null" })
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Repository with one commit on `main`.
    fn repo(name: &str) -> (PathBuf, PathBuf) {
        let tmp = std::env::temp_dir().join(format!("git-track-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        // No background gc / maintenance writing into .git during the test.
        git(&repo, &["config", "gc.auto", "0"]);
        git(&repo, &["config", "maintenance.auto", "false"]);
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-q", "-m", "one"]);
        (tmp, repo)
    }

    /// Some file systems have coarse timestamps.
    fn tick() {
        std::thread::sleep(Duration::from_millis(30));
    }

    #[test]
    fn fingerprint_follows_git_state_only() {
        let (tmp, repo) = repo("fingerprint");
        let dirs = |p: &Path| discover::git_dirs(p).unwrap().unwrap();
        let print = |p: &Path| fingerprint(&dirs(p)).unwrap().unwrap();

        let start = print(&repo);
        assert_eq!(print(&repo), start, "stable without changes");

        tick();
        fs::write(repo.join("a.txt"), "edited\n").unwrap();
        assert_eq!(print(&repo), start, "working-tree edits do not count");

        let mut last = start;
        let mut changes = |args: &[&str], what: &str| {
            tick();
            git(&repo, args);
            let now = print(&repo);
            assert_ne!(now, last, "{what} changes the fingerprint");
            last = now;
        };
        changes(&["commit", "-q", "-am", "two"], "a commit");
        changes(&["branch", "other"], "a new branch");
        changes(
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
            "a remote ref (push / fetch)",
        );
        fs::write(repo.join("a.txt"), "stash me\n").unwrap();
        changes(&["stash", "-q"], "a stash");

        // Linked worktree: its refs live in the main repository.
        git(&repo, &["worktree", "add", "-q", "../wt", "-b", "wt"]);
        let wt = tmp.join("wt");
        let wt_start = print(&wt);
        tick();
        git(&repo, &["update-ref", "refs/heads/wt", "HEAD~1"]);
        assert_ne!(print(&wt), wt_start, "worktree sees its branch move");

        let known = dirs(&repo);
        fs::remove_dir_all(repo.join(".git")).unwrap();
        assert_eq!(fingerprint(&known).unwrap(), None);
        assert_eq!(probe(&repo, Some(&known)).1.unwrap(), None);
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn watcher_checks_changed_repos_and_reports_gone_ones() {
        let (tmp, repo) = repo("watcher");
        let path = repo.to_string_lossy().into_owned();
        let mut options = ScanOptions::default();
        // Clean then (no remote is fine), so it is probed only every COLD_POLL.
        options.track.no_remote = false;
        let events = Mutex::new(Vec::new());
        let emit = |e: Event| events.lock().unwrap().push(e);
        let take = || std::mem::take(&mut *events.lock().unwrap());

        let mut watcher = Watcher::default();
        let status = crate::check_one(&repo, None, &options);
        assert!(status.is_clean(), "{status:?}");
        let pass = watcher.command(
            WatchCommand::Set {
                options: options.clone(),
                repos: vec![status.clone()],
            },
            Instant::now(),
        );
        assert_eq!(pass, Pass::Now);
        watcher.pass(false, &emit);
        assert!(take().is_empty(), "just checked, nothing changed");

        tick();
        fs::write(repo.join("b.txt"), "new\n").unwrap();
        git(&repo, &["add", "b.txt"]);
        watcher.pass(false, &emit);
        assert!(take().is_empty(), "quiet clean repository: not probed yet");
        // As on window activation.
        watcher.pass(true, &emit);
        let events = take();
        let [Event::Repo(changed)] = events.as_slice() else {
            panic!("expected one check, got {events:?}");
        };
        assert_ne!(changed, &status, "the staged file shows up");
        assert!(watcher.repos[&path].hot(Instant::now()));

        fs::remove_dir_all(repo.join(".git")).unwrap();
        watcher.pass(false, &emit);
        assert!(matches!(take().as_slice(), [Event::Gone { path: p }] if *p == path));
        assert!(watcher.repos.is_empty());
        fs::remove_dir_all(&tmp).unwrap();
    }
}
