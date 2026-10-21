//! Reads installed programs from the Uninstall keys.

use crate::reg::{Hive, Key, View};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const UNINSTALL: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

/// What built the uninstaller. Decides which switches make it quiet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installer {
    Msi,
    Inno,
    Nsis,
    Squirrel,
    Unknown,
}

impl Installer {
    pub fn label(self) -> &'static str {
        match self {
            Installer::Msi => "Windows Installer",
            Installer::Inno => "Inno Setup",
            Installer::Nsis => "NSIS",
            Installer::Squirrel => "Squirrel",
            Installer::Unknown => "",
        }
    }
}

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
    pub installer: Installer,
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

    /// Command to run. MSI entries use /X instead of /I. A quiet command comes
    /// from the entry when it has one, otherwise from the installer type.
    pub fn command(&self, quiet: bool) -> Option<String> {
        if self.msi && is_guid(&self.key_name) {
            let mode = if quiet { " /qn /norestart" } else { "" };
            return Some(format!("MsiExec.exe /X{}{}", self.key_name, mode));
        }
        let cmd = self.uninstall.clone()?;
        if !quiet {
            return Some(cmd);
        }
        if let Some(q) = &self.quiet_uninstall {
            return Some(q.clone());
        }
        let low = cmd.to_lowercase();
        let extra = match self.installer {
            Installer::Inno if !low.contains("/verysilent") => " /VERYSILENT /SUPPRESSMSGBOXES /NORESTART",
            Installer::Nsis if !low.split_whitespace().any(|a| a == "/s") => " /S",
            Installer::Squirrel if low.contains("--uninstall") && !low.contains(" -s") => " -s",
            _ => "",
        };
        Some(format!("{cmd}{extra}"))
    }

    pub fn can_be_quiet(&self) -> bool {
        self.command(true) != self.command(false)
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

    let installer = guess_installer(msi, &key_name, uninstall.as_deref());
    Some(App {
        installer,
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

/// Cheap guess from names alone. NSIS needs a look inside the file, see `detect_installer`.
fn guess_installer(msi: bool, key_name: &str, cmd: Option<&str>) -> Installer {
    if msi {
        return Installer::Msi;
    }
    let Some((exe, args)) = cmd.and_then(split_command) else { return Installer::Unknown };
    let file = Path::new(&exe).file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
    if key_name.ends_with("_is1") || (file.starts_with("unins") && file.ends_with(".exe") && file.len() <= 12) {
        Installer::Inno
    } else if file == "update.exe" && args.contains("--uninstall") {
        Installer::Squirrel
    } else {
        Installer::Unknown
    }
}

/// Reads the start of the uninstaller to find the NSIS stub's manifest name.
pub fn detect_installer(app: &App) -> Installer {
    if app.installer != Installer::Unknown {
        return app.installer;
    }
    let Some((exe, _)) = app.uninstall.as_deref().and_then(split_command) else { return Installer::Unknown };
    use std::io::Read;
    let mut buf = vec![0u8; 512 * 1024];
    let n = std::fs::File::open(&exe).and_then(|mut f| f.read(&mut buf)).unwrap_or(0);
    let hay = &buf[..n];
    if hay.windows(8).any(|w| w == b"Nullsoft") {
        Installer::Nsis
    } else {
        Installer::Unknown
    }
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
    fn quiet_switches() {
        let mut a = App {
            name: "Foo".into(),
            version: String::new(),
            publisher: String::new(),
            install_date: String::new(),
            size_kb: 0,
            location: None,
            location_guessed: false,
            uninstall: Some(r#""C:\Program Files\Foo\unins000.exe""#.into()),
            quiet_uninstall: None,
            msi: false,
            hive: Hive::Hklm,
            view: View::V64,
            key_name: "Foo_is1".into(),
            installer: Installer::Unknown,
        };
        a.installer = guess_installer(a.msi, &a.key_name, a.uninstall.as_deref());
        assert_eq!(a.installer, Installer::Inno);
        assert!(a.command(true).unwrap().ends_with("/VERYSILENT /SUPPRESSMSGBOXES /NORESTART"));
        a.installer = Installer::Nsis;
        assert!(a.command(true).unwrap().ends_with(" /S"));
        a.quiet_uninstall = Some("x /quiet".into());
        assert_eq!(a.command(true).as_deref(), Some("x /quiet"));
        assert!(!a.command(false).unwrap().contains("/S"));
    }

    #[test]
    fn guid() {
        assert!(is_guid("{23170F69-40C1-2702-2301-000001000000}"));
        assert!(!is_guid("7-Zip"));
    }
}
