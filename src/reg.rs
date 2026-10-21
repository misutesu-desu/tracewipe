//! Thin registry layer over windows-sys.

use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hive {
    Hklm,
    Hkcu,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum View {
    V64,
    V32,
}

impl Hive {
    fn raw(self) -> HKEY {
        match self {
            Hive::Hklm => HKEY_LOCAL_MACHINE,
            Hive::Hkcu => HKEY_CURRENT_USER,
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            Hive::Hklm => "HKLM",
            Hive::Hkcu => "HKCU",
        }
    }
}

impl View {
    fn flag(self) -> u32 {
        match self {
            View::V64 => KEY_WOW64_64KEY,
            View::V32 => KEY_WOW64_32KEY,
        }
    }
    pub fn reg_switch(self) -> &'static str {
        match self {
            View::V64 => "/reg:64",
            View::V32 => "/reg:32",
        }
    }
}

/// Full path as shown to the user. The 32-bit view is written as WOW6432Node.
pub fn display_path(hive: Hive, view: View, path: &str) -> String {
    if hive == Hive::Hklm && view == View::V32 {
        if let Some(rest) = strip_prefix_ci(path, "SOFTWARE\\") {
            return format!("HKLM\\SOFTWARE\\WOW6432Node\\{rest}");
        }
    }
    format!("{}\\{}", hive.short(), path)
}

fn strip_prefix_ci<'a>(s: &'a str, p: &str) -> Option<&'a str> {
    if s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p) {
        Some(&s[p.len()..])
    } else {
        None
    }
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

pub enum Value {
    Str(String),
    Dword(u32),
    Other,
}

impl Key {
    pub fn open(hive: Hive, view: View, path: &str) -> Option<Key> {
        Self::open_with(hive, view, path, KEY_READ)
    }

    fn open_with(hive: Hive, view: View, path: &str, access: u32) -> Option<Key> {
        let mut h: HKEY = null_mut();
        let p = wide(path);
        let r = unsafe { RegOpenKeyExW(hive.raw(), p.as_ptr(), 0, access | view.flag(), &mut h) };
        (r == ERROR_SUCCESS).then_some(Key(h))
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = [0u16; 256];
        let mut i = 0;
        loop {
            let mut len = buf.len() as u32;
            let r = unsafe {
                RegEnumKeyExW(self.0, i, buf.as_mut_ptr(), &mut len, null_mut(), null_mut(), null_mut(), null_mut())
            };
            if r != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
            i += 1;
        }
        out
    }

    /// Every value name, with its data when it is a string.
    pub fn values(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        let mut name = vec![0u16; 16384];
        let mut data: Vec<u8> = vec![0; 4096];
        let mut i = 0;
        loop {
            let mut nlen = name.len() as u32;
            let mut dlen = data.len() as u32;
            let mut ty = 0u32;
            let r = unsafe {
                RegEnumValueW(self.0, i, name.as_mut_ptr(), &mut nlen, null_mut(), &mut ty, data.as_mut_ptr(), &mut dlen)
            };
            if r == ERROR_MORE_DATA {
                data.resize(dlen as usize + 2, 0);
                continue;
            }
            if r != ERROR_SUCCESS {
                break;
            }
            let n = String::from_utf16_lossy(&name[..nlen as usize]);
            out.push((n, decode(ty, &data[..dlen as usize])));
            i += 1;
        }
        out
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let n = wide(name);
        let mut ty = 0u32;
        let mut len = 0u32;
        let r = unsafe { RegQueryValueExW(self.0, n.as_ptr(), null_mut(), &mut ty, null_mut(), &mut len) };
        if r != ERROR_SUCCESS {
            return None;
        }
        let mut data = vec![0u8; len as usize + 2];
        let r = unsafe { RegQueryValueExW(self.0, n.as_ptr(), null_mut(), &mut ty, data.as_mut_ptr(), &mut len) };
        if r != ERROR_SUCCESS {
            return None;
        }
        Some(decode(ty, &data[..len as usize]))
    }

    pub fn string(&self, name: &str) -> Option<String> {
        match self.get(name)? {
            Value::Str(s) => {
                let s = s.trim().to_string();
                (!s.is_empty()).then_some(s)
            }
            Value::Dword(d) => Some(d.to_string()),
            Value::Other => None,
        }
    }

    pub fn dword(&self, name: &str) -> Option<u32> {
        match self.get(name)? {
            Value::Dword(d) => Some(d),
            Value::Str(s) => s.trim().parse().ok(),
            Value::Other => None,
        }
    }
}

fn decode(ty: u32, data: &[u8]) -> Value {
    match ty {
        REG_SZ | REG_EXPAND_SZ => {
            let u: Vec<u16> = data.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let end = u.iter().position(|&c| c == 0).unwrap_or(u.len());
            Value::Str(String::from_utf16_lossy(&u[..end]))
        }
        REG_DWORD if data.len() >= 4 => Value::Dword(u32::from_le_bytes([data[0], data[1], data[2], data[3]])),
        _ => Value::Other,
    }
}

pub fn key_exists(hive: Hive, view: View, path: &str) -> bool {
    Key::open(hive, view, path).is_some()
}

/// Deletes a key with all of its subkeys. A missing key counts as success.
pub fn delete_tree(hive: Hive, view: View, path: &str) -> Result<(), u32> {
    let Some(k) = Key::open_with(hive, view, path, KEY_ALL_ACCESS) else {
        return if key_exists(hive, view, path) { Err(5) } else { Ok(()) };
    };
    let r = unsafe { RegDeleteTreeW(k.0, std::ptr::null()) };
    drop(k);
    if r != ERROR_SUCCESS {
        return Err(r);
    }
    let p = wide(path);
    let r = unsafe { RegDeleteKeyExW(hive.raw(), p.as_ptr(), view.flag(), 0) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(r)
    }
}

pub fn delete_value(hive: Hive, view: View, path: &str, name: &str) -> Result<(), u32> {
    let k = Key::open_with(hive, view, path, KEY_SET_VALUE).ok_or(5u32)?;
    let n = wide(name);
    let r = unsafe { RegDeleteValueW(k.0, n.as_ptr()) };
    if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(r)
    }
}
