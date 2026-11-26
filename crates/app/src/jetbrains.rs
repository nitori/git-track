//! JetBrains IDE integration: which IDEs are installed and which one a
//! project was last opened with.
//!
//! * Installed IDEs come from JetBrains Toolbox (`state.json`) and from
//!   `product-info.json` in the standard install locations.
//! * Every IDE keeps `%APPDATA%\JetBrains\<Product><Version>\options\recentProjects.xml`
//!   with the project paths it opened (WSL projects as `//wsl$/<distro>/…` or
//!   `//wsl.localhost/<distro>/…`), the product code and timestamps. The IDE
//!   that activated a project last wins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::actions::unc_path;
use crate::scanner::SourceKind;

#[derive(Debug, Clone)]
pub struct Ide {
    /// Product code, e.g. "PS", "RR", "IU".
    pub code: String,
    pub name: String,
    pub exe: PathBuf,
}

#[derive(Default)]
pub struct JetBrains {
    /// Sorted by name.
    ides: Vec<Ide>,
    /// Normalized project path → (product code, last activation in ms).
    recent: HashMap<String, (String, u64)>,
}

impl JetBrains {
    /// Reads everything from disk (a few small files).
    pub fn load() -> Self {
        let mut recent = HashMap::new();
        if let Some(config) = dirs::config_dir() {
            load_recent_projects(&config.join("JetBrains"), &mut recent);
        }
        Self {
            ides: installed_ides(),
            recent,
        }
    }

    pub fn ides(&self) -> &[Ide] {
        &self.ides
    }

    /// The IDE the project at `path` was last opened with, if it is installed.
    pub fn detect(&self, kind: SourceKind, path: &str, distro: Option<&str>) -> Option<&Ide> {
        let (code, _) = self.recent.get(&project_key(kind, path, distro)?)?;
        self.ides.iter().find(|ide| &ide.code == code)
    }
}

/// Opens the project in the IDE (an already running IDE takes it over).
pub fn launch(ide: &Ide, kind: SourceKind, path: &str, distro: Option<&str>) -> Result<(), String> {
    let target = match kind {
        SourceKind::Windows => path.to_string(),
        SourceKind::Wsl => unc_path(distro.unwrap_or_default(), path),
    };
    Command::new(&ide.exe)
        .arg(&target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| format!("cannot start {} ({}): {e}", ide.name, ide.exe.display()))
}

/// Comparable form of a project path as Windows programs see it.
fn project_key(kind: SourceKind, path: &str, distro: Option<&str>) -> Option<String> {
    match kind {
        SourceKind::Windows => Some(normalize(path)),
        SourceKind::Wsl => Some(normalize(&format!("//wsl.localhost/{}{path}", distro?))),
    }
}

fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/").to_lowercase();
    let path = match path.strip_prefix("//wsl$/") {
        Some(rest) => format!("//wsl.localhost/{rest}"),
        None => path,
    };
    path.trim_end_matches('/').to_string()
}

fn load_recent_projects(jetbrains_config: &Path, recent: &mut HashMap<String, (String, u64)>) {
    let Ok(entries) = std::fs::read_dir(jetbrains_config) else {
        return;
    };
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    for entry in entries.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let Ok(text) = std::fs::read_to_string(entry.path().join("options/recentProjects.xml"))
        else {
            continue;
        };
        let Ok(doc) = roxmltree::Document::parse(&text) else {
            continue;
        };
        for node in doc.descendants().filter(|n| n.has_tag_name("entry")) {
            let Some(key) = node.attribute("key") else {
                continue;
            };
            let Some(meta) = node
                .descendants()
                .find(|n| n.has_tag_name("RecentProjectMetaInfo"))
            else {
                continue;
            };
            let option = |name: &str| {
                meta.children()
                    .find(|n| n.has_tag_name("option") && n.attribute("name") == Some(name))
                    .and_then(|n| n.attribute("value"))
            };
            let Some(code) = option("productionCode")
                .map(str::to_string)
                .or_else(|| code_from_config_dir(&dir_name))
            else {
                continue;
            };
            let time = option("activationTimestamp")
                .or_else(|| option("projectOpenTimestamp"))
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            let key = normalize(&key.replace("$USER_HOME$", &home));
            let slot = recent.entry(key).or_insert_with(|| (code.clone(), 0));
            if time >= slot.1 {
                *slot = (code, time);
            }
        }
    }
}

/// Product code from a config directory name like "PhpStorm2026.3" (for old
/// files without `productionCode`).
fn code_from_config_dir(name: &str) -> Option<String> {
    let product = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    let code = match product {
        "PhpStorm" => "PS",
        "RustRover" => "RR",
        "PyCharm" => "PY",
        "PyCharmCE" => "PC",
        "IntelliJIdea" => "IU",
        "IdeaIC" => "IC",
        "WebStorm" => "WS",
        "CLion" => "CL",
        "Rider" => "RD",
        "GoLand" => "GO",
        "RubyMine" => "RM",
        "DataGrip" => "DB",
        _ => return None,
    };
    Some(code.to_string())
}

fn installed_ides() -> Vec<Ide> {
    let mut ides: Vec<Ide> = Vec::new();
    let mut add = |ide: Ide| {
        if ide.exe.is_file() && !ides.iter().any(|i| i.code == ide.code) {
            ides.push(ide);
        }
    };

    let local = dirs::data_local_dir();
    // Toolbox knows exactly what it installed and where.
    if let Some(local) = &local {
        for ide in toolbox_ides(&local.join(r"JetBrains\Toolbox\state.json")) {
            add(ide);
        }
    }
    // Standalone installs (and Toolbox's default install location).
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(local) = &local {
        bases.push(local.join("Programs"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        bases.push(PathBuf::from(program_files).join("JetBrains"));
    }
    for base in bases {
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(ide) = product_info_ide(&entry.path()) {
                add(ide);
            }
        }
    }

    ides.sort_by(|a, b| a.name.cmp(&b.name));
    ides
}

fn toolbox_ides(state_json: &Path) -> Vec<Ide> {
    let Ok(text) = std::fs::read_to_string(state_json) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(tools) = state["tools"].as_array() else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| {
            Some(Ide {
                code: tool["productCode"].as_str()?.to_string(),
                name: tool["displayName"].as_str()?.to_string(),
                exe: PathBuf::from(tool["launchCommand"].as_str()?),
            })
        })
        .collect()
}

fn product_info_ide(install_dir: &Path) -> Option<Ide> {
    let text = std::fs::read_to_string(install_dir.join("product-info.json")).ok()?;
    let info: serde_json::Value = serde_json::from_str(&text).ok()?;
    let launcher = info["launch"]
        .as_array()?
        .iter()
        .find(|l| l["os"].as_str() == Some("Windows"))?["launcherPath"]
        .as_str()?;
    Some(Ide {
        code: info["productCode"].as_str()?.to_string(),
        name: info["name"].as_str()?.to_string(),
        exe: install_dir.join(launcher),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_keys_match_recent_project_paths() {
        assert_eq!(
            project_key(SourceKind::Windows, r"D:\Projects\Foo\", None).unwrap(),
            normalize("D:/Projects/Foo")
        );
        let wsl = project_key(SourceKind::Wsl, "/home/me/projects/x", Some("Debian")).unwrap();
        assert_eq!(wsl, normalize("//wsl$/Debian/home/me/projects/x"));
        assert_eq!(wsl, normalize("//wsl.localhost/Debian/home/me/projects/x/"));
        assert_eq!(project_key(SourceKind::Wsl, "/home/me", None), None);
    }

    #[test]
    fn recent_projects_pick_latest_activation() {
        let tmp = std::env::temp_dir().join(format!("git-track-jb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let xml = |entries: &[(&str, &str, u64)]| {
            let mut s = String::from(
                r#"<application><component name="RecentProjectsManager"><option name="additionalInfo"><map>"#,
            );
            for (key, code, time) in entries {
                s += &format!(
                    r#"<entry key="{key}"><value><RecentProjectMetaInfo frameTitle="x">
                       <option name="activationTimestamp" value="{time}" />
                       <option name="productionCode" value="{code}" />
                       </RecentProjectMetaInfo></value></entry>"#
                );
            }
            s + "</map></option></component></application>"
        };
        for (dir, content) in [
            (
                "PhpStorm2026.3",
                xml(&[("D:/a", "PS", 10), ("D:/b", "PS", 10)]),
            ),
            ("RustRover2026.3", xml(&[("D:/b", "RR", 20)])),
        ] {
            std::fs::create_dir_all(tmp.join(dir).join("options")).unwrap();
            std::fs::write(tmp.join(dir).join("options/recentProjects.xml"), content).unwrap();
        }

        let mut recent = HashMap::new();
        load_recent_projects(&tmp, &mut recent);
        assert_eq!(recent["d:/a"].0, "PS");
        assert_eq!(recent["d:/b"].0, "RR");

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    /// Diagnostic: `cargo test -p git-track print_local -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn print_local_installation() {
        let jb = JetBrains::load();
        for ide in jb.ides() {
            println!("{} {} {}", ide.code, ide.name, ide.exe.display());
        }
        let mut recent: Vec<_> = jb.recent.iter().collect();
        recent.sort();
        for (path, (code, time)) in recent {
            println!("{code} {time} {path}");
        }
    }

    #[test]
    fn config_dir_codes() {
        assert_eq!(
            code_from_config_dir("PhpStorm2026.3").as_deref(),
            Some("PS")
        );
        assert_eq!(
            code_from_config_dir("IntelliJIdea2026.1").as_deref(),
            Some("IU")
        );
        assert_eq!(code_from_config_dir("consentOptions"), None);
    }
}
