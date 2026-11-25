//! Launching the per-repository buttons (terminal, explorer, …).

use std::process::{Command, Stdio};

use crate::config::Action;
use crate::scanner::SourceKind;

pub struct Vars<'a> {
    pub kind: SourceKind,
    pub path: &'a str,
    pub distro: Option<&'a str>,
}

impl Vars<'_> {
    fn substitute(&self, arg: &str) -> String {
        let distro = self.distro.unwrap_or_default();
        let unc = match self.kind {
            SourceKind::Wsl => unc_path(distro, self.path),
            SourceKind::Windows => self.path.to_string(),
        };
        arg.replace("{path}", self.path)
            .replace("{distro}", distro)
            .replace("{unc}", &unc)
    }
}

/// A WSL path as Windows programs see it: `\\wsl.localhost\<distro>\…`.
pub fn unc_path(distro: &str, path: &str) -> String {
    format!(r"\\wsl.localhost\{distro}{}", path.replace('/', "\\"))
}

pub fn launch(action: &Action, vars: &Vars) -> Result<(), String> {
    let mut argv = action.command.iter().map(|a| vars.substitute(a));
    let program = argv
        .next()
        .ok_or_else(|| format!("action '{}' has an empty command", action.label))?;
    let mut cmd = Command::new(&program);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if vars.kind == SourceKind::Windows {
        cmd.current_dir(vars.path);
    }
    cmd.spawn()
        .map(drop)
        .map_err(|e| format!("{}: cannot start '{program}': {e}", action.label))
}
