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
    Service { name: String },
    Firewall { id: String, name: String },
    PathEntry { hive: Hive, entry: String },
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
    /// Name of the program the trace belongs to, set when several are removed at once.
    pub owner: String,
}

impl Leftover {
    pub(crate) fn new(kind: Kind, confidence: Confidence, reason: &'static str) -> Self {
        Leftover { kind, confidence, reason, size: 0, checked: confidence == Confidence::High, owner: String::new() }
    }

    pub fn group(&self) -> u8 {
        match self.kind {
            Kind::Path { .. } => 0,
            Kind::Key { .. } | Kind::Value { .. } | Kind::PathEntry { .. } => 1,
            Kind::Task { .. } => 2,
            Kind::Service { .. } | Kind::Firewall { .. } => 3,
        }
    }

    pub fn label(&self) -> String {
        match &self.kind {
            Kind::Path { path, .. } => path.display().to_string(),
            Kind::Key { hive, view, path } => reg::display_path(*hive, *view, path),
            Kind::Value { hive, view, path, name } => format!("{} : {}", reg::display_path(*hive, *view, path), name),
            Kind::Task { name } => name.clone(),
            Kind::Service { name } => format!("Service {name}"),
            Kind::Firewall { name, .. } => format!("Firewall rule {name}"),
            Kind::PathEntry { hive, entry } => format!("{} PATH: {entry}", hive.short()),
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
        // Package folders are named after the package id and version, not the program.
        if !app.location_guessed && !matches!(app.source, crate::apps::Source::Store { .. }) {
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

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p = std::ptr::null_mut();
    unsafe {
        if SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) != 0 {
            CoTaskMemFree(p as _);
            return None;
        }
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        CoTaskMemFree(p as _);
        Some(PathBuf::from(s))
    }
}

/// A folder in the user profile that holds per-program folders.
pub struct ProfileRoot {
    path: PathBuf,
    /// Only folders starting with a dot count, like .gradle in the home folder.
    dotted: bool,
    /// Best confidence a match here can get. Documents holds the user's own files.
    cap: Confidence,
}

fn lower(p: &Path) -> String {
    p.to_string_lossy().trim_end_matches('\\').to_lowercase()
}

struct Places {
    file_roots: Vec<PathBuf>,
    /// Root, and whether it is a Start menu. Desktop folders are never matched by name.
    shortcut_roots: Vec<(PathBuf, bool)>,
    tasks: Option<PathBuf>,
    profile_roots: Vec<ProfileRoot>,
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

        use windows_sys::Win32::UI::Shell::{FOLDERID_Documents, FOLDERID_SavedGames};
        let docs = known_folder(&FOLDERID_Documents);
        let mut profile_roots = Vec::new();
        if let Some(h) = &home {
            profile_roots.push(ProfileRoot { path: h.clone(), dotted: true, cap: Confidence::High });
        }
        if let Some(d) = &docs {
            profile_roots.push(ProfileRoot { path: d.join("My Games"), dotted: false, cap: Confidence::High });
            profile_roots.push(ProfileRoot { path: d.clone(), dotted: false, cap: Confidence::Medium });
        }
        if let Some(g) = known_folder(&FOLDERID_SavedGames) {
            profile_roots.push(ProfileRoot { path: g, dotted: false, cap: Confidence::High });
        }
        for r in &profile_roots {
            protected.push(lower(&r.path));
        }
        for (root, _) in crate::pkgmgr::scoop_roots() {
            for sub in ["", "\\apps", "\\persist", "\\shims", "\\cache", "\\buckets"] {
                protected.push(format!("{}{sub}", lower(&root)));
            }
        }
        if let Some(root) = crate::pkgmgr::choco_root() {
            for sub in ["", "\\lib", "\\bin"] {
                protected.push(format!("{}{sub}", lower(&root)));
            }
        }

        Places {
            profile_roots,
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

pub(crate) struct Matcher {
    names: Vec<String>,
    /// Versions from the name and version fields, with their major.minor forms.
    versions: Vec<String>,
    publishers: Vec<String>,
    pub(crate) location: Option<String>,
    /// Executable names found in the install folder, without .exe.
    pub(crate) exe_stems: HashSet<String>,
    other_names: HashSet<String>,
    other_publishers: HashSet<String>,
    other_locations: Vec<String>,
    /// Another program's folder sits inside the install folder.
    nested_others: bool,
    /// Other programs' folders inside the install folder.
    nested: Vec<String>,
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
        let nested: Vec<String> = match &location {
            Some(s) => other_locations.iter().filter(|o| o.starts_with(&format!("{s}\\"))).cloned().collect(),
            None => Vec::new(),
        };
        let nested_others = !nested.is_empty();
        Matcher {
            names,
            versions: app_versions(app),
            exe_stems: location.as_deref().map(|l| exe_stems(l, &nested)).unwrap_or_default(),
            publishers: publisher_variants(app),
            location,
            other_names: others.iter().flat_map(name_variants).collect(),
            other_publishers: others.iter().flat_map(publisher_variants).collect(),
            nested_others,
            nested: nested.clone(),
            other_locations,
        }
    }

    /// A name that carries a version, like "Python 3.11" or "Python311",
    /// only matches when the version is this program's version.
    pub(crate) fn name_hit(&self, entry: &str) -> Option<Confidence> {
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
    pub(crate) fn mentions_location(&self, text_lower: &str) -> bool {
        let Some(loc) = &self.location else { return false };
        let mut from = 0;
        let boundary = |rest: &str| matches!(rest.chars().next(), None | Some('\\' | '"' | '\'' | '\0' | ',' | ';'));
        while let Some(i) = text_lower[from..].find(loc.as_str()) {
            let start = from + i;
            let end = start + loc.len();
            let tail = &text_lower[start..];
            let in_nested = self.nested.iter().any(|o| tail.starts_with(o.as_str()) && boundary(&tail[o.len()..]));
            if boundary(&text_lower[end..]) && !in_nested {
                return true;
            }
            from = end;
        }
        false
    }

    /// A reference into the install folder may belong to another program living
    /// inside it, like Steam and its games.
    pub(crate) fn mention_confidence(&self) -> Confidence {
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

/// How far the scan reaches. Deeper finds more and needs more care in review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// Only traces with strong evidence.
    Safe,
    Normal,
    /// Also folders and keys whose name starts with the program's name.
    Deep,
}

impl Depth {
    pub const ALL: [(Depth, &'static str); 3] = [(Depth::Safe, "Safe"), (Depth::Normal, "Normal"), (Depth::Deep, "Deep")];
}

pub fn scan(app: &App, others: &[App], depth: Depth) -> Vec<Leftover> {
    let m = Matcher::new(app, others);
    let pl = places();

    let (mut files, (mut keys, tasks)) = rayon::join(
        || {
            let mut v: Vec<Leftover> = pl.file_roots.par_iter().flat_map_iter(|r| scan_file_root(r, &m)).collect();
            v.extend(pl.profile_roots.iter().flat_map(|r| scan_profile_root(r, &m)));
            v.extend(pl.shortcut_roots.par_iter().flat_map_iter(|(r, menu)| scan_shortcuts(r, &m, *menu, 0)).collect::<Vec<_>>());
            if let crate::apps::Source::Store { family, .. } = &app.source {
                if let Some(d) = crate::store::data_folder(family).filter(|d| d.is_dir()) {
                    v.push(Leftover::new(Kind::Path { path: d, dir: true }, Confidence::High, "Package data folder"));
                }
            }
            if let crate::apps::Source::Scoop { name, global } = &app.source {
                for (root, g) in crate::pkgmgr::scoop_roots() {
                    let persist = root.join("persist").join(name);
                    if g == *global && persist.is_dir() {
                        v.push(Leftover::new(Kind::Path { path: persist, dir: true }, Confidence::High, "Scoop persist folder"));
                    }
                }
            }
            if let Some(loc) = &app.location {
                if m.location.is_some() && loc.exists() {
                    v.push(Leftover::new(Kind::Path { path: loc.clone(), dir: loc.is_dir() }, Confidence::High, "Install folder"));
                }
            }
            v
        },
        || {
            let (mut r, (t, d)) = rayon::join(|| scan_registry(app, &m), || rayon::join(|| scan_tasks(pl.tasks.as_deref(), &m), || crate::deep::scan(&m)));
            r.extend(d);
            (r, t)
        },
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
    files.extend(crate::trace::leftovers(app, &m.names, m.location.as_deref()));
    if depth == Depth::Deep {
        files.extend(scan_prefixes(&m));
    }
    if depth == Depth::Safe {
        files.retain(|l| l.confidence == Confidence::High);
    }

    let mut out = dedupe(files);
    out.par_iter_mut().for_each(|l| {
        if let Kind::Path { path, dir } = &l.kind {
            l.size = if *dir { dir_size(path) } else { fs::metadata(path).map(|m| m.len()).unwrap_or(0) };
        }
    });
    out.sort_by(|a, b| (a.group(), a.confidence, a.label().to_lowercase()).cmp(&(b.group(), b.confidence, b.label().to_lowercase())));
    out
}

/// Executable names in the install folder, a few levels deep.
fn exe_stems(location: &str, skip: &[String]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut stack = vec![(PathBuf::from(location), 0u8)];
    let mut seen = 0;
    while let Some((dir, depth)) = stack.pop() {
        for (name, path, is_dir) in read_dir_names(&dir) {
            seen += 1;
            if seen > 5000 {
                return out;
            }
            if is_dir {
                if depth < 3 && !skip.contains(&lower(&path)) {
                    stack.push((path, depth + 1));
                }
            } else if let Some(stem) = name.to_lowercase().strip_suffix(".exe") {
                out.insert(stem.to_string());
            }
        }
    }
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
            let is_container = matches!(l.kind, Kind::Path { .. } | Kind::Key { .. });
            if is_container {
                kept.push(k);
            }
            out.push(l);
        }
    }
    out
}

/// Deep scan: top-level folders and keys whose name starts with one of the
/// program's names, like "FooUpdater" or "Foo Helper" for "Foo".
fn scan_prefixes(m: &Matcher) -> Vec<Leftover> {
    let starts = |entry: &str| {
        let c = compact(entry);
        m.name_hit(entry).is_none() && m.names.iter().any(|n| n.chars().count() >= 4 && c.len() > n.len() && c.starts_with(n.as_str()))
    };
    let reason = "Name starts with the program's name";
    let mut out: Vec<Leftover> = places()
        .file_roots
        .par_iter()
        .flat_map_iter(|root| {
            read_dir_names(root)
                .into_iter()
                .filter(|(n, _, d)| *d && starts(n))
                .map(|(_, path, _)| Leftover::new(Kind::Path { path, dir: true }, Confidence::Medium, reason))
                .collect::<Vec<_>>()
        })
        .filter(|l| matches!(&l.kind, Kind::Path { path, .. } if !is_protected(path) && !m.owned_by_other(path)))
        .collect();
    for (hive, view) in SOFTWARE_VIEWS {
        let Some(sw) = Key::open(hive, view, "SOFTWARE") else { continue };
        for name in sw.subkeys().into_iter().filter(|n| starts(n)) {
            out.push(Leftover::new(Kind::Key { hive, view, path: format!("SOFTWARE\\{name}") }, Confidence::Medium, reason));
        }
    }
    out
}

/// Folders that belong to Windows, runtimes or drivers and have no Uninstall
/// entry of their own. Compared in compact form.
const SYSTEM_FOLDERS: &[&str] = &[
    "internetexplorer", "windowsapps", "modifiablewindowsapps", "microsoftnet", "referenceassemblies", "dotnet",
    "msbuild", "windowsdefender", "windowsdefenderadvancedthreatprotection", "windowsmail", "windowsmediaplayer",
    "windowsnt", "windowsphotoviewer", "windowsportabledevices", "windowssecurity", "windowspowershell",
    "windowssidebar", "uninstallinformation", "nvidiacorporation", "nvidia", "regid", "ssh", "usoshared",
    "usoprivate", "softwaredistribution", "comms", "connecteddevicesplatform", "d3dscache", "crashdumps",
    "packagecache", "chocolatey", "scoop", "peernetworking", "publishers", "virtualstore", "devicesync",
    "microsoftedge", "microsoftupdatehealthtools", "windowsdefenderapplicationguard", "iis", "iisexpress",
    "intel", "amd", "ati", "realtek", "dell", "hp", "lenovo", "asus", "msi", "oem", "drivers", "driverstore",
    "mozillamaintenanceservice", "googleupdater", "edgeupdate", "edgewebview", "fontcache", "igfx",
    "windowskits", "wsl", "applicationverifier", "windowsappcertificationkit", "windowsperformanceanalyzer",
    "toastnotificationmanagercompat", "placeholdertilelogofolder", "peerdistrepub", "elevateddiagnostics",
    "visualstudiosetup", "downloadedinstallations", "shimgen",
];

/// How long a folder must sit untouched before it counts as unclaimed.
const ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// Newest modification time of a folder and its direct children.
fn last_touched(p: &Path) -> Option<std::time::SystemTime> {
    let mut newest = fs::metadata(p).and_then(|m| m.modified()).ok()?;
    if let Ok(rd) = fs::read_dir(p) {
        for e in rd.flatten().take(500) {
            if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                newest = newest.max(t);
            }
        }
    }
    Some(newest)
}

/// Top-level folders in the usual program locations that no installed program
/// claims by name, publisher or location, and that nothing has written to for
/// a month. Every result comes unchecked: this is a list to look through.
pub fn orphans(apps: &[App]) -> Vec<Leftover> {
    let pl = places();
    let mut claimed: HashSet<String> = HashSet::new();
    for a in apps {
        claimed.extend(name_variants(a));
        claimed.extend(publisher_variants(a));
    }
    let locations: Vec<String> = apps.iter().filter_map(|a| a.location.as_ref()).map(|l| lower(l)).collect();
    let cutoff = std::time::SystemTime::now() - ORPHAN_AGE;
    let mut out: Vec<Leftover> = pl
        .file_roots
        .par_iter()
        .flat_map_iter(|root| {
            let mut v = Vec::new();
            for (name, path, is_dir) in read_dir_names(root) {
                if !is_dir || name.starts_with(['.', '{', '$']) {
                    continue;
                }
                let c = compact(&name);
                if c.len() < 3 || GENERIC.contains(&c.as_str()) || SYSTEM_FOLDERS.iter().any(|s| c.starts_with(s)) {
                    continue;
                }
                if claimed.contains(&c) || claimed.iter().any(|n| n.len() >= 5 && (c.starts_with(n.as_str()) || n.starts_with(c.as_str()))) {
                    continue;
                }
                let l = lower(&path);
                let prefix = format!("{l}\\");
                if is_protected(&path) || locations.iter().any(|o| *o == l || o.starts_with(&prefix) || l.starts_with(&format!("{o}\\"))) {
                    continue;
                }
                if last_touched(&path).is_none_or(|t| t > cutoff) {
                    continue;
                }
                v.push(Leftover::new(Kind::Path { path, dir: true }, Confidence::Medium, "No installed program claims this folder, untouched for a month"));
            }
            v
        })
        .collect();
    out.par_iter_mut().for_each(|l| {
        if let Kind::Path { path, .. } = &l.kind {
            l.size = dir_size(path);
        }
    });
    out.sort_by_key(|l| std::cmp::Reverse(l.size));
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

fn scan_profile_root(r: &ProfileRoot, m: &Matcher) -> Vec<Leftover> {
    let mut out = Vec::new();
    for (name, path, is_dir) in read_dir_names(&r.path) {
        if !is_dir {
            continue;
        }
        let bare = match name.strip_prefix('.') {
            Some(b) if r.dotted => b,
            None if !r.dotted => name.as_str(),
            _ => continue,
        };
        if let Some(c) = m.name_hit(bare) {
            let c = c.max(r.cap);
            let reason = if c == Confidence::Medium && r.cap == Confidence::Medium {
                "In Documents, may hold your own files"
            } else {
                reason_for(c, "Profile folder with the program's name")
            };
            out.push(Leftover::new(Kind::Path { path, dir: true }, c, reason));
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

    if app.source == crate::apps::Source::Registry && reg::key_exists(app.hive, app.view, &app.key_path()) {
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
            installer: crate::apps::Installer::Unknown,
            source: crate::apps::Source::Registry,
            icon: None,
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
