//! Microsoft Store and other packaged (MSIX/AppX) apps of the current user.

use crate::apps::{App, Installer, Source};
use crate::reg::{Hive, View};
use std::path::PathBuf;
use windows::core::HSTRING;
use windows::Management::Deployment::PackageManager;

/// Seconds between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF: i64 = 11_644_473_600;

fn date(universal_time: i64) -> String {
    let secs = universal_time / 10_000_000 - EPOCH_DIFF;
    if secs <= 0 {
        return String::new();
    }
    // Civil date from days since 1970, after Howard Hinnant's algorithm.
    let z = secs / 86_400 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn load() -> Vec<App> {
    let Ok(pm) = PackageManager::new() else { return Vec::new() };
    let Ok(packages) = pm.FindPackagesByUserSecurityId(&HSTRING::new()) else { return Vec::new() };
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()).to_lowercase();
    let mut out = Vec::new();
    for p in packages {
        let skip = p.IsFramework().unwrap_or(true) || p.IsResourcePackage().unwrap_or(true) || p.IsStub().unwrap_or(false);
        if skip {
            continue;
        }
        let Ok(id) = p.Id() else { continue };
        let path = p.InstalledPath().map(|h| h.to_string()).unwrap_or_default();
        // Packages under the Windows folder are part of Windows and cannot be removed.
        if path.to_lowercase().starts_with(&windir) {
            continue;
        }
        let name = p.DisplayName().map(|h| h.to_string()).unwrap_or_default();
        let package = id.Name().map(|h| h.to_string()).unwrap_or_default();
        // Unresolved resource strings look like ms-resource:AppName.
        let name = if name.is_empty() || name.starts_with("ms-resource:") { package.clone() } else { name };
        let version = id.Version().map(|v| format!("{}.{}.{}.{}", v.Major, v.Minor, v.Build, v.Revision)).unwrap_or_default();
        out.push(App {
            name,
            version,
            publisher: p.PublisherDisplayName().map(|h| h.to_string()).unwrap_or_default(),
            install_date: p.InstalledDate().map(|d| date(d.UniversalTime)).unwrap_or_default(),
            size_kb: 0,
            location: (!path.is_empty()).then(|| PathBuf::from(&path)),
            location_guessed: false,
            uninstall: None,
            quiet_uninstall: None,
            msi: false,
            hive: Hive::Hkcu,
            view: View::V64,
            key_name: package,
            installer: Installer::Unknown,
            source: Source::Store {
                full_name: id.FullName().map(|h| h.to_string()).unwrap_or_default(),
                family: id.FamilyName().map(|h| h.to_string()).unwrap_or_default(),
            },
        });
    }
    out
}

pub fn is_installed(family: &str) -> bool {
    let Ok(pm) = PackageManager::new() else { return false };
    pm.FindPackagesByUserSecurityIdPackageFamilyName(&HSTRING::new(), &HSTRING::from(family))
        .map(|it| it.into_iter().next().is_some())
        .unwrap_or(false)
}

/// Removes the package for the current user and waits for it to finish.
pub fn remove(full_name: &str) -> Result<(), String> {
    let pm = PackageManager::new().map_err(|e| e.message())?;
    let op = pm.RemovePackageAsync(&HSTRING::from(full_name)).map_err(|e| e.message())?;
    match op.join() {
        Ok(r) => match r.ExtendedErrorCode() {
            Ok(code) if code.is_err() => Err(r.ErrorText().map(|t| t.to_string()).unwrap_or_else(|_| code.message())),
            _ => Ok(()),
        },
        Err(e) => Err(e.message()),
    }
}

/// Folder where the package keeps its data, under %LOCALAPPDATA%\Packages.
pub fn data_folder(family: &str) -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("Packages").join(family))
}

#[cfg(test)]
mod tests {
    #[test]
    fn converts_dates() {
        // 2024-03-01 00:00:00 UTC in 100 ns ticks since 1601.
        let secs: i64 = 1_709_251_200;
        assert_eq!(super::date((secs + super::EPOCH_DIFF) * 10_000_000), "2024-03-01");
    }
}
