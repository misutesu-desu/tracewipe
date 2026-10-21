//! Install tracing: what an installer created, recorded while it runs.
//!
//! Files come from the NTFS change journal. Before the installer starts, the
//! journal's next record number is noted; afterwards every "file created"
//! record since then is read and resolved to a path. Nothing hooks the
//! installer, so tracing costs nothing while it runs. Registry keys come from
//! comparing key names before and after.
//!
//! Other programs keep writing while an installer runs, so a trace also holds
//! unrelated files. The leftover scan treats traced items that name the program
//! or sit in its folder as strong evidence, and the rest as weak.

use crate::apps::App;
use crate::leftovers::{Confidence, Kind, Leftover};
use crate::reg::{self, wide, Hive, Key, View};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Ioctl::*;
use windows_sys::Win32::System::IO::DeviceIoControl;

const UNINSTALL: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall";

struct Volume {
    letter: char,
    journal: u64,
    next: i64,
}

pub struct Snapshot {
    volumes: Vec<Volume>,
    keys: HashSet<(Hive, View, String)>,
    uninstall: HashSet<(Hive, View, String)>,
}

#[derive(Default)]
pub struct Trace {
    /// Uninstall entries the installer created: hive, view and key name.
    pub entries: Vec<(Hive, View, String)>,
    pub files: Vec<PathBuf>,
    pub keys: Vec<(Hive, View, String)>,
}

fn open_volume(letter: char) -> Option<HANDLE> {
    let path = wide(&format!("\\\\.\\{letter}:"));
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    (h != INVALID_HANDLE_VALUE && !h.is_null()).then_some(h)
}

fn query_journal(h: HANDLE) -> Option<USN_JOURNAL_DATA_V0> {
    let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut got = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut data as *mut _ as *mut _,
            std::mem::size_of_val(&data) as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(data)
}

/// Fixed NTFS drives with a change journal.
fn volumes() -> Vec<Volume> {
    let mut out = Vec::new();
    for letter in 'C'..='Z' {
        let root = wide(&format!("{letter}:\\"));
        if unsafe { GetDriveTypeW(root.as_ptr()) } != 3 {
            continue; // DRIVE_FIXED
        }
        let Some(h) = open_volume(letter) else { continue };
        if let Some(j) = query_journal(h) {
            out.push(Volume { letter, journal: j.UsnJournalID, next: j.NextUsn });
        }
        unsafe { CloseHandle(h) };
    }
    out
}

/// Key names to a fixed depth. Deep enough for SOFTWARE\Vendor\Product and
/// Classes\CLSID\{id}, shallow enough to read in about a second.
fn key_names(hive: Hive, view: View, root: &str, depth: u8, out: &mut HashSet<(Hive, View, String)>) {
    let Some(k) = Key::open(hive, view, root) else { return };
    for sub in k.subkeys() {
        let path = format!("{root}\\{sub}");
        if depth > 1 {
            key_names(hive, view, &path, depth - 1, out);
        }
        out.insert((hive, view, path.to_lowercase()));
    }
}

const ROOTS: [(Hive, View, &str, u8); 5] = [
    (Hive::Hkcu, View::V64, "SOFTWARE", 3),
    (Hive::Hklm, View::V64, "SOFTWARE", 3),
    (Hive::Hklm, View::V32, "SOFTWARE", 3),
    (Hive::Hklm, View::V64, "SYSTEM\\CurrentControlSet\\Services", 1),
    (Hive::Hkcu, View::V64, "SOFTWARE\\Classes", 2),
];

fn registry() -> HashSet<(Hive, View, String)> {
    let mut out = HashSet::new();
    for (hive, view, root, depth) in ROOTS {
        key_names(hive, view, root, depth, &mut out);
    }
    out
}

fn uninstall_entries() -> HashSet<(Hive, View, String)> {
    let mut out = HashSet::new();
    for (hive, view) in [(Hive::Hklm, View::V64), (Hive::Hklm, View::V32), (Hive::Hkcu, View::V64)] {
        if let Some(k) = Key::open(hive, view, UNINSTALL) {
            out.extend(k.subkeys().into_iter().map(|s| (hive, view, s)));
        }
    }
    out
}

/// Records the state before an install. Reading the journal needs administrator rights.
pub fn begin() -> Result<Snapshot, String> {
    let volumes = volumes();
    if volumes.is_empty() {
        return Err("Tracing needs administrator rights to read the NTFS change journal".into());
    }
    Ok(Snapshot { volumes, keys: registry(), uninstall: uninstall_entries() })
}

/// File reference numbers of everything created on the volume since `from`.
fn created_since(h: HANDLE, v: &Volume, until: i64) -> Vec<i64> {
    let mut ids = Vec::new();
    let mut start = v.next;
    let mut buf = vec![0u8; 1 << 16];
    while start < until {
        let req = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: start,
            ReasonMask: USN_REASON_FILE_CREATE,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: v.journal,
        };
        let mut got = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_READ_USN_JOURNAL,
                &req as *const _ as *const _,
                std::mem::size_of_val(&req) as u32,
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || got <= 8 {
            break;
        }
        let next = i64::from_le_bytes(buf[..8].try_into().unwrap());
        let mut off = 8usize;
        while off + 60 <= got as usize {
            let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            let major = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
            if len == 0 {
                break;
            }
            if major == 2 {
                let id = i64::from_le_bytes(buf[off + 8..off + 16].try_into().unwrap());
                let reason = u32::from_le_bytes(buf[off + 40..off + 44].try_into().unwrap());
                if reason & USN_REASON_FILE_CREATE != 0 {
                    ids.push(id);
                }
            }
            off += len;
        }
        if next <= start {
            break;
        }
        start = next;
    }
    ids
}

/// Current path of a file by its reference number, if it still exists.
fn path_of(volume: HANDLE, id: i64) -> Option<PathBuf> {
    let mut desc: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
    desc.dwSize = std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32;
    desc.Type = FileIdType;
    desc.Anonymous.FileId = id;
    unsafe {
        let h = OpenFileById(
            volume,
            &desc,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        );
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let n = GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0);
        CloseHandle(h);
        if n == 0 || n as usize >= buf.len() {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        Some(PathBuf::from(s.trim_start_matches("\\\\?\\")))
    }
}

/// Places that change all the time for reasons unrelated to installing.
fn is_noise(p: &Path) -> bool {
    let s = p.to_string_lossy().to_lowercase();
    let env_lower = |k: &str| std::env::var(k).map(|v| v.to_lowercase()).ok();
    let mut prefixes: Vec<String> = ["SystemRoot", "TEMP", "TMP"].iter().filter_map(|k| env_lower(k)).collect();
    if let Some(l) = env_lower("LOCALAPPDATA") {
        prefixes.push(format!("{l}\\microsoft\\windows"));
        prefixes.push(format!("{l}\\packages"));
        prefixes.push(format!("{l}\\nietono"));
        prefixes.push(format!("{l}\\temp"));
    }
    if let Some(r) = env_lower("APPDATA") {
        prefixes.push(format!("{r}\\microsoft\\windows\\recent"));
    }
    if let Some(d) = env_lower("ProgramData") {
        prefixes.push(format!("{d}\\microsoft"));
        prefixes.push(format!("{d}\\package cache"));
    }
    s.contains("\\$recycle.bin")
        || s.contains("\\system volume information")
        || s.contains("\\$extend")
        || prefixes.iter().any(|p| s.starts_with(&format!("{p}\\")) || s == *p)
}

/// Keeps only the top-most entries: a created folder stands for everything in it.
fn collapse(mut v: Vec<String>) -> Vec<String> {
    v.sort_by_key(|s| s.to_lowercase());
    let mut out: Vec<String> = Vec::new();
    for s in v {
        let low = s.to_lowercase();
        if out.last().is_some_and(|p| low == p.to_lowercase() || low.starts_with(&format!("{}\\", p.to_lowercase()))) {
            continue;
        }
        out.push(s);
    }
    out
}

/// Reads what changed since `begin`.
pub fn finish(snap: Snapshot) -> Trace {
    let mut files: Vec<String> = Vec::new();
    for v in &snap.volumes {
        let Some(h) = open_volume(v.letter) else { continue };
        let until = query_journal(h).map(|j| j.NextUsn).unwrap_or(v.next);
        let mut seen = HashSet::new();
        for id in created_since(h, v, until) {
            if seen.insert(id) {
                if let Some(p) = path_of(h, id).filter(|p| !is_noise(p)) {
                    files.push(p.to_string_lossy().into_owned());
                }
            }
        }
        unsafe { CloseHandle(h) };
    }
    let after = registry();
    let new_keys: Vec<(Hive, View, String)> = after.difference(&snap.keys).cloned().collect();
    // Collapse per hive and view so that SOFTWARE\Foo stands for SOFTWARE\Foo\Bar.
    let mut keys = Vec::new();
    for (hive, view) in [(Hive::Hkcu, View::V64), (Hive::Hklm, View::V64), (Hive::Hklm, View::V32)] {
        let group: Vec<String> = new_keys.iter().filter(|(h, w, _)| *h == hive && *w == view).map(|(_, _, p)| p.clone()).collect();
        keys.extend(collapse(group).into_iter().map(|p| (hive, view, p)));
    }
    let entries = uninstall_entries().difference(&snap.uninstall).cloned().collect();
    Trace { entries, files: collapse(files).into_iter().map(PathBuf::from).collect(), keys }
}

// ---------------------------------------------------------------- storage

fn dir() -> PathBuf {
    crate::backup::root().parent().map(|p| p.join("traces")).unwrap_or_else(|| "traces".into())
}

fn hive_code(h: Hive) -> &'static str {
    h.short()
}

fn view_code(v: View) -> &'static str {
    match v {
        View::V64 => "64",
        View::V32 => "32",
    }
}

fn parse_hive(s: &str) -> Option<Hive> {
    match s {
        "HKLM" => Some(Hive::Hklm),
        "HKCU" => Some(Hive::Hkcu),
        _ => None,
    }
}

fn parse_view(s: &str) -> Option<View> {
    match s {
        "64" => Some(View::V64),
        "32" => Some(View::V32),
        _ => None,
    }
}

/// Writes the trace as one tab separated line per item. Returns the file.
pub fn save(t: &Trace) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir())?;
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let file = dir().join(format!("{secs}.txt"));
    let mut text = String::new();
    for (h, v, k) in &t.entries {
        text.push_str(&format!("entry\t{}\t{}\t{k}\n", hive_code(*h), view_code(*v)));
    }
    for f in &t.files {
        text.push_str(&format!("file\t{}\n", f.display()));
    }
    for (h, v, k) in &t.keys {
        text.push_str(&format!("key\t{}\t{}\t{k}\n", hive_code(*h), view_code(*v)));
    }
    std::fs::write(&file, text)?;
    Ok(file)
}

fn parse(text: &str) -> Trace {
    let mut t = Trace::default();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["entry", h, v, k] => {
                if let (Some(h), Some(v)) = (parse_hive(h), parse_view(v)) {
                    t.entries.push((h, v, k.to_string()));
                }
            }
            ["file", p] => t.files.push(PathBuf::from(p)),
            ["key", h, v, k] => {
                if let (Some(h), Some(v)) = (parse_hive(h), parse_view(v)) {
                    t.keys.push((h, v, k.to_string()));
                }
            }
            _ => {}
        }
    }
    t
}

/// Uninstall entries that have a recorded trace, lowercased.
pub fn index() -> HashSet<(Hive, View, String)> {
    let Ok(rd) = std::fs::read_dir(dir()) else { return HashSet::new() };
    rd.flatten()
        .flat_map(|e| parse(&std::fs::read_to_string(e.path()).unwrap_or_default()).entries)
        .map(|(h, v, k)| (h, v, k.to_lowercase()))
        .collect()
}

/// Asks for an installer with the standard Open dialog.
pub fn pick_installer() -> Option<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    let filter: Vec<u16> = "Installers (*.exe, *.msi)\0*.exe;*.msi\0All files\0*.*\0\0".encode_utf16().collect();
    let title = wide("Choose an installer to trace");
    let mut file = vec![0u16; 4096];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let end = file.iter().position(|&c| c == 0).unwrap_or(0);
    Some(PathBuf::from(String::from_utf16_lossy(&file[..end])))
}

/// Command line that runs an installer file.
pub fn install_command(p: &Path) -> String {
    let is_msi = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("msi"));
    if is_msi {
        format!("msiexec.exe /i \"{}\"", p.display())
    } else {
        format!("\"{}\"", p.display())
    }
}

/// The recorded trace of a program's install, if there is one.
pub fn find(app: &App) -> Option<Trace> {
    let rd = std::fs::read_dir(dir()).ok()?;
    rd.flatten()
        .map(|e| parse(&std::fs::read_to_string(e.path()).unwrap_or_default()))
        .find(|t| t.entries.iter().any(|(h, v, k)| *h == app.hive && *v == app.view && k.eq_ignore_ascii_case(&app.key_name)))
}

/// Leftovers from the program's install trace that still exist. Items in the
/// install folder or carrying the program's name are strong evidence.
pub fn leftovers(app: &App, names: &[String], location: Option<&str>) -> Vec<Leftover> {
    let Some(t) = find(app) else { return Vec::new() };
    let named = |s: &str| {
        let low = s.to_lowercase();
        low.split('\\').any(|part| names.contains(&crate::leftovers::compact(part))) || location.is_some_and(|l| low.starts_with(l))
    };
    let mut out = Vec::new();
    for f in t.files.iter().filter(|f| f.exists() && !crate::leftovers::is_protected(f)) {
        let c = if named(&f.to_string_lossy()) { Confidence::High } else { Confidence::Medium };
        out.push(Leftover::new(Kind::Path { path: f.clone(), dir: f.is_dir() }, c, "Created while the program was installed"));
    }
    for (h, v, k) in t.keys.iter().filter(|(h, v, k)| reg::key_exists(*h, *v, k)) {
        let c = if named(k) { Confidence::High } else { Confidence::Medium };
        out.push(Leftover::new(
            Kind::Key { hive: *h, view: *v, path: k.clone() },
            c,
            "Registry key created while the program was installed",
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_children() {
        let v = collapse(vec![r"C:\A\Foo\x.dll".into(), r"C:\A\Foo".into(), r"C:\A\Foobar".into(), r"C:\B".into()]);
        assert_eq!(v, vec![r"C:\A\Foo".to_string(), r"C:\A\Foobar".to_string(), r"C:\B".to_string()]);
    }

    /// Runs only with administrator rights, which reading the journal needs.
    #[test]
    fn records_created_files_and_keys() {
        if !crate::remove::is_admin() {
            return;
        }
        let snap = begin().expect("journal");
        let home = PathBuf::from(std::env::var("USERPROFILE").unwrap());
        let dir = home.join(format!("nietono-trace-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("a.txt"), "x").unwrap();
        let key = format!("SOFTWARE\\NietonoTraceTest{}", std::process::id());
        std::process::Command::new("reg.exe").args(["add", &format!("HKCU\\{key}\\Inner"), "/f"]).output().unwrap();

        let t = finish(snap);
        let _ = std::fs::remove_dir_all(&dir);
        reg::delete_tree(Hive::Hkcu, View::V64, &key).unwrap();

        assert!(t.files.iter().any(|f| f.to_string_lossy().eq_ignore_ascii_case(&dir.to_string_lossy())), "{:?}", t.files);
        assert!(!t.files.iter().any(|f| f.starts_with(&dir) && *f != dir), "children collapse into the folder");
        assert!(t.keys.iter().any(|(h, _, k)| *h == Hive::Hkcu && k.eq_ignore_ascii_case(&key)), "{:?}", t.keys);
    }

    #[test]
    fn saved_traces_parse_back() {
        let text = "entry\tHKLM\t32\tFoo_is1\nfile\tC:\\Program Files\\Foo\nkey\tHKCU\t64\tsoftware\\foo\n";
        let t = parse(text);
        assert_eq!(t.entries, vec![(Hive::Hklm, View::V32, "Foo_is1".to_string())]);
        assert_eq!(t.files, vec![PathBuf::from(r"C:\Program Files\Foo")]);
        assert_eq!(t.keys, vec![(Hive::Hkcu, View::V64, "software\\foo".to_string())]);
    }
}
