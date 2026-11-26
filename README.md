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

1. **Agent in WSL** (from a WSL shell in this repo):

   ```sh
   cargo install --path crates/agent --locked
   ```

   This puts `git-track-agent` into `~/.cargo/bin/`, the default path the app uses.
   Rebuild it after changing `crates/core` or `crates/agent`.

2. **App on Windows**:

   ```bat
   cargo build --release -p git-track
   target\release\git-track.exe
   ```

   (`cargo build` without `-p` builds only core + agent; the app is excluded
   from the default members so building in WSL does not pull in GPUI.)

On first start a fully commented config with all defaults is created at
`%APPDATA%\git-track\config.toml`. No Windows folders are scanned until you add
some (WSL starts with `~/projects`).

**Settings** ("Menu ▾" → *Settings…* or Ctrl+,) edits the common options: the
folders to scan and ignored paths per side (type a path or *Browse…*; for WSL
pick a folder below `\\wsl.localhost\<distro>`), the WSL distribution, the
reported issue kinds, the rescan interval, tray, autostart and the JetBrains
button. Saving writes them into the config file and keeps its comments and
everything else. Actions, excluded folder names, branches and depth are edited
in the file itself ("Menu ▾" → *Open config in default editor*); it is re-read
on every full refresh.

## Using the app

* **Refresh all** / **Rescan** per source — full rescan (also automatic every
  `refresh_interval_secs`, default 10 min).
* **↻** — re-check a single repository (no discovery, fast).
* Action buttons (Git Bash, Terminal, Explorer, WSL Terminal, …) are defined in
  the config as `[[actions]]` with `{path}`, `{distro}` and `{unc}` placeholders.
  Repositories an action was launched for are re-checked automatically whenever
  the window regains focus, so they disappear right after you push.
* **JetBrains projects**: if the repository or a parent directory (up to the
  root) contains a `.idea` folder, a button opens that project in its IDE. The
  IDE is taken from the IDEs' recent projects lists
  (`%APPDATA%\JetBrains\*\options\recentProjects.xml`, the most recently used
  one wins); if the project is in none of them, "IDE ▾" lists the installed IDEs
  (from JetBrains Toolbox's `state.json`, or `product-info.json` in
  `%LOCALAPPDATA%\Programs` / `%ProgramFiles%\JetBrains`). WSL projects are opened
  via `\\wsl.localhost\…`. Disable with `[jetbrains] enabled = false`.
* **Deleted repositories** (just `.git` or the whole folder) disappear from the
  list without a full rescan: Windows repositories are looked up every 15 s and
  when the window is activated; WSL repositories are re-checked by the agent on
  activation (at most once a minute). A re-check (↻) of a deleted repository
  removes it, too.
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
a JSON `ScanRequest` on stdin, NDJSON `Event`s on stdout (see
`crates/core/src/protocol.rs`).

## Notes

* Background `git` calls run with `GIT_OPTIONAL_LOCKS=0`, so scans never take
  the index lock in a repository you are working in.
* Repositories git refuses to open (e.g. "dubious ownership") show up with an
  error chip; fix them with `git config --global --add safe.directory …`.
