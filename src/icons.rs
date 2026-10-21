//! Program icons, read from executables and .ico files as RGBA pixels.

use crate::reg::wide;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Shell::ExtractIconExW;
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

pub struct Rgba {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// Splits a DisplayIcon value such as `"C:\x\app.exe",-101` into path and index.
pub fn parse(value: &str) -> Option<(PathBuf, i32)> {
    let v = value.trim();
    let (path, index) = match v.rsplit_once(',') {
        Some((p, i)) if i.trim().parse::<i32>().is_ok() => (p, i.trim().parse().unwrap_or(0)),
        _ => (v, 0),
    };
    let path = crate::reg::expand_env(path.trim().trim_matches('"'));
    (!path.is_empty()).then(|| (PathBuf::from(path), index))
}

/// The 32 pixel icon at `index` in the file.
pub fn load(path: &Path, index: i32) -> Option<Rgba> {
    let w = wide(&path.to_string_lossy());
    let mut large: HICON = std::ptr::null_mut();
    unsafe {
        if ExtractIconExW(w.as_ptr(), index, &mut large, std::ptr::null_mut(), 1) == 0 || large.is_null() {
            return None;
        }
        let out = to_rgba(large);
        DestroyIcon(large);
        out
    }
}

unsafe fn bitmap_bits(dc: HDC, bm: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = width;
    // Negative height asks for rows from the top down.
    bmi.bmiHeader.biHeight = -height;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;
    let mut buf = vec![0u8; (width * height * 4) as usize];
    let lines = GetDIBits(dc, bm, 0, height as u32, buf.as_mut_ptr() as *mut _, &mut bmi, DIB_RGB_COLORS);
    (lines == height).then_some(buf)
}

unsafe fn to_rgba(icon: HICON) -> Option<Rgba> {
    let mut info: ICONINFO = std::mem::zeroed();
    if GetIconInfo(icon, &mut info) == 0 {
        return None;
    }
    let cleanup = |info: &ICONINFO| {
        if !info.hbmColor.is_null() {
            DeleteObject(info.hbmColor as HGDIOBJ);
        }
        if !info.hbmMask.is_null() {
            DeleteObject(info.hbmMask as HGDIOBJ);
        }
    };
    if info.hbmColor.is_null() {
        cleanup(&info);
        return None;
    }
    let mut bm: BITMAP = std::mem::zeroed();
    GetObjectW(info.hbmColor as HGDIOBJ, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _);
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    let dc = CreateCompatibleDC(std::ptr::null_mut());
    let color = bitmap_bits(dc, info.hbmColor, w, h);
    let mask = bitmap_bits(dc, info.hbmMask, w, h);
    DeleteDC(dc);
    cleanup(&info);
    let mut px = color?;
    // Old icons carry no alpha; their transparency lives in the mask.
    let has_alpha = px.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
    for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        p.swap(0, 2);
        if !has_alpha {
            let transparent = mask.as_ref().is_some_and(|m| m[i * 4] != 0);
            p[3] = if transparent { 0 } else { 255 };
        }
    }
    Some(Rgba { width: w as usize, height: h as usize, pixels: px })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_display_icon() {
        assert_eq!(parse(r#""C:\a b\x.exe",-101"#), Some((PathBuf::from(r"C:\a b\x.exe"), -101)));
        assert_eq!(parse(r"C:\x.ico"), Some((PathBuf::from(r"C:\x.ico"), 0)));
    }

    #[test]
    fn reads_an_icon_from_explorer() {
        let windir = std::env::var("SystemRoot").unwrap();
        let icon = load(&Path::new(&windir).join("explorer.exe"), 0).expect("explorer icon");
        assert_eq!(icon.pixels.len(), icon.width * icon.height * 4);
        assert!(icon.pixels.as_chunks::<4>().0.iter().any(|p| p[3] == 255));
    }
}
