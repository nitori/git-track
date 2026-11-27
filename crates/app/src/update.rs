//! Update check against the GitHub releases, and self-replacement.
//!
//! A release carries the portable `git-track.exe`. Installing downloads it
//! next to the running executable, checks its SHA-256 against the digest
//! GitHub publishes, swaps the files (a running executable can be renamed on
//! Windows, just not overwritten) and starts the new one with
//! `--updated <pid>`, which waits for this process to exit before it takes
//! over (see [`after_start`]). The old file is deleted on the next start.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO: &str = "nitori/git-track";
const ASSET: &str = "git-track.exe";
const ARG: &str = "--updated";

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Without the leading "v".
    pub version: String,
    pub notes_url: String,
    download_url: String,
    /// Hex SHA-256 of the asset, if GitHub reports one.
    sha256: Option<String>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

fn client(timeout: Duration) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("git-track/{CURRENT}"))
        .timeout(timeout)
        .build()
        .map_err(|e| format!("cannot create HTTP client: {e}"))
}

/// The latest release if it is newer than this build. Blocking.
pub fn check() -> Result<Option<Release>, String> {
    let response = client(Duration::from_secs(30))?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| format!("cannot reach GitHub: {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // No release published yet.
        return Ok(None);
    }
    let text = response
        .error_for_status()
        .and_then(|r| r.text())
        .map_err(|e| format!("update check failed: {e}"))?;
    let release: ApiRelease =
        serde_json::from_str(&text).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    let version = release.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&version, CURRENT) {
        return Ok(None);
    }
    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name.eq_ignore_ascii_case(ASSET))
        .ok_or_else(|| format!("release {version} has no {ASSET}"))?;
    Ok(Some(Release {
        version,
        notes_url: release.html_url,
        download_url: asset.browser_download_url,
        sha256: asset
            .digest
            .and_then(|d| d.strip_prefix("sha256:").map(str::to_ascii_lowercase)),
    }))
}

/// `a > b` for versions like "1.2.3"; anything unparsable is never newer.
fn is_newer(a: &str, b: &str) -> bool {
    fn parse(v: &str) -> Option<Vec<u64>> {
        // Pre-release suffixes ("1.2.3-beta") are not used; ignore them.
        let core = v.split(['-', '+']).next()?;
        core.split('.').map(|n| n.parse().ok()).collect()
    }
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Downloads the release and puts it in place of the running executable.
/// Blocking. Afterwards [`restart`] starts it.
pub fn install(release: &Release) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate git-track.exe: {e}"))?;
    let new = sibling(&exe, ".new");
    let old = sibling(&exe, ".old");

    let bytes = client(Duration::from_secs(300))?
        .get(&release.download_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("download failed: {e}"))?;
    if bytes.len() < 1024 {
        return Err("download failed: the file is empty".into());
    }
    if let Some(expected) = &release.sha256 {
        let actual: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != *expected {
            return Err(format!(
                "download corrupted (SHA-256 {actual}, expected {expected})"
            ));
        }
    }

    std::fs::write(&new, &bytes)
        .map_err(|e| format!("cannot write {}: {e}", new.display()))?;
    let _ = std::fs::remove_file(&old);
    if let Err(e) = std::fs::rename(&exe, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(format!("cannot move {} aside: {e}", exe.display()));
    }
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        let _ = std::fs::remove_file(&new);
        return Err(format!("cannot put the new version in place: {e}"));
    }
    Ok(())
}

/// Starts the (new) executable, which waits for this process to exit.
pub fn restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate git-track.exe: {e}"))?;
    std::process::Command::new(&exe)
        .arg(ARG)
        .arg(std::process::id().to_string())
        .spawn()
        .map(drop)
        .map_err(|e| format!("cannot start {}: {e}", exe.display()))
}

/// Call first thing on startup. After an update, waits for the previous
/// process to exit (so the single-instance check does not hand over to
/// it); always removes a leftover old executable. Returns whether this
/// start finishes an update.
pub fn after_start() -> bool {
    let mut args = std::env::args().skip(1);
    let previous = args
        .by_ref()
        .find(|a| a == ARG)
        .and_then(|_| args.next())
        .and_then(|pid| pid.parse::<u32>().ok());
    if let Some(pid) = previous {
        imp::wait_for_exit(pid, Duration::from_secs(10));
    }
    if let Ok(exe) = std::env::current_exe() {
        let old = sibling(&exe, ".old");
        // Just exited: its file may stay locked for a moment.
        for _ in 0..if previous.is_some() { 10 } else { 1 } {
            match std::fs::remove_file(&old) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    std::thread::sleep(Duration::from_millis(200));
                }
                _ => break,
            }
        }
    }
    previous.is_some()
}

#[cfg(windows)]
mod imp {
    use std::time::Duration;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    pub fn wait_for_exit(pid: u32, timeout: Duration) {
        unsafe {
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            // Null: exited already (or not ours to wait for).
            if !process.is_null() {
                WaitForSingleObject(process, timeout.as_millis() as u32);
                CloseHandle(process);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn wait_for_exit(_pid: u32, timeout: std::time::Duration) {
        std::thread::sleep(timeout.min(std::time::Duration::from_secs(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.0"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("0.2.0-beta", "0.2.0"));
    }

    #[test]
    #[ignore = "asks api.github.com"]
    fn reaches_github() {
        let result = check();
        println!("{result:?}");
        assert!(result.is_ok());
    }

    #[test]
    fn sibling_names() {
        let exe = Path::new(r"C:\Tools\git-track.exe");
        assert_eq!(sibling(exe, ".old"), Path::new(r"C:\Tools\git-track.exe.old"));
    }
}
