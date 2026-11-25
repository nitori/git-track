//! Recursive discovery of git working trees.
//!
//! Every directory containing a `.git` entry (directory *or* file, so
//! worktrees and submodules count too) is a repository. The walk continues
//! *into* repositories, because nested repositories that are neither
//! submodules nor tracked are common. `.gitignore` is deliberately not
//! honoured: nested repositories are usually ignored by their parent.

use std::fs;
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

/// Finds all repositories below `root` (including `root` itself).
///
/// Must be called from within a rayon thread pool; the walk is parallel.
pub fn discover(root: &Path, options: &ScanOptions) -> std::io::Result<Vec<PathBuf>> {
    // Surface unreadable / missing roots as an error instead of an empty result.
    fs::read_dir(root)?;

    let excludes: Vec<String> = options
        .exclude_dirs
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
    let max_depth = options.max_depth.unwrap_or(usize::MAX);
    let found = Mutex::new(Vec::new());
    walk(root, 0, max_depth, &excludes, &found);

    let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    found.sort();
    found.dedup();
    Ok(found)
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

fn walk(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    excludes: &[String],
    found: &Mutex<Vec<PathBuf>>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            found
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(dir.to_path_buf());
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
        if excludes.contains(&lower) {
            continue;
        }
        subdirs.push(entry.path());
    }

    if depth < max_depth {
        subdirs
            .par_iter()
            .for_each(|sub| walk(sub, depth + 1, max_depth, excludes, found));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nested_repos_and_skips_excludes() {
        let tmp = std::env::temp_dir().join(format!("git-track-discover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        for d in [
            "a/.git",
            "a/sub/nested/.git",
            "b/inner/.git/objects",
            "c/node_modules/pkg/.git",
            "deep/1/2/3/.git",
        ] {
            fs::create_dir_all(tmp.join(d)).unwrap();
        }
        // worktree / submodule style `.git` file
        fs::create_dir_all(tmp.join("wt")).unwrap();
        fs::write(tmp.join("wt/.git"), "gitdir: ../a/.git/worktrees/wt\n").unwrap();

        let mut opts = ScanOptions::default();
        let found = discover(&tmp, &opts).unwrap();
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

        opts.max_depth = Some(2);
        let found = discover(&tmp, &opts).unwrap();
        assert!(!found.iter().any(|p| p.ends_with("3")));

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
