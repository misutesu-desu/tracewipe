//! Finds files, registry entries and tasks left behind after an uninstall.
//!
//! Matching is deliberately conservative: a folder or key name must equal
//! the program's name, or its content must point into the install folder.
//! Similar names and traces shared with another program come unchecked.

use crate::apps::{is_guid, App};
use crate::reg::{self, Hive, Key, Value, View};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Path { path: PathBuf, dir: bool },
    Key { hive: Hive, view: View, path: String },
    Value { hive: Hive, view: View, path: String, name: String },
    Task { name: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    High,
    Medium,
}

#[derive(Clone, Debug)]
pub struct Leftover {
    pub kind: Kind,
    pub confidence: Confidence,
    pub reason: &'static str,
    pub size: u64,
    pub checked: bool,
}

impl Leftover {
    fn new(kind: Kind, confidence: Confidence, reason: &'static str) -> Self {
        Leftover { kind, confidence, reason, size: 0, checked: confidence == Confidence::High }
    }

    pub fn group(&self) -> u8 {
        match self.kind {
            Kind::Path { .. } => 0,
            Kind::Key { .. } | Kind::Value { .. } => 1,
            Kind::Task { .. } => 2,
        }
    }

    pub fn label(&self) -> String {
        match &self.kind {
            Kind::Path { path, .. } => path.display().to_string(),
            Kind::Key { hive, view, path } => reg::display_path(*hive, *view, path),
            Kind::Value { hive, view, path, name } => format!("{} : {}", reg::display_path(*hive, *view, path), name),
            Kind::Task { name } => name.clone(),
        }
    }

    fn dedupe_key(&self) -> String {
        self.label().to_lowercase()
    }
}

// ---------------------------------------------------------------- names

const GENERIC: &[&str] = &[
    "microsoft", "windows", "commonfiles", "temp", "tmp", "programs", "packages", "packagecache", "google",
    "apple", "update", "updater", "app", "apps", "application", "applications", "data", "cache", "caches",
    "software", "system", "system32", "user", "users", "default", "installer", "installers", "setup", "launcher",
    "tools", "tool", "runtime", "runtimes", "driver", "drivers", "service", "services", "classes", "policies",
    "wow6432node", "programfiles", "desktop", "documents", "downloads", "music", "pictures", "videos", "public",
    "local", "locallow", "roaming", "appdata", "programdata", "logs", "log", "config", "settings", "plugins",
    "bin", "lib", "share", "shared", "x64", "x86", "win64", "win32", "uninstall", "uninstaller", "startmenu",
    "startup", "accessories", "administrativetools", "maintenance", "systemtools", "crashpad", "crashdumps",
    "crashreports", "d3dscache", "history", "cookies", "identities", "credentials", "protect", "crypto",
    "systemcertificates", "vault", "virtualstore", "clients", "registeredapplications", "intel", "nvidia",
    "amd", "realtek", "oem", "node", "java", "client", "server", "helper", "core", "common", "player", "studio",
    "browser", "editor", "viewer", "manager", "center", "centre", "suite", "pro", "free", "lite", "beta",
    "portable", "web", "online", "assistant", "agent", "sdk", "toolkit", "library", "framework", "engine",
    "host", "redistributable", "components", "extensions", "addons", "addin", "updates", "version", "versions",
    "community", "professional", "enterprise", "edition", "standard", "basic", "home", "office", "games", "game",
];

const COMPANY_SUFFIX: &[&str] = &[
    "inc", "llc", "ltd", "limited", "corp", "corporation", "co", "gmbh", "ag", "sa", "srl", "bv", "pty", "plc",
    "company", "the", "as", "ab", "oy", "kg", "sl", "spa", "sas", "ltda", "kft", "sro",
];

/// Splits into lowercase words, dropping versions, architecture tags and anything in brackets.
pub fn words(s: &str) -> Vec<String> {
    let mut clean = String::with_capacity(s.len());
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = (depth - 1).max(0),
            '®' | '™' | '©' => {}
            _ if depth == 0 => clean.push(c),
            _ => {}
        }
    }
    let arch = ["x64", "x86", "64-bit", "32-bit", "amd64", "arm64", "win64", "win32", "64bit", "32bit"];
    let mut out = Vec::new();
    for tok in clean.split_whitespace() {
        let t = tok.to_lowercase();
        if arch.contains(&t.as_str()) || is_version(&t) {
            continue;
        }
        for w in t.split(|c: char| !c.is_alphanumeric()) {
            if !w.is_empty() {
                out.push(w.to_string());
            }
        }
    }
    out
}

fn is_version(t: &str) -> bool {
    let t = t.strip_prefix('v').unwrap_or(t);
    let Some(first) = t.chars().next() else { return false };
    first.is_ascii_digit()
        && (t.bytes().all(|b| b.is_ascii_digit()) || (t.contains('.') && t.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))))
}

pub fn compact(s: &str) -> String {
    words(s).concat()
}

/// Lowercase letters and digits only, versions kept.
fn raw_compact(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Version-like words in a name, without a leading v.
fn version_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .map(|t| t.to_lowercase())
        .filter(|t| is_version(t))
        .map(|t| t.trim_start_matches('v').to_string())
        .collect()
}

fn app_versions(app: &App) -> Vec<String> {
    let mut v = version_tokens(&app.name);
    if !app.version.is_empty() && is_version(&app.version.to_lowercase()) {
        v.push(app.version.to_lowercase().trim_start_matches('v').to_string());
    }
    let mm: Vec<String> = v.iter().filter_map(|s| {
        let parts: Vec<&str> = s.split('.').collect();
        (parts.len() > 2).then(|| format!("{}.{}", parts[0], parts[1]))
    }).collect();
    v.extend(mm);
    v.sort();
    v.dedup();
    v
}

fn usable(v: &str) -> bool {
    v.chars().count() >= 3 && v.chars().any(|c| c.is_alphabetic()) && !GENERIC.contains(&v) && !has_hex_id(v)
}

/// Names that carry an id, like a GUID or a version folder.
/// Looks for a hex run of 8 or more characters with more than 3 digits.
fn has_hex_id(v: &str) -> bool {
    let (mut run, mut digits) = (0, 0);
    for c in v.chars() {
        if c.is_ascii_hexdigit() {
            run += 1;
            digits += c.is_ascii_digit() as u32;
            if run >= 8 && digits > 3 {
                return true;
            }
        } else {
            (run, digits) = (0, 0);
        }
    }
    false
}

fn publisher_words(p: &str) -> Vec<String> {
    words(p).into_iter().filter(|w| !COMPANY_SUFFIX.contains(&w.as_str())).collect()
}

/// Forms the program's name can take as a folder or key name.
pub fn name_variants(app: &App) -> Vec<String> {
    let mut v = Vec::new();
    let w = words(&app.name);
    v.push(w.concat());
    let pw = publisher_words(&app.publisher);
    if !pw.is_empty() && w.len() > pw.len() && w[..pw.len()] == pw[..] {
        v.push(w[pw.len()..].concat());
    }
    if let (Some(p0), true) = (pw.first(), w.len() > 1) {
        if &w[0] == p0 {
            v.push(w[1..].concat());
        }
    }
    if let Some(loc) = &app.location {
        if !app.location_guessed {
            if let Some(n) = loc.file_name() {
                v.push(compact(&n.to_string_lossy()));
            }
        }
    }
    if !is_guid(&app.key_name) {
        let k = app.key_name.trim_end_matches("_is1");
        v.push(compact(k));
    }
    v.retain(|x| usable(x));
    v.sort();
    v.dedup();
    v
}

pub fn publisher_variants(app: &App) -> Vec<String> {
    let pw = publisher_words(&app.publisher);
    let mut v = Vec::new();
    if !pw.is_empty() {
        v.push(pw.concat());
        if pw[0].chars().count() >= 4 {
            v.push(pw[0].clone());
        }
    }
    v.retain(|x| x.chars().count() >= 3 && x.chars().any(|c| c.is_alphabetic()));
    v.sort();
    v.dedup();
    v
}

// ---------------------------------------------------------------- protected paths

fn env(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn lower(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

struct Places {
    file_roots: Vec<PathBuf>,
    /// Root, and whether it is a Start menu. Desktop folders are never matched by name.
    shortcut_roots: Vec<(PathBuf, bool)>,
    tasks: Option<PathBuf>,
    protected: Vec<String>,
    windir: String,
}

fn places() -> &'static Places {
    static P: OnceLock<Places> = OnceLock::new();
    P.get_or_init(|| {
        let pf = env("ProgramW6432").or_else(|| env("ProgramFiles"));
        let pf86 = env("ProgramFiles(x86)");
        let pd = env("ProgramData");
        let roaming = env("APPDATA");
        let local = env("LOCALAPPDATA");
        let home = env("USERPROFILE");
        let public = env("PUBLIC");
        let windir = env("SystemRoot").or_else(|| env("windir")).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));

        let mut file_roots: Vec<PathBuf> = Vec::new();
        file_roots.extend(pf.clone());
        file_roots.extend(pf86.clone());
        file_roots.extend(pd.clone());
        file_roots.extend(roaming.clone());
        file_roots.extend(local.clone());
        file_roots.extend(local.as_ref().map(|l| l.join("Programs")));
        file_roots.extend(home.as_ref().map(|h| h.join("AppData\\LocalLow")));
        file_roots.sort();
        file_roots.dedup();

        let mut shortcut_roots = Vec::new();
        shortcut_roots.extend(roaming.as_ref().map(|r| (r.join("Microsoft\\Windows\\Start Menu\\Programs"), true)));
        shortcut_roots.extend(pd.as_ref().map(|r| (r.join("Microsoft\\Windows\\Start Menu\\Programs"), true)));
        shortcut_roots.extend(home.as_ref().map(|h| (h.join("Desktop"), false)));
        shortcut_roots.extend(public.as_ref().map(|h| (h.join("Desktop"), false)));

        let mut protected: Vec<String> = Vec::new();
        for r in file_roots.iter().chain(shortcut_roots.iter().map(|(r, _)| r)) {
            let l = lower(r);
            for sub in ["", "\\microsoft", "\\packages", "\\temp", "\\programs", "\\common files", "\\windowsapps"] {
                protected.push(format!("{l}{sub}"));
            }
        }
        for p in [&home, &public, &env("CommonProgramFiles"), &env("CommonProgramFiles(x86)"), &env("CommonProgramW6432")]
            .into_iter()
            .flatten()
        {
            protected.push(lower(p));
        }
        protected.push(lower(&windir));

        Places {
            tasks: Some(windir.join("System32\\Tasks")).filter(|p| p.is_dir()),
            file_roots,
            shortcut_roots,
            protected,
            windir: lower(&windir),
        }
    })
}

/// Paths whose removal could break Windows or other programs.
pub fn is_protected(p: &Path) -> bool {
    let s = lower(p);
    if s.len() <= 3 || !s.contains('\\') {
        return true;
    }
    let pl = places();
    if s == pl.windir || s.starts_with(&format!("{}\\", pl.windir)) {
        return true;
    }
    pl.protected.iter().any(|r| *r == s || r.starts_with(&format!("{s}\\")))
}

// ---------------------------------------------------------------- matcher

struct Matcher {
    names: Vec<String>,
    /// Versions from the name and version fields, with their major.minor forms.
    versions: Vec<String>,
    publishers: Vec<String>,
    location: Option<String>,
    other_names: HashSet<String>,
    other_publishers: HashSet<String>,
    other_locations: Vec<String>,
    /// Another program's folder sits inside the install folder.
    nested_others: bool,
}

impl Matcher {
    fn new(app: &App, others: &[App]) -> Self {
        let names = name_variants(app);
        let own = app.location.as_ref().map(|p| lower(p));
        // When two programs point at the same folder, the one named in the path owns it.
        // Thorium and the WhatsApp Web app it installs are such a pair.
        let other_locations: Vec<String> = others
            .iter()
            .filter_map(|o| {
                let l = lower(o.location.as_ref()?);
                if own.as_deref() == Some(l.as_str()) {
                    let parts: Vec<String> = l.split('\\').map(compact).collect();
                    let ours = names.iter().any(|n| parts.contains(n));
                    let theirs = name_variants(o).iter().any(|n| parts.contains(n));
                    if ours && !theirs {
                        return None;
                    }
                }
                Some(l)
            })
            .collect();
        // A location inside, or equal to, another program's folder is not ours to report.
        let location = own
            .filter(|s| s.len() > 6 && !is_protected(Path::new(s)))
            .filter(|s| !other_locations.iter().any(|o| o == s || s.starts_with(&format!("{o}\\"))));
        let nested_others = location.as_ref().is_some_and(|s| other_locations.iter().any(|o| o.starts_with(&format!("{s}\\"))));
        Matcher {
            names,
            versions: app_versions(app),
            publishers: publisher_variants(app),
            location,
            other_names: others.iter().flat_map(name_variants).collect(),
            other_publishers: others.iter().flat_map(publisher_variants).collect(),
            nested_others,
            other_locations,
        }
    }

    /// A name that carries a version, like "Python 3.11" or "Python311",
    /// only matches when the version is this program's version.
    fn name_hit(&self, entry: &str) -> Option<Confidence> {
        let c = compact(entry);
        if self.names.contains(&c) {
            if let Some(v) = version_tokens(entry).first() {
                if self.versions.is_empty() {
                    return Some(Confidence::Medium);
                }
                return self.version_matches(v).then_some(Confidence::High);
            }
            return Some(if self.other_names.contains(&c) { Confidence::Medium } else { Confidence::High });
        }
        let raw = raw_compact(entry);
        for n in &self.names {
            if let Some(rest) = raw.strip_prefix(n.as_str()) {
                let digits = !rest.is_empty() && rest.len() <= 4 && rest.bytes().all(|b| b.is_ascii_digit());
                if digits && self.versions.iter().any(|v| v.replace('.', "") == rest) {
                    return Some(Confidence::High);
                }
            }
        }
        None
    }

    fn version_matches(&self, v: &str) -> bool {
        self.versions.iter().any(|a| a == v || a.starts_with(&format!("{v}.")) || v.starts_with(&format!("{a}.")))
    }

    fn pub_hit(&self, entry: &str) -> bool {
        let c = compact(entry);
        !c.is_empty() && self.publishers.contains(&c)
    }

    fn pub_shared(&self, entry: &str) -> bool {
        self.other_publishers.contains(&compact(entry))
    }

    fn name_prefix(&self, entry: &str) -> bool {
        let c = compact(entry);
        self.names.iter().any(|n| n.chars().count() >= 4 && c.starts_with(n.as_str()))
    }

    /// Does the text contain a path into the install folder? The next character is
    /// checked so that a folder named Foo does not match Foobar.
    fn mentions_location(&self, text_lower: &str) -> bool {
        let Some(loc) = &self.location else { return false };
        let mut from = 0;
        while let Some(i) = text_lower[from..].find(loc.as_str()) {
            let end = from + i + loc.len();
            match text_lower[end..].chars().next() {
                None | Some('\\' | '"' | '\'' | '\0' | ',' | ';') => return true,
                _ => from = end,
            }
        }
        false
    }

    /// A reference into the install folder may belong to another program living
    /// inside it, like Steam and its games.
    fn mention_confidence(&self) -> Confidence {
        if self.nested_others {
            Confidence::Medium
        } else {
            Confidence::High
        }
    }

    fn is_other_location(&self, path: &Path) -> bool {
        let s = lower(path);
        self.other_locations.iter().any(|o| *o == s || s.starts_with(&format!("{o}\\")))
    }

    /// Is the folder another program's folder, or does it contain one?
    fn owned_by_other(&self, path: &Path) -> bool {
        let s = lower(path);
        let prefix = format!("{s}\\");
        self.other_locations.iter().any(|o| *o == s || o.starts_with(&prefix))
    }
}

// ---------------------------------------------------------------- scan

pub fn scan(app: &App, others: &[App]) -> Vec<Leftover> {
    let m = Matcher::new(app, others);
    let pl = places();

    let (mut files, (mut keys, tasks)) = rayon::join(
        || {
            let mut v: Vec<Leftover> = pl.file_roots.par_iter().flat_map_iter(|r| scan_file_root(r, &m)).collect();
            v.extend(pl.shortcut_roots.par_iter().flat_map_iter(|(r, menu)| scan_shortcuts(r, &m, *menu, 0)).collect::<Vec<_>>());
            if let Some(loc) = &app.location {
                if m.location.is_some() && loc.exists() {
                    v.push(Leftover::new(Kind::Path { path: loc.clone(), dir: loc.is_dir() }, Confidence::High, "Install folder"));
                }
            }
            v
        },
        || rayon::join(|| scan_registry(app, &m), || scan_tasks(pl.tasks.as_deref(), &m)),
    );

    files.retain_mut(|l| {
        let Kind::Path { path, .. } = &l.kind else { return true };
        if is_protected(path) || m.is_other_location(path) {
            return false;
        }
        if m.owned_by_other(path) {
            l.confidence = Confidence::Medium;
            l.checked = false;
            l.reason = "Contains another installed program";
        }
        true
    });
    files.append(&mut keys);
    files.extend(tasks);

    let mut out = dedupe(files);
    out.par_iter_mut().for_each(|l| {
        if let Kind::Path { path, dir } = &l.kind {
            l.size = if *dir { dir_size(path) } else { fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        }
    });
    out.sort_by(|a, b| (a.group(), a.confidence, a.label().to_lowercase()).cmp(&(b.group(), b.confidence, b.label().to_lowercase())));
    out
}

/// Keeps each trace once and drops traces nested inside another one.
fn dedupe(mut v: Vec<Leftover>) -> Vec<Leftover> {
    v.sort_by(|a, b| a.dedupe_key().cmp(&b.dedupe_key()).then(a.confidence.cmp(&b.confidence)));
    let mut out: Vec<Leftover> = Vec::with_capacity(v.len());
    let mut kept: Vec<String> = Vec::new();
    for l in v {
        let k = l.dedupe_key();
        let inside = kept.iter().any(|p| k == *p || k.starts_with(&format!("{p}\\")));
        if !inside {
            let is_container = !matches!(l.kind, Kind::Value { .. } | Kind::Task { .. });
            if is_container {
                kept.push(k);
            }
            out.push(l);
        }
    }
    out
}

fn read_dir_names(p: &Path) -> Vec<(String, PathBuf, bool)> {
    let Ok(rd) = fs::read_dir(p) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let ft = e.file_type().ok()?;
            if ft.is_symlink() {
                return None;
            }
            Some((e.file_name().to_string_lossy().into_owned(), e.path(), ft.is_dir()))
        })
        .collect()
}

fn scan_file_root(root: &Path, m: &Matcher) -> Vec<Leftover> {
    let mut out = Vec::new();
    for (name, path, is_dir) in read_dir_names(root) {
        if !is_dir {
            continue;
        }
        if let Some(c) = m.name_hit(&name) {
            // A shared folder such as Programs\Python may hold a versioned
            // subfolder that is clearly this program's. Prefer that one.
            let versioned: Vec<PathBuf> = if c == Confidence::Medium {
                read_dir_names(&path)
                    .into_iter()
                    .filter(|(cn, _, cd)| *cd && m.name_hit(cn) == Some(Confidence::High))
                    .map(|(_, cp, _)| cp)
                    .collect()
            } else {
                Vec::new()
            };
            if versioned.is_empty() {
                out.push(Leftover::new(Kind::Path { path, dir: true }, c, reason_for(c, "Same name as the program")));
            }
            for cp in versioned {
                out.push(Leftover::new(Kind::Path { path: cp, dir: true }, Confidence::High, "Folder for this version of the program"));
            }
        } else if m.pub_hit(&name) {
            let mut found = false;
            for (cn, cp, cd) in read_dir_names(&path) {
                if cd {
                    if let Some(c) = m.name_hit(&cn) {
                        out.push(Leftover::new(Kind::Path { path: cp, dir: true }, c, reason_for(c, "Inside the publisher folder, same name as the program")));
                        found = true;
                    }
                }
            }
            if !found && !m.pub_shared(&name) && m.names.iter().any(|n| n.starts_with(&compact(&name))) {
                out.push(Leftover::new(Kind::Path { path, dir: true }, Confidence::Medium, "Publisher folder, no other program uses it"));
            }
        }
    }
    out
}

fn reason_for(c: Confidence, high: &'static str) -> &'static str {
    match c {
        Confidence::High => high,
        Confidence::Medium => "Another installed program uses this name too",
    }
}

fn scan_shortcuts(dir: &Path, m: &Matcher, start_menu: bool, depth: u8) -> Vec<Leftover> {
    let mut out = Vec::new();
    let max_depth = if start_menu { 3 } else { 1 };
    for (name, path, is_dir) in read_dir_names(dir) {
        if is_dir {
            let hit = if start_menu { m.name_hit(&name) } else { None };
            if let Some(c) = hit {
                out.push(Leftover::new(Kind::Path { path, dir: true }, c, reason_for(c, "Start menu folder")));
            } else if depth < max_depth {
                out.extend(scan_shortcuts(&path, m, start_menu, depth + 1));
            }
            continue;
        }
        let low = name.to_lowercase();
        let Some(stem) = low.strip_suffix(".lnk").or_else(|| low.strip_suffix(".url")) else { continue };
        if let Some(c) = m.name_hit(stem) {
            out.push(Leftover::new(Kind::Path { path, dir: false }, c, reason_for(c, "Shortcut with the program's name")));
        } else if m.location.is_some() && m.mentions_location(&read_text_lower(&path, 64 * 1024)) {
            out.push(Leftover::new(Kind::Path { path, dir: false }, m.mention_confidence(), "Shortcut points into the install folder"));
        }
    }
    out
}

/// Reads a file as both UTF-16 and 8-bit text, lowercased.
/// Shortcut and task files may store a path either way.
fn read_text_lower(p: &Path, limit: usize) -> String {
    let Ok(mut b) = fs::read(p) else { return String::new() };
    b.truncate(limit);
    let u16s: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let mut s = String::from_utf16_lossy(&u16s);
    s.push('\0');
    s.extend(b.iter().map(|&c| c as char));
    s.to_lowercase()
}

pub fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(p) else { return 0 };
    let entries: Vec<_> = rd.flatten().collect();
    entries
        .par_iter()
        .map(|e| match e.file_type() {
            Ok(ft) if ft.is_symlink() => 0,
            Ok(ft) if ft.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

const SOFTWARE_VIEWS: [(Hive, View); 3] = [(Hive::Hkcu, View::V64), (Hive::Hklm, View::V64), (Hive::Hklm, View::V32)];
const SKIP_KEYS: &[&str] = &["classes", "wow6432node", "policies", "registeredapplications", "clients"];

fn scan_registry(app: &App, m: &Matcher) -> Vec<Leftover> {
    let mut out = Vec::new();

    if reg::key_exists(app.hive, app.view, &app.key_path()) {
        out.push(Leftover::new(
            Kind::Key { hive: app.hive, view: app.view, path: app.key_path() },
            Confidence::High,
            "Uninstall entry",
        ));
    }

    for (hive, view) in SOFTWARE_VIEWS {
        let Some(sw) = Key::open(hive, view, "SOFTWARE") else { continue };
        for name in sw.subkeys() {
            if SKIP_KEYS.contains(&name.to_lowercase().as_str()) {
                continue;
            }
            let path = format!("SOFTWARE\\{name}");
            if let Some(c) = m.name_hit(&name) {
                out.push(Leftover::new(Kind::Key { hive, view, path }, c, reason_for(c, "Same name as the program")));
            } else if m.pub_hit(&name) || name.eq_ignore_ascii_case("microsoft") {
                let Some(k) = Key::open(hive, view, &path) else { continue };
                for sub in k.subkeys() {
                    if let Some(c) = m.name_hit(&sub) {
                        let reason = reason_for(c, "Inside the publisher key, same name as the program");
                        out.push(Leftover::new(Kind::Key { hive, view, path: format!("{path}\\{sub}") }, c, reason));
                    }
                }
            }
        }

        for run in ["Run", "RunOnce"] {
            let path = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\{run}");
            let Some(k) = Key::open(hive, view, &path) else { continue };
            for (name, val) in k.values() {
                let data = match val {
                    Value::Str(s) => s.to_lowercase(),
                    _ => String::new(),
                };
                let named = m.name_hit(&name) == Some(Confidence::High);
                let hit = if m.mentions_location(&data) {
                    let c = if named { Confidence::High } else { m.mention_confidence() };
                    Some((c, "Starts at logon from the install folder"))
                } else {
                    m.name_hit(&name).map(|_| (Confidence::Medium, "Startup entry with the program's name"))
                };
                if let Some((c, reason)) = hit {
                    out.push(Leftover::new(Kind::Value { hive, view, path: path.clone(), name }, c, reason));
                }
            }
        }

        let ap = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths";
        if let Some(k) = Key::open(hive, view, ap) {
            for sub in k.subkeys() {
                let path = format!("{ap}\\{sub}");
                let Some(sk) = Key::open(hive, view, &path) else { continue };
                let target = sk.string("").unwrap_or_default().to_lowercase();
                if m.mentions_location(&target) {
                    out.push(Leftover::new(Kind::Key { hive, view, path }, m.mention_confidence(), "App Paths entry pointing into the install folder"));
                }
            }
        }
    }
    out
}

fn scan_tasks(root: Option<&Path>, m: &Matcher) -> Vec<Leftover> {
    let Some(root) = root else { return Vec::new() };
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0u8)];
    while let Some((dir, depth)) = stack.pop() {
        for (name, path, is_dir) in read_dir_names(&dir) {
            if is_dir {
                if depth < 3 && !name.eq_ignore_ascii_case("microsoft") {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let task = path.strip_prefix(root).map(|r| format!("\\{}", r.display())).unwrap_or(name.clone());
            if m.mentions_location(&read_text_lower(&path, 256 * 1024)) {
                out.push(Leftover::new(Kind::Task { name: task }, m.mention_confidence(), "Task runs from the install folder"));
            } else if m.name_prefix(&name) {
                out.push(Leftover::new(Kind::Task { name: task }, Confidence::Medium, "Task name starts with the program's name"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, publisher: &str, key: &str, loc: Option<&str>) -> App {
        App {
            name: name.into(),
            version: String::new(),
            publisher: publisher.into(),
            install_date: String::new(),
            size_kb: 0,
            location: loc.map(PathBuf::from),
            location_guessed: false,
            uninstall: None,
            quiet_uninstall: None,
            msi: false,
            hive: Hive::Hklm,
            view: View::V64,
            key_name: key.into(),
        }
    }

    #[test]
    fn words_drop_versions_and_arch() {
        assert_eq!(compact("Mozilla Firefox (x64 en-US)"), "mozillafirefox");
        assert_eq!(compact("7-Zip 23.01 (x64)"), "7zip");
        assert_eq!(compact("Notepad++ (64-bit x64)"), "notepad");
        assert_eq!(compact("Microsoft Visual C++ 2015 Redistributable"), "microsoftvisualcredistributable");
        assert_eq!(compact("3DMark"), "3dmark");
        assert_eq!(compact("Python 3.12.1 (64-bit)"), "python");
    }

    #[test]
    fn variants() {
        let a = app("Mozilla Firefox (x64 en-US)", "Mozilla", "Mozilla Firefox 131.0 (x64 en-US)", Some(r"C:\Program Files\Mozilla Firefox"));
        let v = name_variants(&a);
        assert!(v.contains(&"mozillafirefox".to_string()));
        assert!(v.contains(&"firefox".to_string()));
        assert_eq!(publisher_variants(&a), vec!["mozilla".to_string()]);

        let b = app("Git", "The Git Development Community", "Git_is1", None);
        assert_eq!(name_variants(&b), vec!["git".to_string()]);

        let c = app("Microsoft Visual Studio Code (User)", "Microsoft Corporation", "{771FD6B0-FA20-440A-A002-3B3BAC16DC50}_is1", None);
        assert!(name_variants(&c).contains(&"visualstudiocode".to_string()));
    }

    #[test]
    fn generic_names_never_match() {
        let a = app("Update", "", "Update", None);
        assert!(name_variants(&a).is_empty());
        let b = app("Roblox Player", "Roblox Corporation", "roblox-player", Some(r"C:\Program Files\Roblox\Versions\version-9ac8595712b447a7"));
        assert_eq!(name_variants(&b), vec!["robloxplayer".to_string()]);
        let c = app("ShareX", "ShareX Team", "82E6AC09-0FEF-4390-AD9F-0DD3F5561EFC_is1", None);
        assert_eq!(name_variants(&c), vec!["sharex".to_string()]);
    }

    #[test]
    fn shared_location_is_ignored() {
        let a = app("WhatsApp Web", "Thorium", "x", Some(r"C:\Users\A\AppData\Local\Thorium\Application"));
        let b = app("Thorium", "The Thorium Authors", "Thorium", Some(r"C:\Users\A\AppData\Local\Thorium\Application"));
        let m = Matcher::new(&a, &[b.clone()]);
        assert!(m.location.is_none());
        let m = Matcher::new(&b, &[a]);
        assert!(m.location.is_some());
    }

    #[test]
    fn location_boundary() {
        let a = app("Foo", "", "Foo", Some(r"C:\Program Files\Foo"));
        let m = Matcher::new(&a, &[]);
        assert!(m.mentions_location(r#""c:\program files\foo\foo.exe" --tray"#));
        assert!(!m.mentions_location(r"c:\program files\foobar\foo.exe"));
    }

    #[test]
    fn shared_names_are_medium() {
        let a = app("Python 3.12.1 (64-bit)", "Python Software Foundation", "{A}", None);
        let b = app("Python 3.11.4 (64-bit)", "Python Software Foundation", "{B}", None);
        let m = Matcher::new(&a, &[b]);
        assert_eq!(m.name_hit("Python"), Some(Confidence::Medium));
    }

    #[test]
    fn versioned_names_need_the_same_version() {
        let mut a = app("Python 3.11.9 (64-bit)", "Python Software Foundation", "{A}", None);
        a.version = "3.11.9150.0".into();
        let m = Matcher::new(&a, &[]);
        assert_eq!(m.name_hit("Python 3.11"), Some(Confidence::High));
        assert_eq!(m.name_hit("Python311"), Some(Confidence::High));
        assert_eq!(m.name_hit("Python 3.13"), None);
        assert_eq!(m.name_hit("Python313"), None);
        assert_eq!(m.name_hit("Python"), Some(Confidence::High));
    }

    #[test]
    fn protected_paths() {
        assert!(is_protected(Path::new(r"C:\")));
        assert!(is_protected(Path::new(r"C:\Windows\System32")));
        if let Some(pf) = env("ProgramFiles") {
            assert!(is_protected(&pf));
            assert!(is_protected(&pf.join("Common Files")));
            assert!(!is_protected(&pf.join("SomeApp")));
        }
        if let Some(h) = env("USERPROFILE") {
            assert!(is_protected(&h));
            assert!(is_protected(h.parent().unwrap()));
        }
    }

    #[test]
    fn nested_items_collapse() {
        let k = |p: &str| Leftover::new(Kind::Path { path: PathBuf::from(p), dir: true }, Confidence::High, "");
        let v = dedupe(vec![k(r"C:\A\Foo\sub"), k(r"C:\A\Foo"), k(r"C:\A\foo"), k(r"C:\A\Foobar")]);
        let labels: Vec<_> = v.iter().map(|l| l.label()).collect();
        assert_eq!(labels.len(), 2);
        assert!(labels.iter().any(|l| l.ends_with("Foobar")));
    }
}
