//! Update check against the GitHub releases, and self-replacement.
//!
//! A release carries the portable `git-track.exe`. Installing downloads it
//! next to the running executable, checks its SHA-256 against the digest
//! GitHub publishes, swaps the files (a running executable can be renamed on
//! Windows, just not overwritten) and starts the new one with
//! `--updated <pid>`, which waits for this process to exit before it takes
//! over (see [`after_start`]). The old file is deleted on the next start.
//!
//! HTTP goes through WinHTTP (part of Windows, uses its TLS and proxy
//! settings) instead of a Rust HTTP/TLS stack, which would add ~3.5 MB.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO: &str = "nitori/git-track";
const ASSET: &str = "git-track.exe";
const ARG: &str = "--updated";
/// Per connect / send / receive step, not for the whole transfer.
const TIMEOUT: Duration = Duration::from_secs(30);

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

/// `GET url` over HTTPS (redirects are followed); returns status and body.
fn get(url: &str, accept: Option<&str>) -> Result<(u16, Vec<u8>), String> {
    let (host, path) = split_url(url).ok_or_else(|| format!("unsupported URL {url}"))?;
    imp::get(host, path, &format!("git-track/{CURRENT}"), accept)
}

/// `https://host/path?query` into host and path (with the query).
fn split_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, "/"),
    };
    (!host.is_empty() && !host.contains([':', '@'])).then_some((host, path))
}

/// The latest release if it is newer than this build. Blocking.
pub fn check() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let (status, body) = get(&url, Some("application/vnd.github+json"))
        .map_err(|e| format!("cannot reach GitHub: {e}"))?;
    match status {
        200 => {}
        // No release published yet.
        404 => return Ok(None),
        _ => return Err(format!("update check failed: GitHub answered HTTP {status}")),
    }
    let release: ApiRelease =
        serde_json::from_slice(&body).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
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

    let (status, bytes) =
        get(&release.download_url, None).map_err(|e| format!("download failed: {e}"))?;
    if status != 200 {
        return Err(format!("download failed: HTTP {status}"));
    }
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
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
    use windows_sys::Win32::Networking::WinHttp::*;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    /// A WinHTTP handle, closed on drop.
    struct Handle(*mut c_void);

    impl Handle {
        fn new(handle: *mut c_void) -> Result<Self, String> {
            if handle.is_null() {
                Err(last_error())
            } else {
                Ok(Self(handle))
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn last_error() -> String {
        let code = unsafe { GetLastError() };
        let what = match code {
            ERROR_WINHTTP_TIMEOUT => "timed out",
            ERROR_WINHTTP_NAME_NOT_RESOLVED => "server name not resolved (offline?)",
            ERROR_WINHTTP_CANNOT_CONNECT => "cannot connect",
            ERROR_WINHTTP_CONNECTION_ERROR => "connection reset",
            ERROR_WINHTTP_SECURE_FAILURE => "TLS certificate error",
            // Not WinHTTP's own range: the system knows the message.
            _ if !(12000..13000).contains(&code) => {
                return std::io::Error::from_raw_os_error(code as i32).to_string();
            }
            _ => "network error",
        };
        format!("{what} (WinHTTP error {code})")
    }

    pub fn get(
        host: &str,
        path: &str,
        user_agent: &str,
        accept: Option<&str>,
    ) -> Result<(u16, Vec<u8>), String> {
        let timeout = i32::try_from(super::TIMEOUT.as_millis()).unwrap_or(i32::MAX);
        let headers = accept.map(|a| wide(&format!("Accept: {a}")));
        unsafe {
            // Automatic proxy: the system's proxy settings / auto-config.
            let session = Handle::new(WinHttpOpen(
                wide(user_agent).as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            ))?;
            if WinHttpSetTimeouts(session.0, timeout, timeout, timeout, timeout) == 0 {
                return Err(last_error());
            }
            let connection = Handle::new(WinHttpConnect(
                session.0,
                wide(host).as_ptr(),
                INTERNET_DEFAULT_HTTPS_PORT,
                0,
            ))?;
            // Redirects (GitHub's downloads go to another host) are followed
            // by default, except from HTTPS to HTTP.
            let request = Handle::new(WinHttpOpenRequest(
                connection.0,
                wide("GET").as_ptr(),
                wide(path).as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            let (headers, length) = match &headers {
                Some(h) => (h.as_ptr(), u32::MAX), // null-terminated
                None => (null(), 0),
            };
            if WinHttpSendRequest(request.0, headers, length, null(), 0, 0, 0) == 0
                || WinHttpReceiveResponse(request.0, null_mut()) == 0
            {
                return Err(last_error());
            }
            let mut status = 0u32;
            let mut size = size_of::<u32>() as u32;
            if WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&raw mut status).cast(),
                &mut size,
                null_mut(),
            ) == 0
            {
                return Err(last_error());
            }
            let mut body = Vec::new();
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let mut read = 0u32;
                if WinHttpReadData(
                    request.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut read,
                ) == 0
                {
                    return Err(last_error());
                }
                if read == 0 {
                    break;
                }
                body.extend_from_slice(&buffer[..read as usize]);
            }
            Ok((status as u16, body))
        }
    }

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
    pub fn get(
        _host: &str,
        _path: &str,
        _user_agent: &str,
        _accept: Option<&str>,
    ) -> Result<(u16, Vec<u8>), String> {
        Err("updates are only supported on Windows".into())
    }

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
    fn splits_urls() {
        assert_eq!(
            split_url("https://api.github.com/repos/a/b/releases/latest"),
            Some(("api.github.com", "/repos/a/b/releases/latest"))
        );
        assert_eq!(split_url("https://example.com"), Some(("example.com", "/")));
        assert_eq!(split_url("https://h/p?q=1"), Some(("h", "/p?q=1")));
        assert_eq!(split_url("http://example.com/x"), None);
        assert_eq!(split_url("https://example.com:8443/x"), None);
        assert_eq!(split_url("https:///x"), None);
    }

    #[test]
    #[ignore = "downloads the latest release from GitHub"]
    fn downloads_release() {
        let url = format!("https://github.com/{REPO}/releases/latest/download/{ASSET}");
        let (status, body) = get(&url, None).unwrap();
        println!("HTTP {status}, {} bytes", body.len());
        assert_eq!(status, 200);
        assert_eq!(&body[..2], b"MZ");
    }

    #[test]
    fn sibling_names() {
        let exe = Path::new(r"C:\Tools\git-track.exe");
        assert_eq!(sibling(exe, ".old"), Path::new(r"C:\Tools\git-track.exe.old"));
    }
}
