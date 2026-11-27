use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The WSL agent is built for this target: static, runs on any x86_64 distribution.
const AGENT_TARGET: &str = "x86_64-unknown-linux-musl";

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/git-track.ico");
    println!("cargo:rerun-if-changed=assets/git-track-alert.ico");
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if windows {
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("failed to compile Windows resources");
    }
    bundle_agent(windows);
}

/// Generates `$OUT_DIR/agent.rs` with the agent binary to install inside
/// WSL (Windows builds only). Without it the app still works with an agent
/// installed by hand.
fn bundle_agent(windows: bool) {
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let none = "pub const AGENT: Option<(&[u8], &str)> = None;\n".to_string();
    let code = if !windows {
        none
    } else {
        match agent_binary().and_then(|bin| {
            fs::read(&bin).map_err(|e| format!("cannot read {}: {e}", bin.display()))
        }) {
            Ok(bytes) => {
                let copy = out.join("git-track-agent");
                fs::write(&copy, &bytes).expect("writing to OUT_DIR");
                format!(
                    "pub const AGENT: Option<(&[u8], &str)> = Some((include_bytes!({:?}), \"{:016x}\"));\n",
                    copy.to_string_lossy(),
                    fnv1a(&bytes)
                )
            }
            Err(e) => {
                println!("cargo:warning=the WSL agent is not bundled: {e}");
                none
            }
        }
    };
    fs::write(out.join("agent.rs"), code).expect("writing to OUT_DIR");
}

/// Path of the agent binary: `GIT_TRACK_AGENT_BIN`, or built from the
/// workspace with cargo (separate target directory, so the outer build's
/// lock is not in the way).
fn agent_binary() -> Result<PathBuf, String> {
    println!("cargo:rerun-if-env-changed=GIT_TRACK_AGENT_BIN");
    if let Some(bin) = env::var_os("GIT_TRACK_AGENT_BIN") {
        let bin = PathBuf::from(bin);
        println!("cargo:rerun-if-changed={}", bin.display());
        return Ok(bin);
    }

    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let workspace = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("workspace not found")?;
    for path in [
        "crates/core/src",
        "crates/core/Cargo.toml",
        "crates/agent/src",
        "crates/agent/Cargo.toml",
        "Cargo.lock",
    ] {
        println!("cargo:rerun-if-changed={}", workspace.join(path).display());
    }

    let target_dir = workspace.join("target").join("agent");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(workspace)
        .args(["build", "-p", "git-track-agent", "--release", "--locked"])
        .args(["--target", AGENT_TARGET, "--target-dir"])
        .arg(&target_dir)
        // The agent is pure Rust: Rust's own linker is all it needs.
        .env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER", "rust-lld")
        // Meant for the outer (Windows) build.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !status.success() {
        return Err(format!(
            "building it for {AGENT_TARGET} failed ({status}); is the target installed? \
             (rustup target add {AGENT_TARGET})"
        ));
    }
    Ok(target_dir
        .join(AGENT_TARGET)
        .join("release")
        .join("git-track-agent"))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
