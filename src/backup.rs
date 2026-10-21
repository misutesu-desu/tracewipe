//! The backup folder: everything a cleanup removes, and how to put it back.
//!
//! Each cleanup gets a folder with a `manifest.txt`. Every line is one
//! removed item, tab separated:
//!
//! ```text
//! file      files\3\Foo   C:\Program Files\Foo
//! recycled  D:\Games\Foo
//! reg       004.reg
//! task      005-task.xml  \Foo Updater
//! ```
//!
//! Files on the same drive as the backup folder are moved there, which is
//! instant and lets them come back exactly. Files on other drives go to the
//! Recycle Bin instead.

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const KEEP_DAYS: u64 = 7;

pub fn root() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("Tracewipe").join("backups")
}

fn safe_name(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { '_' }).collect();
    s.trim().chars().take(60).collect()
}

/// A new, empty backup folder for one cleanup.
pub fn create(app_name: &str) -> PathBuf {
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let dir = root().join(format!("{secs}-{}", safe_name(app_name)));
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("name.txt"), app_name);
    dir
}

pub fn note(dir: &Path, fields: &[&str]) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("manifest.txt")) {
        let _ = writeln!(f, "{}", fields.join("\t"));
    }
}

pub enum Stored {
    Moved,
    Recycled,
}

/// Moves a file or folder into the backup, or to the Recycle Bin when it
/// lives on another drive.
pub fn store(dir: &Path, i: usize, p: &Path) -> Result<Stored, String> {
    let name = p.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| "item".into());
    let rel = PathBuf::from("files").join(i.to_string()).join(&name);
    let dest = dir.join(&rel);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    match std::fs::rename(p, &dest) {
        Ok(()) => {
            note(dir, &["file", &rel.to_string_lossy(), &p.to_string_lossy()]);
            Ok(Stored::Moved)
        }
        // ERROR_NOT_SAME_DEVICE
        Err(e) if e.raw_os_error() == Some(17) => {
            crate::remove::recycle(p)?;
            note(dir, &["recycled", &p.to_string_lossy()]);
            Ok(Stored::Recycled)
        }
        Err(e) => Err(match e.raw_os_error() {
            Some(5) => "Access denied, run as administrator".into(),
            Some(32) => "File is in use".into(),
            _ => e.to_string(),
        }),
    }
}

pub struct Backup {
    pub dir: PathBuf,
    pub name: String,
    pub when: SystemTime,
    pub items: usize,
    pub size: u64,
}

pub fn list() -> Vec<Backup> {
    let Ok(rd) = std::fs::read_dir(root()) else { return Vec::new() };
    let mut out: Vec<Backup> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| {
            let dir = e.path();
            let secs: u64 = e.file_name().to_string_lossy().split('-').next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let name = std::fs::read_to_string(dir.join("name.txt")).unwrap_or_else(|_| e.file_name().to_string_lossy().into_owned());
            let items = std::fs::read_to_string(dir.join("manifest.txt")).map(|m| m.lines().count()).unwrap_or(0);
            Backup { size: crate::leftovers::dir_size(&dir), dir, name, when: SystemTime::UNIX_EPOCH + Duration::from_secs(secs), items }
        })
        .collect();
    out.sort_by(|a, b| b.when.cmp(&a.when));
    out
}

fn hidden(cmd: &str) -> Command {
    let mut c = Command::new(cmd);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

/// Puts everything in the backup back. Returns what could not be restored.
/// The backup folder is removed when nothing failed.
pub fn restore(dir: &Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(dir.join("manifest.txt")).unwrap_or_default();
    let mut failed = Vec::new();
    // Files first, so that registry entries and tasks point at something.
    let mut lines: Vec<Vec<&str>> = manifest.lines().map(|l| l.split('\t').collect()).collect();
    lines.sort_by_key(|f| match f.first() {
        Some(&"file") => 0,
        Some(&"reg") => 1,
        _ => 2,
    });
    for f in lines {
        let res: Result<(), String> = match f.as_slice() {
            ["file", rel, orig] => {
                let orig = Path::new(orig);
                if orig.exists() {
                    Err(format!("{} exists again, left the backup copy in place", orig.display()))
                } else {
                    orig.parent().map(std::fs::create_dir_all);
                    std::fs::rename(dir.join(rel), orig).map_err(|e| format!("{}: {e}", orig.display()))
                }
            }
            ["recycled", orig] => Err(format!("{orig} is in the Recycle Bin")),
            ["reg", file] => {
                let ok = hidden("reg.exe").arg("import").arg(dir.join(file)).arg("/reg:64").output().is_ok_and(|o| o.status.success());
                if ok {
                    Ok(())
                } else {
                    Err(format!("Could not import {file}, run as administrator"))
                }
            }
            ["task", file, name] => {
                let ok = hidden("schtasks.exe").args(["/Create", "/TN", name, "/XML"]).arg(dir.join(file)).arg("/F").output().is_ok_and(|o| o.status.success());
                if ok {
                    Ok(())
                } else {
                    Err(format!("Could not recreate the task {name}"))
                }
            }
            _ => Ok(()),
        };
        if let Err(e) = res {
            failed.push(e);
        }
    }
    if failed.is_empty() {
        let _ = std::fs::remove_dir_all(dir);
    }
    failed
}

/// Deletes a backup for good.
pub fn discard(dir: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(dir)
}

/// Deletes backups older than `KEEP_DAYS`.
pub fn purge_old() {
    let limit = Duration::from_secs(KEEP_DAYS * 24 * 3600);
    for b in list() {
        if b.when.elapsed().is_ok_and(|age| age > limit) {
            let _ = discard(&b.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::leftovers::{Confidence, Kind, Leftover};
    use crate::reg::{self, Hive, View};

    #[test]
    fn clean_then_restore_round_trip() {
        let tmp = std::env::temp_dir().join(format!("tracewipe-rt-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub").join("a.txt"), "hello").unwrap();
        let key = format!("SOFTWARE\\TracewipeTest{}", std::process::id());
        std::process::Command::new("reg.exe").args(["add", &format!("HKCU\\{key}"), "/v", "x", "/d", "1", "/f"]).output().unwrap();

        let items = vec![
            Leftover::new(Kind::Path { path: tmp.clone(), dir: true }, Confidence::High, ""),
            Leftover::new(Kind::Value { hive: Hive::Hkcu, view: View::V64, path: key.clone(), name: "x".into() }, Confidence::High, ""),
        ];
        let report = crate::remove::clean("round trip test", &items);
        assert_eq!(report.removed, 2, "{:?}", report.failed.iter().map(|f| &f.reason).collect::<Vec<_>>());
        assert!(!tmp.exists());
        assert!(reg::Key::open(Hive::Hkcu, View::V64, &key).unwrap().get("x").is_none());

        let failed = super::restore(&report.backup);
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(std::fs::read_to_string(tmp.join("sub").join("a.txt")).unwrap(), "hello");
        assert_eq!(reg::Key::open(Hive::Hkcu, View::V64, &key).unwrap().string("x").as_deref(), Some("1"));
        assert!(!report.backup.exists());

        let _ = std::fs::remove_dir_all(&tmp);
        reg::delete_tree(Hive::Hkcu, View::V64, &key).unwrap();
    }
}
