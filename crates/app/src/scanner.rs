//! Runs scans off the UI thread.
//!
//! Every scan gets its own OS thread and streams results back through an
//! unbounded channel, which the UI drains in a foreground task:
//! * Windows: the core scanner runs in-process.
//! * WSL: `git-track-agent` is started via `wsl.exe` and its NDJSON output is
//!   forwarded line by line. Scanning natively inside WSL is far faster than
//!   accessing `\\wsl$` from Windows (or `/mnt/*` from WSL).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use git_track_core::{Event, ScanRequest};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Windows,
    Wsl,
}

#[derive(Debug)]
pub enum Msg {
    Event(Event),
    /// The scan could not be run (or the agent died). Partial results may have been sent.
    Failed(String),
}

pub fn start(kind: SourceKind, request: ScanRequest, config: &Config) -> UnboundedReceiver<Msg> {
    let (tx, rx) = unbounded();
    let wsl = config.wsl.clone();
    let name = format!("git-track-{kind:?}").to_lowercase();
    let spawned = std::thread::Builder::new().name(name).spawn({
        let tx = tx.clone();
        move || match kind {
            SourceKind::Windows => run_local(&request, &tx),
            SourceKind::Wsl => run_wsl(&request, wsl.distro.as_deref(), &wsl.agent, &tx),
        }
    });
    if let Err(e) = spawned {
        let _ = tx.unbounded_send(Msg::Failed(format!("cannot start scan thread: {e}")));
    }
    rx
}

fn run_local(request: &ScanRequest, tx: &UnboundedSender<Msg>) {
    git_track_core::scan(request, &|event| {
        let _ = tx.unbounded_send(Msg::Event(event));
    });
}

fn run_wsl(request: &ScanRequest, distro: Option<&str>, agent: &str, tx: &UnboundedSender<Msg>) {
    let fail = |msg: String| {
        let _ = tx.unbounded_send(Msg::Failed(msg));
    };

    let mut cmd = Command::new("wsl.exe");
    if let Some(distro) = distro.filter(|d| !d.is_empty()) {
        cmd.args(["-d", distro]);
    }
    // `-e` runs the program without a shell (no quoting issues), so `~` is
    // not expanded; start in the home directory and use a relative path instead.
    if let Some(relative) = agent.strip_prefix("~/") {
        cmd.args(["--cd", "~", "-e", relative]);
    } else {
        cmd.args(["-e", agent]);
    }
    cmd.args(["scan", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return fail(format!("cannot start wsl.exe: {e}")),
    };

    let request_json = serde_json::to_string(request).unwrap_or_default();
    if let Some(mut stdin) = child.stdin.take() {
        // Dropping stdin closes it, which tells the agent the request is complete.
        let _ = stdin.write_all(request_json.as_bytes());
    }

    // Drain stderr concurrently so a chatty agent cannot block on a full pipe.
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        })
    });

    let mut done = false;
    let mut protocol_error = None;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line) {
                Ok(event) => {
                    done |= matches!(event, Event::Done { .. } | Event::Fatal { .. });
                    if tx.unbounded_send(Msg::Event(event)).is_err() {
                        // UI no longer interested.
                        let _ = child.kill();
                        return;
                    }
                }
                Err(e) => {
                    protocol_error
                        .get_or_insert_with(|| format!("unexpected agent output ({e}): {line}"));
                }
            }
        }
    }

    let status = child.wait();
    let stderr = stderr_reader
        .and_then(|h| h.join().ok())
        .map(|b| decode_wsl_output(&b))
        .unwrap_or_default();
    if !done {
        let mut msg = match status {
            Ok(s) => format!("WSL agent exited ({s}) without finishing"),
            Err(e) => format!("WSL agent: {e}"),
        };
        if let Some(p) = protocol_error {
            msg.push_str(&format!("\n{p}"));
        }
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            msg.push_str(&format!("\n{stderr}"));
        }
        msg.push_str(&format!(
            "\nIs the agent installed? Expected at '{agent}' (inside WSL: cargo install --path crates/agent)"
        ));
        fail(msg);
    }
}

/// `wsl.exe` prints its own messages as UTF-16LE, while the Linux side writes UTF-8.
fn decode_wsl_output(bytes: &[u8]) -> String {
    let odd_nuls = bytes.iter().skip(1).step_by(2).filter(|b| **b == 0).count();
    if bytes.len() >= 4 && odd_nuls * 2 >= bytes.len() / 2 {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}
