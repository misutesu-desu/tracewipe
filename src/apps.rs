//! Reads installed programs from the Uninstall keys.

use crate::reg::{Hive, Key, View};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const UNINSTALL: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

#[derive(Clone, Debug)]
pub struct App {
    pub name: String,
    pub version: String,
    pub publisher: String,
    pub install_date: String,
    /// In KB. 0 means unknown.
    pub size_kb: u64,
    pub location: Option<PathBuf>,
    /// The entry had no location, so it was derived from the uninstaller path.
    pub location_guessed: bool,
    pub uninstall: Option<String>,
    pub quiet_uninstall: Option<String>,
    pub msi: bool,
    pub hive: Hive,
    pub view: View,
    /// Name of the key under Uninstall.
    pub key_name: String,
}

impl App {
    pub fn key_path(&self) -> String {
        format!("{UNINSTALL}\\{}", self.key_name)
    }

    pub fn scope(&self) -> &'static str {
        match (self.hive, self.view) {
            (Hive::Hkcu, _) => "User",
            (Hive::Hklm, View::V64) => "64-bit",
            (Hive::Hklm, View::V32) => "32-bit",
        }
    }

    /// Command to run. MSI entries use /X instead of /I.
    pub fn command(&self, quiet: bool) -> Option<String> {
        if self.msi && is_guid(&self.key_name) {
            let mode = if quiet { " /passive /norestart" } else { "" };
            return Some(format!("MsiExec.exe /X{}{}", self.key_name, mode));
        }
        if quiet {
            if let Some(q) = &self.quiet_uninstall {
                return Some(q.clone());
            }
        }
        self.uninstall.clone()
    }
}

pub fn is_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 38 && b[0] == b'{' && b[37] == b'}' && b[1..37].iter().all(|c| c.is_ascii_hexdigit() || *c == b'-')
}

pub fn load() -> Vec<App> {
    let sources = [
        (Hive::Hklm, View::V64),
        (Hive::Hklm, View::V32),
        (Hive::Hkcu, View::V64),
    ];
    let mut out: Vec<App> = Vec::new();
    let mut seen = HashSet::new();
    for (hive, view) in sources {
        let Some(root) = Key::open(hive, view, UNINSTALL) else { continue };
        for name in root.subkeys() {
            let Some(k) = Key::open(hive, view, &format!("{UNINSTALL}\\{name}")) else { continue };
            if let Some(app) = read(&k, hive, view, name) {
                let id = (app.name.to_lowercase(), app.version.clone(), app.key_name.to_lowercase());
                if seen.insert(id) {
                    out.push(app);
                }
            }
        }
    }
    out.sort_by_cached_key(|a| a.name.to_lowercase());
    out
}

fn read(k: &Key, hive: Hive, view: View, key_name: String) -> Option<App> {
    let name = k.string("DisplayName")?;
    if k.dword("SystemComponent") == Some(1) || k.string("ParentKeyName").is_some() {
        return None;
    }
    if let Some(rt) = k.string("ReleaseType") {
        let rt = rt.to_lowercase();
        if rt.contains("update") || rt.contains("hotfix") {
            return None;
        }
    }
    let msi = k.dword("WindowsInstaller") == Some(1);
    let uninstall = k.string("UninstallString");
    if uninstall.is_none() && !msi {
        return None;
    }
    let quiet_uninstall = k.string("QuietUninstallString");

    let mut location = k.string("InstallLocation").map(|s| PathBuf::from(s.trim_matches('"').trim_end_matches('\\')));
    let mut location_guessed = false;
    if location.as_ref().is_none_or(|p| p.as_os_str().is_empty()) {
        location = uninstall
            .as_deref()
            .and_then(|c| split_command(c).map(|(exe, _)| exe))
            .and_then(|exe| guess_location(Path::new(&exe)));
        location_guessed = location.is_some();
    }

    Some(App {
        name,
        version: k.string("DisplayVersion").unwrap_or_default(),
        publisher: k.string("Publisher").unwrap_or_default(),
        install_date: k.string("InstallDate").map(|d| format_date(&d)).unwrap_or_default(),
        size_kb: k.dword("EstimatedSize").unwrap_or(0) as u64,
        location,
        location_guessed,
        uninstall,
        quiet_uninstall,
        msi,
        hive,
        view,
        key_name,
    })
}

fn format_date(d: &str) -> String {
    if d.len() == 8 && d.bytes().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..8])
    } else {
        d.to_string()
    }
}

/// Returns the uninstaller's folder when it looks like the program's own folder.
fn guess_location(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let low = dir.to_string_lossy().to_lowercase();
    let bad = ["\\windows", "package cache", "\\installer", "\\temp", "{"];
    if bad.iter().any(|b| low.contains(b)) || crate::leftovers::is_protected(dir) {
        return None;
    }
    dir.exists().then(|| dir.to_path_buf())
}

/// Splits a command line into program and arguments. Handles unquoted paths with spaces.
pub fn split_command(cmd: &str) -> Option<(String, String)> {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some((rest[..end].to_string(), rest[end + 1..].trim().to_string()));
    }
    let low = cmd.to_ascii_lowercase();
    if let Some(i) = low.find(".exe") {
        let end = i + 4;
        if end == cmd.len() || cmd.as_bytes()[end] == b' ' {
            return Some((cmd[..end].to_string(), cmd[end..].trim().to_string()));
        }
    }
    let mut it = cmd.splitn(2, ' ');
    Some((it.next()?.to_string(), it.next().unwrap_or("").trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_commands() {
        assert_eq!(
            split_command(r#""C:\Program Files\A B\unins000.exe" /SILENT"#),
            Some((r"C:\Program Files\A B\unins000.exe".into(), "/SILENT".into()))
        );
        assert_eq!(
            split_command(r"C:\Program Files\X\uninst.exe /S"),
            Some((r"C:\Program Files\X\uninst.exe".into(), "/S".into()))
        );
        assert_eq!(
            split_command("MsiExec.exe /X{1234}"),
            Some(("MsiExec.exe".into(), "/X{1234}".into()))
        );
    }

    #[test]
    fn guid() {
        assert!(is_guid("{23170F69-40C1-2702-2301-000001000000}"));
        assert!(!is_guid("7-Zip"));
    }
}
