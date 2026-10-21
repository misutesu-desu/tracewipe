//! System-wide traces: services, firewall rules, COM classes, file types,
//! PATH entries and the caches Windows keeps about executables.
//!
//! Almost every check here asks one question: does this entry point into
//! the program's install folder? That is a strong signal with few false
//! positives, so name matches alone only produce unchecked items.

use crate::leftovers::{Confidence, Kind, Leftover, Matcher};
use crate::reg::{self, Hive, Key, Value, View};
use rayon::prelude::*;
use std::path::PathBuf;

pub(crate) fn scan(m: &Matcher) -> Vec<Leftover> {
    let jobs: [fn(&Matcher) -> Vec<Leftover>; 7] = [services, firewall, classes, path_entries, tracing, crash_reports, shell_caches];
    jobs.par_iter().flat_map_iter(|f| f(m)).collect()
}

fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => reg::expand_env(s).to_lowercase(),
        _ => String::new(),
    }
}

const SERVICES: &str = "SYSTEM\\CurrentControlSet\\Services";

fn services(m: &Matcher) -> Vec<Leftover> {
    let Some(root) = Key::open(Hive::Hklm, View::V64, SERVICES) else { return Vec::new() };
    root.subkeys()
        .into_par_iter()
        .filter_map(|name| {
            let k = Key::open(Hive::Hklm, View::V64, &format!("{SERVICES}\\{name}"))?;
            let image = k.get("ImagePath").map(|v| text(&v)).unwrap_or_default();
            let image = image.trim_start_matches("\\??\\").to_string();
            if m.mentions_location(&image) {
                Some(Leftover::new(Kind::Service { name }, m.mention_confidence(), "Service runs from the install folder"))
            } else if m.name_hit(&name).is_some() {
                Some(Leftover::new(Kind::Service { name }, Confidence::Medium, "Service with the program's name"))
            } else {
                None
            }
        })
        .collect()
}

const FIREWALL: &str = "SYSTEM\\CurrentControlSet\\Services\\SharedAccess\\Parameters\\FirewallPolicy\\FirewallRules";

/// Reads one field of a rule such as `v2.31|Action=Allow|App=C:\x.exe|Name=X|`.
fn rule_field<'a>(rule: &'a str, field: &str) -> Option<&'a str> {
    rule.split('|').find_map(|p| p.split_once('=').filter(|(k, _)| k.eq_ignore_ascii_case(field)).map(|(_, v)| v))
}

fn firewall(m: &Matcher) -> Vec<Leftover> {
    let Some(k) = Key::open(Hive::Hklm, View::V64, FIREWALL) else { return Vec::new() };
    let mut out = Vec::new();
    for (id, v) in k.values() {
        let Value::Str(rule) = v else { continue };
        let app = rule_field(&rule, "App").map(|a| reg::expand_env(a).to_lowercase()).unwrap_or_default();
        let name = rule_field(&rule, "Name").unwrap_or(&id).to_string();
        let hit = if m.mentions_location(&app) {
            Some((m.mention_confidence(), "Firewall rule for a program in the install folder"))
        } else if !name.starts_with('@') && m.name_hit(&name).is_some() {
            Some((Confidence::Medium, "Firewall rule with the program's name"))
        } else {
            None
        };
        if let Some((c, reason)) = hit {
            out.push(Leftover::new(Kind::Firewall { id, name }, c, reason));
        }
    }
    out
}

/// Class keys that are not file types or ProgIDs and are far too large to walk.
const CLASS_SKIP: &[&str] = &["clsid", "interface", "typelib", "appid", "wow6432node", "local settings", "*", "directory", "folder", "drive"];

fn classes(m: &Matcher) -> Vec<Leftover> {
    if m.location.is_none() {
        return Vec::new();
    }
    let roots = [(Hive::Hkcu, View::V64, true), (Hive::Hklm, View::V64, true), (Hive::Hklm, View::V32, false)];
    roots
        .par_iter()
        .flat_map_iter(|&(hive, view, progids)| {
            let base = "SOFTWARE\\Classes";
            let mut out = Vec::new();

            let clsid = format!("{base}\\CLSID");
            if let Some(k) = Key::open(hive, view, &clsid) {
                let found: Vec<Leftover> = k
                    .subkeys()
                    .into_par_iter()
                    .filter_map(|id| {
                        let path = format!("{clsid}\\{id}");
                        let hit = ["InprocServer32", "LocalServer32"].iter().any(|s| {
                            Key::open(hive, view, &format!("{path}\\{s}"))
                                .and_then(|sk| sk.get(""))
                                .is_some_and(|v| m.mentions_location(&text(&v)))
                        });
                        hit.then(|| Leftover::new(Kind::Key { hive, view, path }, m.mention_confidence(), "COM class served from the install folder"))
                    })
                    .collect();
                out.extend(found);
            }

            if progids {
                if let Some(k) = Key::open(hive, view, base) {
                    let found: Vec<Leftover> = k
                        .subkeys()
                        .into_par_iter()
                        .filter(|n| !n.starts_with('.') && !CLASS_SKIP.contains(&n.to_lowercase().as_str()))
                        .filter_map(|n| {
                            let path = format!("{base}\\{n}");
                            let cmd = Key::open(hive, view, &format!("{path}\\shell\\open\\command"))?.get("")?;
                            m.mentions_location(&text(&cmd))
                                .then(|| Leftover::new(Kind::Key { hive, view, path }, m.mention_confidence(), "File type opens with the program"))
                        })
                        .collect();
                    out.extend(found);
                }
            }
            out
        })
        .collect()
}

pub(crate) const ENV_KEYS: [(Hive, &str); 2] = [
    (Hive::Hkcu, "Environment"),
    (Hive::Hklm, "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment"),
];

fn path_entries(m: &Matcher) -> Vec<Leftover> {
    let mut out = Vec::new();
    for (hive, key) in ENV_KEYS {
        let Some(k) = Key::open(hive, View::V64, key) else { continue };
        let Some(Value::Str(path)) = k.get("Path") else { continue };
        for entry in path.split(';').filter(|e| !e.trim().is_empty()) {
            let full = reg::expand_env(entry.trim()).to_lowercase();
            if m.mentions_location(full.trim_end_matches('\\')) {
                out.push(Leftover::new(
                    Kind::PathEntry { hive, entry: entry.to_string() },
                    m.mention_confidence(),
                    "PATH entry inside the install folder",
                ));
            }
        }
    }
    out
}

/// Executable names too common to identify a program on their own.
const COMMON_EXE: &[&str] = &["setup", "install", "installer", "update", "updater", "helper", "launcher", "uninstall", "crashpad_handler", "crashreporter", "elevate", "service", "notification_helper"];

fn exe_hit(m: &Matcher, exe: &str) -> bool {
    let stem = exe.trim_end_matches(".exe").to_lowercase();
    if COMMON_EXE.contains(&stem.as_str()) || stem.starts_with("unins") || stem.len() < 3 {
        return false;
    }
    m.exe_stems.contains(&stem) || m.name_hit(&stem) == Some(Confidence::High)
}

fn tracing(m: &Matcher) -> Vec<Leftover> {
    let mut out = Vec::new();
    for view in [View::V64, View::V32] {
        let base = "SOFTWARE\\Microsoft\\Tracing";
        let Some(k) = Key::open(Hive::Hklm, view, base) else { continue };
        for sub in k.subkeys() {
            let Some((exe, suffix)) = sub.rsplit_once('_') else { continue };
            if !suffix.eq_ignore_ascii_case("RASAPI32") && !suffix.eq_ignore_ascii_case("RASMANCS") {
                continue;
            }
            if exe_hit(m, exe) {
                out.push(Leftover::new(
                    Kind::Key { hive: Hive::Hklm, view, path: format!("{base}\\{sub}") },
                    m.mention_confidence(),
                    "Network tracing entry for the program's executable",
                ));
            }
        }
    }
    out
}

fn crash_reports(m: &Matcher) -> Vec<Leftover> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for base in [std::env::var_os("ProgramData"), std::env::var_os("LOCALAPPDATA")].into_iter().flatten() {
        let wer = PathBuf::from(base).join("Microsoft\\Windows\\WER");
        dirs.push(wer.join("ReportArchive"));
        dirs.push(wer.join("ReportQueue"));
    }
    let mut out = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            // AppCrash_foo.exe_0123abcd_..._cab_...
            let exe = name.split('_').nth(1).unwrap_or("");
            if exe.ends_with(".exe") && exe_hit(m, exe) {
                out.push(Leftover::new(Kind::Path { path: e.path(), dir: true }, m.mention_confidence(), "Crash report for the program"));
            }
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        if let Ok(rd) = std::fs::read_dir(PathBuf::from(local).join("CrashDumps")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                // foo.exe.1234.dmp
                if let Some(i) = name.find(".exe.") {
                    if name.ends_with(".dmp") && exe_hit(m, &name[..i + 4]) {
                        out.push(Leftover::new(Kind::Path { path: e.path(), dir: false }, m.mention_confidence(), "Crash dump of the program"));
                    }
                }
            }
        }
    }
    out
}

/// Caches whose value names are executable paths.
fn shell_caches(m: &Matcher) -> Vec<Leftover> {
    if m.location.is_none() {
        return Vec::new();
    }
    let keys = [
        (Hive::Hkcu, "SOFTWARE\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\Shell\\MuiCache", "Shell cache entry for the program"),
        (Hive::Hkcu, "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\AppCompatFlags\\Layers", "Compatibility setting for the program"),
        (Hive::Hklm, "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\AppCompatFlags\\Layers", "Compatibility setting for the program"),
        (Hive::Hkcu, "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion\\AppCompatFlags\\Compatibility Assistant\\Store", "Compatibility record for the program"),
    ];
    let mut out = Vec::new();
    for (hive, path, reason) in keys {
        let Some(k) = Key::open(hive, View::V64, path) else { continue };
        for (name, _) in k.values() {
            if m.mentions_location(&name.to_lowercase()) {
                out.push(Leftover::new(Kind::Value { hive, view: View::V64, path: path.to_string(), name }, m.mention_confidence(), reason));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rule_fields() {
        let r = "v2.31|Action=Allow|Active=TRUE|Dir=In|App=C:\\Program Files\\Foo\\foo.exe|Name=Foo Server|";
        assert_eq!(rule_field(r, "app"), Some("C:\\Program Files\\Foo\\foo.exe"));
        assert_eq!(rule_field(r, "Name"), Some("Foo Server"));
        assert_eq!(rule_field(r, "Profile"), None);
    }
}
