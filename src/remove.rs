//! Runs uninstallers and removes traces in a reversible way.

use crate::apps::split_command;
use crate::leftovers::{Kind, Leftover};
use crate::backup;
use crate::procs::{self, Proc};
use crate::reg::{self, wide, Hive, View};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_ELEVATION_REQUIRED: u32 = 740;

pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

/// Restarts this program elevated. On success the caller should exit.
pub fn relaunch_as_admin() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let exe = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let r = unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), exe.as_ptr(), null(), null(), SW_SHOWNORMAL) };
    r as isize > 32
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Runs an uninstaller and waits until it and every process it started have exited.
/// Uninstallers that copy themselves to a temp folder and relaunch, like NSIS,
/// are therefore not mistaken for finished. Stops waiting when `stop` is set.
pub fn run_uninstaller(cmd: &str, stop: &AtomicBool) -> Result<(), String> {
    let (exe, args) = split_command(cmd).ok_or("Could not parse the uninstall command")?;
    let line = if args.is_empty() { format!("\"{exe}\"") } else { format!("\"{exe}\" {args}") };
    let mut line_w = wide(&line);

    unsafe {
        let job = Handle(CreateJobObjectW(null(), null()));
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(null(), line_w.as_mut_ptr(), null(), null(), 0, CREATE_SUSPENDED, null(), null(), &si, &mut pi);
        if ok == 0 {
            let err = GetLastError();
            if err == ERROR_ELEVATION_REQUIRED {
                return run_elevated(&exe, &args, stop);
            }
            return Err(format!("Could not start the uninstaller, error {err}"));
        }
        let process = Handle(pi.hProcess);
        let thread = Handle(pi.hThread);
        let in_job = !job.0.is_null() && AssignProcessToJobObject(job.0, process.0) != 0;
        ResumeThread(thread.0);
        drop(thread);

        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let done = if in_job {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                let ok = QueryInformationJobObject(
                    job.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut _,
                    std::mem::size_of_val(&info) as u32,
                    null_mut(),
                );
                ok == 0 || info.ActiveProcesses == 0
            } else {
                WaitForSingleObject(process.0, 0) == WAIT_OBJECT_0
            };
            if done {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn run_elevated(exe: &str, args: &str, stop: &AtomicBool) -> Result<(), String> {
    let verb = wide("runas");
    let file = wide(exe);
    let params = wide(args);
    unsafe {
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = if args.is_empty() { null() } else { params.as_ptr() };
        sei.nShow = SW_SHOWNORMAL;
        if ShellExecuteExW(&mut sei) == 0 {
            return Err("The uninstaller needs administrator rights".into());
        }
        if sei.hProcess.is_null() {
            return Ok(());
        }
        let p = Handle(sei.hProcess);
        while WaitForSingleObject(p.0, 250) != WAIT_OBJECT_0 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
        }
    }
    Ok(())
}

pub struct Failure {
    pub label: String,
    pub reason: String,
    /// The file or folder that stayed, so it can be retried or queued for restart.
    pub item: Option<Leftover>,
    /// Processes holding files in it open.
    pub lockers: Vec<Proc>,
}

impl Failure {
    fn new(label: String, reason: String) -> Self {
        Failure { label, reason, item: None, lockers: Vec::new() }
    }
}

pub struct Report {
    pub removed: usize,
    pub failed: Vec<Failure>,
    pub backup: PathBuf,
}

/// Removes the selected traces. Everything goes into a backup folder first:
/// files are moved there, registry keys and tasks are exported.
pub fn clean(app_name: &str, items: &[Leftover]) -> Report {
    let backup = backup::create(app_name);
    let mut report = Report { removed: 0, failed: Vec::new(), backup: backup.clone() };

    let mut paths: Vec<&Leftover> = Vec::new();
    let mut rules: Vec<(&str, String)> = Vec::new();
    let mut env_changed = false;
    for (i, l) in items.iter().enumerate() {
        let res = match &l.kind {
            Kind::Path { .. } => {
                paths.push(l);
                continue;
            }
            Kind::Key { hive, view, path } => export_key(&reg::display_path(*hive, *view, path), view.reg_switch(), &backup, i)
                .and_then(|_| reg::delete_tree(*hive, *view, path).map_err(win_err)),
            Kind::Value { hive, view, path, name } => {
                backup_value(*hive, *view, path, name, &backup, i).and_then(|_| reg::delete_value(*hive, *view, path, name).map_err(win_err))
            }
            Kind::Task { name } => delete_task(name, &backup, i),
            Kind::Service { name } => delete_service(name, &backup, i),
            Kind::Firewall { id, .. } => match backup_value(Hive::Hklm, View::V64, FIREWALL, id, &backup, i) {
                Ok(()) => {
                    rules.push((id, l.label()));
                    continue;
                }
                Err(e) => Err(e),
            },
            Kind::PathEntry { hive, entry } => {
                env_changed = true;
                remove_path_entry(*hive, entry, &backup, i)
            }
        };
        match res {
            Ok(()) => report.removed += 1,
            Err(e) => report.failed.push(Failure::new(l.label(), e)),
        }
    }

    if !rules.is_empty() {
        let ids: Vec<&str> = rules.iter().map(|(id, _)| *id).collect();
        let failed = delete_firewall_rules(&ids);
        for (id, label) in rules {
            if failed.contains(&id) {
                report.failed.push(Failure::new(label, "Could not remove the firewall rule, run as administrator".into()));
            } else {
                report.removed += 1;
            }
        }
    }
    if env_changed {
        broadcast_environment();
    }

    let mut gone: Vec<&Path> = Vec::new();
    for (i, l) in paths.iter().enumerate() {
        let Kind::Path { path: p, .. } = &l.kind else { continue };
        if !p.exists() {
            report.removed += 1;
            continue;
        }
        let res = match backup::store(&backup, i, p) {
            Ok(_) if p.exists() => Err("Some files are in use".to_string()),
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        };
        match res {
            Ok(()) => {
                report.removed += 1;
                gone.push(p);
            }
            Err(e) => {
                let lockers = procs::lockers(std::slice::from_ref(p));
                let reason = if lockers.is_empty() {
                    e
                } else {
                    let names: Vec<&str> = lockers.iter().map(|p| p.name.as_str()).collect();
                    format!("In use by {}", names.join(", "))
                };
                report.failed.push(Failure { label: l.label(), reason, item: Some((*l).clone()), lockers });
            }
        }
    }
    remove_empty_parents(&gone);

    if !backup.join("manifest.txt").exists() {
        let _ = std::fs::remove_dir_all(&backup);
    }
    report
}

/// Queues files that could not be removed for deletion at the next restart.
pub fn remove_at_restart(items: &[Leftover]) -> Vec<Failure> {
    items
        .iter()
        .filter_map(|l| {
            let Kind::Path { path, .. } = &l.kind else { return None };
            procs::delete_at_restart(path).err().map(|e| Failure::new(l.label(), e))
        })
        .collect()
}

fn win_err(code: u32) -> String {
    match code {
        5 => "Access denied, run as administrator".into(),
        c => format!("Error {c}"),
    }
}

fn hidden(cmd: &str) -> Command {
    let mut c = Command::new(cmd);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}

fn export_key(full: &str, switch: &str, dir: &Path, i: usize) -> Result<(), String> {
    let name = format!("{i:03}.reg");
    let out = hidden("reg.exe")
        .args(["export", full])
        .arg(dir.join(&name))
        .args(["/y", switch])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        backup::note(dir, &["reg", &name]);
        Ok(())
    } else {
        Err("Backup failed, nothing was deleted".into())
    }
}

const FIREWALL: &str = "SYSTEM\\CurrentControlSet\\Services\\SharedAccess\\Parameters\\FirewallPolicy\\FirewallRules";

fn backup_value(hive: Hive, view: View, path: &str, name: &str, dir: &Path, i: usize) -> Result<(), String> {
    let Some(text) = reg::value_to_reg(hive, view, path, name) else {
        return Err("Backup failed, nothing was deleted".into());
    };
    let file = format!("{i:03}.reg");
    reg::write_reg_file(&dir.join(&file), &text).map_err(|e| e.to_string())?;
    backup::note(dir, &["reg", &file]);
    Ok(())
}

fn delete_service(name: &str, dir: &Path, i: usize) -> Result<(), String> {
    let key = format!("HKLM\\SYSTEM\\CurrentControlSet\\Services\\{name}");
    export_key(&key, "/reg:64", dir, i)?;
    let _ = hidden("sc.exe").args(["stop", name]).output();
    let d = hidden("sc.exe").args(["delete", name]).output().map_err(|e| e.to_string())?;
    if d.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&d.stdout).lines().last().unwrap_or("Could not delete the service").trim().to_string())
    }
}

/// Removes rules through the firewall service so it notices at once.
/// Returns the ids that are still present afterwards.
fn delete_firewall_rules<'a>(ids: &[&'a str]) -> Vec<&'a str> {
    let list: Vec<String> = ids.iter().map(|id| format!("'{}'", id.replace('\'', "''"))).collect();
    let script = format!("Remove-NetFirewallRule -Name {} -ErrorAction SilentlyContinue", list.join(","));
    let _ = hidden("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", &script]).output();
    let Some(k) = reg::Key::open(Hive::Hklm, View::V64, FIREWALL) else { return Vec::new() };
    ids.iter().copied().filter(|id| k.raw(id).is_some()).collect()
}

fn remove_path_entry(hive: Hive, entry: &str, dir: &Path, i: usize) -> Result<(), String> {
    let (_, key) = crate::deep::ENV_KEYS.iter().find(|(h, _)| *h == hive).ok_or("No environment key")?;
    let k = reg::Key::open(hive, View::V64, key).ok_or("Could not open the environment key")?;
    let (ty, _) = k.raw("Path").ok_or("PATH is empty")?;
    let Some(reg::Value::Str(path)) = k.get("Path") else { return Err("PATH is empty".into()) };
    backup_value(hive, View::V64, key, "Path", dir, i)?;
    let kept: Vec<&str> = path.split(';').filter(|e| !e.is_empty() && *e != entry).collect();
    let expand = ty == windows_sys::Win32::System::Registry::REG_EXPAND_SZ;
    reg::set_string(hive, View::V64, key, "Path", &kept.join(";"), expand).map_err(win_err)
}

/// Tells Explorer and other programs that environment variables changed.
fn broadcast_environment() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE};
    let env = wide("Environment");
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 2000, &mut result);
    }
}

fn delete_task(name: &str, dir: &Path, i: usize) -> Result<(), String> {
    let q = hidden("schtasks.exe").args(["/Query", "/TN", name, "/XML"]).output().map_err(|e| e.to_string())?;
    if !q.status.success() {
        return Err("Could not read the task, run as administrator".into());
    }
    let file = format!("{i:03}-task.xml");
    std::fs::write(dir.join(&file), &q.stdout).map_err(|e| e.to_string())?;
    let d = hidden("schtasks.exe").args(["/Delete", "/TN", name, "/F"]).output().map_err(|e| e.to_string())?;
    if d.status.success() {
        backup::note(dir, &["task", &file, name]);
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&d.stderr).trim().to_string())
    }
}

pub fn recycle(p: &Path) -> Result<(), String> {
    let mut from = wide(&p.to_string_lossy());
    from.push(0);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16;
    let r = unsafe { SHFileOperationW(&mut op) };
    if r == 0 && op.fAnyOperationsAborted == 0 {
        Ok(())
    } else {
        Err(match r {
            5 | 0x78 => "Access denied, run as administrator".into(),
            0x20 => "File is in use, close the program".into(),
            c => format!("Hata {c:#x}"),
        })
    }
}

/// Removes parent folders left empty by the cleanup. Protected paths are kept.
fn remove_empty_parents(paths: &[&Path]) {
    for p in paths {
        let mut cur = p.parent();
        while let Some(dir) = cur {
            if crate::leftovers::is_protected(dir) || std::fs::remove_dir(dir).is_err() {
                break;
            }
            cur = dir.parent();
        }
    }
}

/// Opens the folder in Explorer.
pub fn reveal(p: &Path) {
    let _ = Command::new("explorer.exe").arg(p).spawn();
}
