//! Processes: who holds a file open, and ending them.

use crate::reg::wide;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::RestartManager::*;
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
}

/// Files under `p`, at most `limit` of them. Restart Manager needs file paths, not folders.
fn files_under(p: &Path, limit: usize) -> Vec<PathBuf> {
    if p.is_file() {
        return vec![p.to_path_buf()];
    }
    let mut out = Vec::new();
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && !ft.is_symlink() {
                stack.push(e.path());
            } else if ft.is_file() {
                out.push(e.path());
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

/// Processes that keep files under these paths open, found with Restart Manager.
pub fn lockers(paths: &[PathBuf]) -> Vec<Proc> {
    let files: Vec<Vec<u16>> = paths.iter().flat_map(|p| files_under(p, 2000)).map(|f| wide(&f.to_string_lossy())).collect();
    if files.is_empty() {
        return Vec::new();
    }
    let ptrs: Vec<*const u16> = files.iter().map(|f| f.as_ptr()).collect();
    let mut session = 0u32;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    unsafe {
        if RmStartSession(&mut session, 0, key.as_mut_ptr()) != ERROR_SUCCESS {
            return Vec::new();
        }
        let mut out = Vec::new();
        if RmRegisterResources(session, ptrs.len() as u32, ptrs.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null()) == ERROR_SUCCESS {
            let mut needed = 0u32;
            let mut count = 0u32;
            let mut reasons = 0u32;
            let mut info: Vec<RM_PROCESS_INFO> = Vec::new();
            for _ in 0..3 {
                count = info.len() as u32;
                let r = RmGetList(session, &mut needed, &mut count, if info.is_empty() { null_mut() } else { info.as_mut_ptr() }, &mut reasons);
                if r == ERROR_MORE_DATA {
                    info = vec![std::mem::zeroed(); needed as usize + 4];
                    continue;
                }
                if r != ERROR_SUCCESS {
                    count = 0;
                }
                break;
            }
            for i in info.iter().take(count as usize) {
                let end = i.strAppName.iter().position(|&c| c == 0).unwrap_or(i.strAppName.len());
                let p = Proc { pid: i.Process.dwProcessId, name: String::from_utf16_lossy(&i.strAppName[..end]) };
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
        RmEndSession(session);
        out
    }
}

/// Ends the processes. Returns the ones that are still running.
pub fn terminate(procs: &[Proc]) -> Vec<Proc> {
    procs
        .iter()
        .filter(|p| unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, p.pid);
            if h.is_null() {
                return true;
            }
            let ok = TerminateProcess(h, 1) != 0;
            CloseHandle(h);
            !ok
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_this_process_holding_a_file() {
        let dir = std::env::temp_dir().join(format!("tracewipe-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("held.txt");
        let handle = std::fs::File::create(&f).unwrap();
        let found = lockers(&[dir.clone()]);
        assert!(found.iter().any(|p| p.pid == std::process::id()), "{found:?}");
        drop(handle);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Queues a file or folder for deletion at the next restart. Files go first,
/// then folders from the deepest up, since Windows removes only empty folders.
pub fn delete_at_restart(p: &Path) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    if p.is_dir() {
        let mut stack = vec![p.to_path_buf()];
        while let Some(d) = stack.pop() {
            dirs.push(d.clone());
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                match e.file_type() {
                    Ok(ft) if ft.is_dir() && !ft.is_symlink() => stack.push(e.path()),
                    Ok(_) => files.push(e.path()),
                    Err(_) => {}
                }
            }
        }
    } else {
        files.push(p.to_path_buf());
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for f in files.iter().chain(dirs.iter()) {
        let w = wide(&f.to_string_lossy());
        if unsafe { MoveFileExW(w.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) } == 0 {
            return Err("Could not schedule the removal, run as administrator".into());
        }
    }
    Ok(())
}
