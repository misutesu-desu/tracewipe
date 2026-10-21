//! Programs installed by Scoop and Chocolatey.

use crate::apps::{App, Installer, Source};
use crate::reg::{Hive, View};
use std::path::{Path, PathBuf};

fn env(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Scoop roots, user first, with whether each holds global installs.
pub fn scoop_roots() -> Vec<(PathBuf, bool)> {
    let mut v = Vec::new();
    if let Some(r) = env("SCOOP").or_else(|| env("USERPROFILE").map(|h| h.join("scoop"))) {
        v.push((r, false));
    }
    if let Some(r) = env("SCOOP_GLOBAL").or_else(|| env("ProgramData").map(|p| p.join("scoop"))) {
        v.push((r, true));
    }
    v.retain(|(r, _)| r.join("apps").is_dir());
    v
}

pub fn choco_root() -> Option<PathBuf> {
    env("ChocolateyInstall").or_else(|| env("ProgramData").map(|p| p.join("chocolatey"))).filter(|r| r.join("lib").is_dir())
}

/// Text between `open` and the next `close`, for the few fields read from
/// manifest.json and .nuspec files without pulling in a parser.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(text[start..end].trim())
}

fn json_string<'a>(json: &'a str, field: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{field}\""))?;
    let rest = &json[at + field.len() + 2..];
    let colon = rest.find(':')?;
    between(&rest[colon..], "\"", "\"")
}

fn app(name: String, version: String, publisher: &str, location: PathBuf, uninstall: String, source: Source) -> App {
    App {
        name,
        version,
        publisher: publisher.to_string(),
        install_date: String::new(),
        size_kb: 0,
        location: Some(location),
        location_guessed: false,
        uninstall: Some(uninstall),
        quiet_uninstall: None,
        msi: false,
        hive: Hive::Hkcu,
        view: View::V64,
        key_name: String::new(),
        installer: Installer::Unknown,
        source,
        icon: None,
    }
}

fn scoop(root: &Path, global: bool) -> Vec<App> {
    let Ok(rd) = std::fs::read_dir(root.join("apps")) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let manifest = std::fs::read_to_string(e.path().join("current").join("manifest.json")).unwrap_or_default();
            let version = json_string(&manifest, "version").unwrap_or("").to_string();
            let flag = if global { " --global" } else { "" };
            let cmd = format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -Command \"scoop uninstall {name}{flag}\"");
            let mut a = app(name.clone(), version, "Scoop", e.path(), cmd, Source::Scoop { name: name.clone(), global });
            a.key_name = name;
            a
        })
        .collect()
}

fn choco(root: &Path) -> Vec<App> {
    let Ok(rd) = std::fs::read_dir(root.join("lib")) else { return Vec::new() };
    let exe = root.join("bin").join("choco.exe");
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().into_owned();
            if id.eq_ignore_ascii_case("chocolatey") || id.to_lowercase().starts_with("chocolatey-") {
                return None;
            }
            let nuspec = std::fs::read_to_string(e.path().join(format!("{id}.nuspec"))).unwrap_or_default();
            let title = between(&nuspec, "<title>", "</title>").filter(|t| !t.is_empty()).unwrap_or(&id).to_string();
            let version = between(&nuspec, "<version>", "</version>").unwrap_or("").to_string();
            let authors = between(&nuspec, "<authors>", "</authors>").unwrap_or("").to_string();
            let cmd = format!("\"{}\" uninstall {id} -y", exe.display());
            let mut a = app(title, version, &authors, e.path(), cmd, Source::Chocolatey { name: id.clone() });
            a.key_name = id;
            Some(a)
        })
        .collect()
}

pub fn load() -> Vec<App> {
    let mut v: Vec<App> = scoop_roots().iter().flat_map(|(r, g)| scoop(r, *g)).collect();
    v.extend(choco_root().map(|r| choco(&r)).unwrap_or_default());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_manifest_fields() {
        let j = r#"{ "homepage": "x", "version": "1.2.3", "license": "MIT" }"#;
        assert_eq!(json_string(j, "version"), Some("1.2.3"));
        let n = "<metadata><id>git</id><version>2.45.0</version><title>Git</title></metadata>";
        assert_eq!(between(n, "<version>", "</version>"), Some("2.45.0"));
    }
}
