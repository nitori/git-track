//! Headless scanner. The GUI starts it inside WSL (`wsl.exe -e git-track-agent
//! scan --stdin`), writes a JSON `ScanRequest` to stdin and reads NDJSON
//! events from stdout. It is also usable on its own from a terminal.

use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::sync::Mutex;

use git_track_core::{Event, PROTOCOL_VERSION, ScanRequest, Severity, scan};

const USAGE: &str = "\
git-track-agent — find git repositories with unpushed work

USAGE:
    git-track-agent scan [OPTIONS] [ROOT]...
    git-track-agent scan --stdin          (read a JSON ScanRequest, used by the GUI)
    git-track-agent check [OPTIONS] REPO...

OPTIONS:
    --json               Emit NDJSON events instead of text
    --all                Text output: also list clean repositories
    --all-branches       Check every local branch, not just main/master + current
    --important A,B      Branches checked besides the current one (default: main,master)
    --skip A,B           Issue kinds to ignore: staged, modified, untracked, conflicted,
                         unpushed, ahead_elsewhere, no_upstream, upstream_gone,
                         detached_head, no_remote, stash, in_progress
    --exclude NAME       Additional directory name to skip (repeatable)
    --max-depth N        Maximum directory depth below a root (default: 12)
    --threads N          Parallel checks
    --git PATH           git executable
    -V, --version        Print version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(code) => code,
        Err(msg) => {
            eprintln!("error: {msg}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Vec<String>) -> Result<ExitCode, String> {
    let mut it = args.into_iter();
    let command = match it.next().as_deref() {
        Some("scan") => Command::Scan,
        Some("check") => Command::Check,
        Some("-V" | "--version") => {
            println!(
                "git-track-agent {} (protocol {PROTOCOL_VERSION})",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(ExitCode::SUCCESS);
        }
        Some("-h" | "--help") | None => {
            print!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        Some(other) => return Err(format!("unknown command '{other}'")),
    };

    let mut request = ScanRequest::default();
    let mut json = false;
    let mut stdin = false;
    let mut show_clean = false;
    let mut positional = Vec::new();

    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--json" => json = true,
            "--stdin" => stdin = true,
            "--all" => show_clean = true,
            "--all-branches" => request.options.check_all_branches = true,
            "--important" => {
                request.options.important_branches = value("--important")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect()
            }
            "--skip" => {
                for name in value("--skip")?
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    request.options.track.set(name, false)?;
                }
            }
            "--exclude" => request.options.exclude_dirs.push(value("--exclude")?),
            "--max-depth" => {
                request.options.max_depth = Some(
                    value("--max-depth")?
                        .parse()
                        .map_err(|_| "invalid --max-depth")?,
                )
            }
            "--threads" => {
                request.options.threads = Some(
                    value("--threads")?
                        .parse()
                        .map_err(|_| "invalid --threads")?,
                )
            }
            "--git" => request.options.git = Some(value("--git")?),
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(ExitCode::SUCCESS);
            }
            s if s.starts_with('-') => return Err(format!("unknown option '{s}'")),
            _ => positional.push(arg),
        }
    }

    if stdin {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("reading stdin: {e}"))?;
        request = serde_json::from_str(&buf).map_err(|e| format!("invalid request JSON: {e}"))?;
        json = true;
    } else {
        match command {
            Command::Scan if positional.is_empty() => positional.push(".".into()),
            Command::Check if positional.is_empty() => {
                return Err("check needs at least one REPO".into());
            }
            _ => {}
        }
        let target = match command {
            Command::Scan => &mut request.roots,
            Command::Check => &mut request.repos,
        };
        *target = positional.into_iter().map(|p| absolutize(&p)).collect();
    }

    if json {
        run_json(&request);
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(run_text(&request, show_clean))
    }
}

enum Command {
    Scan,
    Check,
}

fn absolutize(p: &str) -> String {
    let path = git_track_core::discover::expand_tilde(p);
    std::path::absolute(&path)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn run_json(request: &ScanRequest) {
    let out = Mutex::new(io::stdout());
    let emit = |event: Event| {
        let Ok(line) = serde_json::to_string(&event) else {
            return;
        };
        let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
        // A closed pipe (GUI went away) is not worth crashing over.
        let _ = writeln!(out, "{line}").and_then(|_| out.flush());
    };
    emit(Event::Hello {
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL_VERSION,
        distro: std::env::var("WSL_DISTRO_NAME").ok(),
    });
    scan(request, &emit);
}

fn run_text(request: &ScanRequest, show_clean: bool) -> ExitCode {
    let state = Mutex::new((0usize, 0usize)); // (total, dirty)
    let emit = |event: Event| match event {
        Event::Repo(repo) => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.0 += 1;
            if repo.is_clean() {
                if show_clean {
                    println!("  ok  {}", repo.path);
                }
                return;
            }
            s.1 += 1;
            let branch = repo.branch.as_deref().unwrap_or("(detached)");
            println!("{}  [{branch}]", repo.path);
            if let Some(err) = &repo.error {
                println!("    ! error: {err}");
            }
            for issue in &repo.issues {
                let marker = match issue.severity() {
                    Severity::Danger => '!',
                    Severity::Warning => '~',
                };
                println!("    {marker} {}", issue.description());
            }
        }
        Event::Gone { path } => eprintln!("not a repository (anymore): {path}"),
        Event::RootError { root, message } => eprintln!("cannot scan {root}: {message}"),
        Event::Fatal { message } => eprintln!("fatal: {message}"),
        Event::Done { elapsed_ms } => {
            let s = state.lock().unwrap_or_else(|e| e.into_inner());
            eprintln!(
                "\n{} repositories checked in {:.1}s, {} with unpushed work",
                s.0,
                elapsed_ms as f64 / 1000.0,
                s.1
            );
        }
        Event::Hello { .. } | Event::Discovered { .. } => {}
    };
    scan(request, &emit);
    let dirty = state.lock().unwrap_or_else(|e| e.into_inner()).1;
    if dirty > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
