//! Recursive discovery of git working trees.
//!
//! Every directory containing a valid `.git` entry (directory *or* file, so
//! worktrees and submodules count too) is a repository. The walk continues
//! *into* repositories, because nested repositories that are neither
//! submodules nor tracked are common. `.gitignore` is deliberately not
//! honoured: nested repositories are usually ignored by their parent.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::protocol::ScanOptions;

/// Expands a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    let home = || {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    };
    if path == "~" {
        if let Some(h) = home() {
            return h;
        }
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(h) = home()
    {
        return h.join(rest);
    }
    PathBuf::from(path)
}

/// True if `path` is `prefix` or lies below it. Windows-style paths compare
/// case-insensitively and accept `/` as well as `\\`; otherwise exact.
pub fn path_within(path: &str, prefix: &str, windows: bool) -> bool {
    let normalize = |p: &str| {
        if windows {
            p.replace('/', "\\").to_lowercase()
        } else {
            p.to_string()
        }
    };
    let sep = if windows { '\\' } else { '/' };
    let path = normalize(path);
    let prefix = normalize(prefix);
    let prefix = prefix.trim_end_matches(sep);
    if prefix.is_empty() {
        return false;
    }
    let path = path.trim_end_matches(sep);
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(sep))
}

/// [`path_within`] for native paths of this platform, against any of `prefixes`.
pub fn is_ignored(path: &Path, prefixes: &[PathBuf]) -> bool {
    let path = path.to_string_lossy();
    prefixes
        .iter()
        .any(|p| path_within(&path, &p.to_string_lossy(), cfg!(windows)))
}

/// Finds all repositories below `root` (including `root` itself), skipping
/// everything at or below an `ignore`d path.
///
/// Must be called from within a rayon thread pool; the walk is parallel.
pub fn discover(
    root: &Path,
    options: &ScanOptions,
    ignore: &[PathBuf],
) -> std::io::Result<Vec<PathBuf>> {
    // Surface unreadable / missing roots as an error instead of an empty result.
    fs::read_dir(root)?;

    let excludes: Vec<String> = options
        .exclude_dirs
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
    let max_depth = options.max_depth.unwrap_or(usize::MAX);
    let found = Mutex::new(Vec::new());
    let walker = Walker {
        max_depth,
        excludes: &excludes,
        ignore,
        found: &found,
    };
    walker.walk(root, 0);

    let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    found.sort();
    found.dedup();
    Ok(found)
}

/// Whether `dir` is a repository, judged from its `.git` entry without
/// running git (git would search the parent directories when it is broken):
///
/// * a `.git` directory needs a `HEAD` file and an object store (`objects/`,
///   or a `commondir` file in linked worktrees),
/// * a `.git` file must point (`gitdir: …`) to such a directory.
///
/// `Ok(false)`: definitely no repository (`.git` missing, empty or broken).
/// `Err`: cannot tell (permissions, unreachable drive, …).
pub fn repo_marker(dir: &Path) -> io::Result<bool> {
    let dot_git = dir.join(".git");
    let meta = match fs::metadata(&dot_git) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        return git_dir_valid(&dot_git);
    }
    let text = match fs::read_to_string(&dot_git) {
        Ok(text) => text,
        // Not UTF-8: certainly not a "gitdir:" file.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => return Ok(false),
        Err(e) => return Err(e),
    };
    let Some(target) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(false);
    };
    git_dir_valid(&dir.join(target))
}

fn git_dir_valid(git_dir: &Path) -> io::Result<bool> {
    match fs::metadata(git_dir.join("HEAD")) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    }
    Ok(git_dir.join("objects").is_dir() || git_dir.join("commondir").is_file())
}

/// True if `repo` is definitely no repository (anymore): its `.git` (or the
/// whole directory) was deleted or is broken. Errors (permissions,
/// unreachable drive, …) do not count.
pub fn is_gone(repo: &Path) -> bool {
    matches!(repo_marker(repo), Ok(false))
}

/// Finds the nearest directory containing a `.idea` folder, starting at
/// `repo` and going up to `root` (inclusive). Without a root only `repo`
/// itself is checked.
pub fn find_idea(repo: &Path, root: Option<&Path>) -> Option<PathBuf> {
    for dir in repo.ancestors() {
        if dir.join(".idea").is_dir() {
            return Some(dir.to_path_buf());
        }
        match root {
            Some(root) if dir.starts_with(root) && dir != root => continue,
            _ => return None,
        }
    }
    None
}

struct Walker<'a> {
    max_depth: usize,
    excludes: &'a [String],
    ignore: &'a [PathBuf],
    found: &'a Mutex<Vec<PathBuf>>,
}

impl Walker<'_> {
    fn walk(&self, dir: &Path, depth: usize) {
        if is_ignored(dir, self.ignore) {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name == ".git" {
                // Broken markers (e.g. an empty `.git` folder) are skipped; when
                // undecidable, let git report the problem.
                if !matches!(repo_marker(dir), Ok(false)) {
                    self.found
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(dir.to_path_buf());
                }
                continue;
            }
            // `file_type` does not follow symlinks (and treats junctions as
            // symlinks on Windows), so link cycles cannot occur.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let lower = name.to_string_lossy().to_lowercase();
            if self.excludes.contains(&lower) {
                continue;
            }
            subdirs.push(entry.path());
        }

        if depth < self.max_depth {
            subdirs.par_iter().for_each(|sub| self.walk(sub, depth + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `.git` directory that passes [`repo_marker`].
    fn fake_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".git/objects")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn finds_nested_repos_and_skips_excludes() {
        let tmp = std::env::temp_dir().join(format!("git-track-discover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        for d in [
            "a",
            "a/sub/nested",
            "b/inner",
            "c/node_modules/pkg",
            "deep/1/2/3",
        ] {
            fake_repo(&tmp.join(d));
        }
        // worktree style `.git` file
        let worktree_git = tmp.join("a/.git/worktrees/wt");
        fs::create_dir_all(&worktree_git).unwrap();
        fs::write(worktree_git.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        fs::write(worktree_git.join("commondir"), "../..\n").unwrap();
        fs::create_dir_all(tmp.join("wt")).unwrap();
        fs::write(tmp.join("wt/.git"), "gitdir: ../a/.git/worktrees/wt\n").unwrap();
        // broken markers are no repositories
        fs::create_dir_all(tmp.join("empty/.git")).unwrap();
        fs::create_dir_all(tmp.join("dangling")).unwrap();
        fs::write(tmp.join("dangling/.git"), "gitdir: ../nowhere\n").unwrap();

        let mut opts = ScanOptions::default();
        let found = discover(&tmp, &opts, &[]).unwrap();
        let rel: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&tmp)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rel, ["a", "a/sub/nested", "b/inner", "deep/1/2/3", "wt"]);

        // Ignoring a repository skips it and everything below it.
        let found = discover(&tmp, &opts, &[tmp.join("a"), tmp.join("deep/1")]).unwrap();
        assert!(found.iter().all(|p| !p.starts_with(tmp.join("a"))));
        assert!(found.iter().all(|p| !p.starts_with(tmp.join("deep"))));
        assert!(found.contains(&tmp.join("b/inner")));

        opts.max_depth = Some(2);
        let found = discover(&tmp, &opts, &[]).unwrap();
        assert!(!found.iter().any(|p| p.ends_with("3")));

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn path_within_matches_whole_components() {
        assert!(path_within("/home/me/p/repo", "/home/me/p", false));
        assert!(path_within("/home/me/p", "/home/me/p/", false));
        assert!(!path_within("/home/me/projects", "/home/me/p", false));
        assert!(!path_within("/home/me/P/repo", "/home/me/p", false));
        assert!(path_within(r"D:\Code\Repo", r"d:/code", true));
        assert!(path_within(r"D:\Code", r"D:\Code\", true));
        assert!(!path_within(r"D:\Codex", r"D:\Code", true));
        assert!(!path_within("/anything", "", false));
    }

    #[test]
    fn deleted_repos_are_gone() {
        let tmp = std::env::temp_dir().join(format!("git-track-gone-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fake_repo(&tmp.join("repo"));
        fs::create_dir_all(tmp.join("plain")).unwrap();
        fs::create_dir_all(tmp.join("worktree")).unwrap();
        fs::write(tmp.join("worktree/.git"), "gitdir: ../repo/.git\n").unwrap();
        fs::create_dir_all(tmp.join("empty/.git")).unwrap();
        fs::create_dir_all(tmp.join("garbage")).unwrap();
        fs::write(tmp.join("garbage/.git"), "something else\n").unwrap();

        assert!(!is_gone(&tmp.join("repo")));
        assert!(!is_gone(&tmp.join("worktree")));
        assert!(is_gone(&tmp.join("plain"))); // only .git deleted
        assert!(is_gone(&tmp.join("missing"))); // whole directory deleted
        assert!(is_gone(&tmp.join("empty"))); // empty .git folder
        assert!(is_gone(&tmp.join("garbage"))); // .git file without gitdir

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn finds_idea_in_repo_or_parent_up_to_root() {
        let tmp = std::env::temp_dir().join(format!("git-track-idea-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("root");
        for d in [
            ".idea",              // above the root: must be ignored
            "root/proj/.idea",    // project with nested repos
            "root/proj/ext/repo", // repo inside it
            "root/own/.idea",     // repo with its own project
            "root/plain",         // no project at all
        ] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }

        let find = |rel: &str, root: Option<&Path>| find_idea(&tmp.join(rel), root);
        assert_eq!(
            find("root/proj/ext/repo", Some(&root)),
            Some(tmp.join("root/proj"))
        );
        assert_eq!(find("root/own", Some(&root)), Some(tmp.join("root/own")));
        assert_eq!(find("root/plain", Some(&root)), None);
        // Without a root only the repository itself counts.
        assert_eq!(find("root/proj/ext/repo", None), None);
        assert_eq!(find("root/own", None), Some(tmp.join("root/own")));

        fs::remove_dir_all(&tmp).unwrap();
    }
}
