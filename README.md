# git-track

Lists git repositories that contain work which is not pushed — on Windows and
inside WSL2 — so nothing gets forgotten on the local machine.

A GPUI desktop app (Windows) scans the Windows project roots itself and starts
a small agent inside WSL for the Linux roots. Each side scans natively; going
through `\\wsl$` or `/mnt/*` would be very slow.

## What counts as "not pushed"

| Issue | Meaning |
|---|---|
| uncommitted | staged, modified, untracked or conflicted files |
| `<branch>: N unpushed` | branch is ahead of its upstream (red if commits are on no remote branch at all, yellow if they exist on some other remote branch) |
| no upstream | branch without upstream that has commits not on any remote |
| upstream gone | upstream was deleted on the remote, local commits not on any remote |
| detached HEAD | detached HEAD with commits not on any remote |
| no remote | repository has commits but no remote configured |
| stash | stash entries (stashes are never pushed) |
| … in progress | unfinished merge / rebase / cherry-pick / revert / bisect |

Branches checked: the checked-out branch plus `main` / `master` (configurable,
or `check_all_branches = true`). Ahead/behind information comes from the local
remote-tracking refs; no network access happens.

Every kind can be switched off in the `[track]` section of the config
(`staged`, `modified`, `untracked`, `conflicted`, `unpushed`, `ahead_elsewhere`,
`no_upstream`, `upstream_gone`, `detached_head`, `no_remote`, `stash`,
`in_progress` — all `true` by default; misspelled names are reported as a
config error). For the agent CLI use `--skip stash,untracked`.

### Repository discovery

Every directory containing a `.git` directory **or file** is a repository, and
the walk continues *inside* repositories, so nested repositories that are not
submodules (and usually gitignored by their parent) are found as well. Broken
`.git` entries are skipped: a `.git` folder needs `HEAD` and an object store, a
`.git` file must point (`gitdir: …`) to such a folder.
`.gitignore` is deliberately not honoured. Symlinks/junctions are not followed.
Directory names in `scan.exclude_dirs` (default: `node_modules`, caches, venvs)
are skipped.

## Layout

```
crates/core    discovery, git checks, JSON protocol (no UI deps)
crates/agent   git-track-agent: CLI, also used by the app inside WSL
crates/app     git-track: the GPUI app (Windows)
```

## Setup

Download `git-track.exe` from the
[releases](https://github.com/nitori/git-track/releases) and start it; it is
portable (no installer). Or build it on Windows:

```bat
rustup target add x86_64-unknown-linux-musl
cargo build --release -p git-track
target\release\git-track.exe
```

(`cargo build` without `-p` builds only core + agent; the app is excluded from
the default members so building in WSL does not pull in GPUI.)

The **WSL agent is bundled**: the app's build script builds it for
`x86_64-unknown-linux-musl` (linked with Rust's own `rust-lld`, no Linux
toolchain needed) and embeds it. On the first WSL scan of a run, the app
copies it to `~/.local/share/git-track/agent-<hash>` inside WSL unless that
exact build is there already. Agents of other builds are deleted after a week
without use. If the musl target is missing, the build prints a warning and the
app falls back to a self-installed agent. To use your own agent build (e.g.
for another architecture), `cargo install --path crates/agent --locked` inside
WSL and set `agent` under `[wsl]` to its full path.

On first start a fully commented config with all defaults is created at
`%APPDATA%\git-track\config.toml`. No Windows folders are scanned until you add
some (WSL starts with `~/projects`).

**Settings** ("Menu ▾" → *Settings…* or Ctrl+,) edits the common options: the
folders to scan and ignored paths per side (type a path or *Browse…*; for WSL
pick a folder below `\\wsl.localhost\<distro>`), the WSL distribution, the
reported issue kinds, the rescan interval, tray, autostart, the update check
and the JetBrains button. *Save* writes them into the config file, keeping its
comments and everything else, and leaves the dialog open. Actions, excluded folder names, branches and depth are edited
in the file itself ("Menu ▾" → *Open config in default editor*); it is re-read
on every full refresh.

## Using the app

* **Refresh all** / **Rescan** per source — full rescan (also automatic every
  `refresh_interval_secs`, default 10 min).
* **↻** — re-check a single repository (no discovery, fast).
* Action buttons (Git Bash, Terminal, Explorer, WSL Terminal, …) are defined in
  the config as `[[actions]]` with `{path}`, `{distro}` and `{unc}` placeholders.
* **JetBrains projects**: if the repository or a parent directory (up to the
  root) contains a `.idea` folder, a button opens that project in its IDE. The
  IDE is taken from the IDEs' recent projects lists
  (`%APPDATA%\JetBrains\*\options\recentProjects.xml`, the most recently used
  one wins); if the project is in none of them, "IDE ▾" lists the installed IDEs
  (from JetBrains Toolbox's `state.json`, or `product-info.json` in
  `%LOCALAPPDATA%\Programs` / `%ProgramFiles%\JetBrains`). WSL projects are opened
  via `\\wsl.localhost\…`. Disable with `[jetbrains] enabled = false`.
* **Live updates** between full scans: a watcher per side (in-process for
  Windows, a long-running agent inside WSL) compares the directory listings of
  every known repository's `.git` and `refs/`. That covers `HEAD`, the index,
  branch and remote refs, the stash and so on. No git process runs, and on
  Windows no file is even opened, so hundreds of repositories cost little.
  * A change means git did something (commit, checkout, push, fetch, stash).
    The repository is then re-checked at once, and every 8 s for the next
    5 minutes, which also catches edits to working-tree files.
  * Those "hot" repositories and the ones with issues are compared every 3 s.
    The quiet clean ones (usually the vast majority) are compared every 30 s,
    and all of them right away when the window is activated.
  * Repositories with issues are re-checked every minute anyway.
  * Launching an action or IDE marks the repository as hot in advance.
  * This covers clean repositories too, so a fresh commit in one shows up
    within 30 s at most (at once on activation), and a pushed one disappears.
  * Deleted repositories (just `.git` or the whole folder) are removed. If
    their root is missing too (an unplugged drive), they stay until the next
    scan.
  * The WSL watcher keeps WSL from shutting down when idle while git-track
    runs. If WSL is shut down anyway, the watcher restarts at the next scan or
    window activation, at most once a minute.
* **Ignore ▾** on a repository hides it, or one of its parent folders (below the
  scan root), from now on: ignored paths are skipped during discovery. Undo it
  in Settings (or `ignore = [...]` under `[windows]` / `[wsl]`).
* Click a repository name to copy its path; hover issues for details.
* **Show clean** also lists repositories without problems.
* **System tray**: closing the window only hides it; the app keeps scanning in
  the background. The tray icon gets a red badge while something is unpushed and
  its tooltip shows the summary. Left click toggles the window (hides it if it
  was in front, otherwise shows it), right click opens a menu with *Quit*. Quit fully with "Menu ▾" → *Quit git-track* or Ctrl+Q.
  Set `close_to_tray = false` to get a normal window without tray icon (restart
  required).
* **Start with Windows**: "Menu ▾" → *Start with Windows* toggles an entry
  under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` for this
  executable. Started that way (`--autostart`), git-track stays hidden in the
  tray until you open it. If the entry points to another copy, the item is
  unchecked and its tooltip names that copy; enabling it switches the entry.
* **Single instance**: starting git-track again just brings the running instance
  to the front. This is per executable path, so a debug and a release build can
  run side by side.
* **Updates**: 30 s after the start and then every 12 hours, git-track asks the
  GitHub API for the latest release (`check_updates = true`; release builds
  only; "Menu ▾" → *Check for updates* works always). A newer version shows a
  banner. *Install and restart* downloads `git-track.exe` and verifies its
  SHA-256 against the digest GitHub lists for it. It then swaps it with the
  running executable (the old one becomes `git-track.exe.old`, deleted on the
  next start) and restarts. The executable path stays the same, so autostart
  keeps working.

The icons are generated by `crates/app/assets/make_icons.py`
(`uv run --with pillow make_icons.py` in that directory) and embedded as Windows
resources through `crates/app/app.rc`.

## Agent CLI

Works standalone as well (Linux or Windows):

```sh
git-track-agent scan ~/projects            # human-readable, exit code 1 if anything is unpushed
git-track-agent scan --all ~/projects      # include clean repositories
git-track-agent check path/to/repo         # single repository, no discovery
git-track-agent scan --json ~/projects     # NDJSON events
```

The app talks to it with `wsl.exe [-d distro] --cd ~ -e <agent> scan --stdin`:
a JSON `ScanRequest` on stdin, NDJSON `Event`s on stdout. `watch` keeps
running and reads NDJSON `WatchCommand`s from stdin until it is closed (see
`crates/core/src/protocol.rs`).

## Releasing

CI (`.github/workflows/ci.yml`) runs clippy and the tests on Linux (core,
agent) and Windows (app) for every push to `master` and every pull request.
To publish a release:

1. Bump `version` in the root `Cargo.toml` and commit.
2. `git tag vX.Y.Z && git push origin master vX.Y.Z`

`release.yml` checks that the tag matches the version, builds the app on
Windows (with the bundled agent) and creates the GitHub release with
`git-track.exe` and the standalone `git-track-agent-x86_64-linux`.

## Notes

* Background `git` calls run with `GIT_OPTIONAL_LOCKS=0`, so scans never take
  the index lock in a repository you are working in.
* Repositories git refuses to open (e.g. "dubious ownership") show up with an
  error chip; fix them with `git config --global --add safe.directory …`.
