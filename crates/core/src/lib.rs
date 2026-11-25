//! Shared logic for git-track: repository discovery, unpushed-work detection
//! and the JSON protocol spoken between the GUI and the WSL agent.

pub mod check;
pub mod discover;
pub mod protocol;

use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

pub use protocol::*;

/// Runs a complete scan and reports progress through `emit`.
///
/// Blocking: call it from a dedicated thread. Discovery and checks run on a
/// private rayon pool, so `emit` is called from several threads concurrently.
/// Emits `Discovered`, then one `Repo` per repository (in completion order),
/// any `RootError`s, and finally `Done`. Does not emit `Hello`.
pub fn scan(request: &ScanRequest, emit: &(dyn Fn(Event) + Sync)) {
    let started = Instant::now();
    let options = &request.options;
    let threads = options.threads.unwrap_or_else(|| {
        // Checks mostly wait for git processes, so oversubscribe a little.
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        (cpus * 2).clamp(4, 32)
    });
    let pool = match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("git-track-scan-{i}"))
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

    pool.install(|| {
        // (repo, root it was found under)
        let mut targets: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        let discover_roots = if request.skip_discovery {
            &[][..]
        } else {
            &request.roots[..]
        };
        for root in discover_roots {
            let root_path = discover::expand_tilde(root);
            match discover::discover(&root_path, options) {
                Ok(repos) => {
                    targets.extend(repos.into_iter().map(|r| (r, Some(root_path.clone()))))
                }
                Err(e) => emit(Event::RootError {
                    root: root.clone(),
                    message: e.to_string(),
                }),
            }
        }
        for repo in &request.repos {
            let path = discover::expand_tilde(repo);
            if discover::is_gone(&path) {
                emit(Event::Gone {
                    path: path.to_string_lossy().into_owned(),
                });
                continue;
            }
            let root = request
                .roots
                .iter()
                .map(|r| discover::expand_tilde(r))
                .find(|r| path.starts_with(r));
            targets.push((path, root));
        }
        // Overlapping roots must not produce duplicates.
        targets.sort_by(|a, b| a.0.cmp(&b.0));
        targets.dedup_by(|a, b| a.0 == b.0);

        emit(Event::Discovered {
            count: targets.len(),
        });

        let git = check::Git::new(options);
        targets.par_iter().for_each(|(repo, root)| {
            emit(Event::Repo(check::check_repo(
                &git,
                repo,
                root.as_deref(),
                options,
            )));
        });
    });

    emit(Event::Done {
        elapsed_ms: started.elapsed().as_millis() as u64,
    });
}

/// Checks a single repository synchronously.
pub fn check_one(path: &Path, root: Option<&Path>, options: &ScanOptions) -> RepoStatus {
    check::check_repo(&check::Git::new(options), path, root, options)
}
