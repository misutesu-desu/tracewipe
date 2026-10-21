<p align="center">
  <img src="assets/icon.png" width="112" alt="Nietono icon">
</p>

<h1 align="center">Nietono</h1>

<p align="center">
  An uninstaller for Windows 10 and 11 that also removes what programs leave behind.<br>
  Everything it deletes goes into a backup you can restore for seven days.
</p>

<p align="center">
  <a href="https://github.com/misutesu-desu/nietono/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/misutesu-desu/nietono/actions/workflows/ci.yml/badge.svg"></a>
  <a href="LICENSE"><img alt="GPL-3.0-or-later" src="https://img.shields.io/github/license/misutesu-desu/nietono"></a>
</p>

<p align="center">
  <a href="https://misutesu-desu.github.io/nietono/">Website</a>
  · <a href="#how-a-removal-works">How it works</a>
  · <a href="#command-line">Command line</a>
  · <a href="#building">Building</a>
  · <a href="#türkçe">Türkçe</a>
</p>

Nietono is a single executable of about 5 MB, written in Rust. It lists installed
programs, runs their own uninstaller, waits for it to really finish and then
looks for the folders, registry keys, services, tasks and other entries the
uninstaller did not remove. You see the list before anything is deleted.

## What it finds

Programs come from four places: the Uninstall registry keys, Microsoft Store
packages, Scoop and Chocolatey. On a machine with 155 programs the list is ready
in about 25 ms and Store packages join it a tenth of a second later.

After an uninstall, Nietono looks for:

| Where | What counts as a trace |
|---|---|
| Program Files, ProgramData, AppData | Folders named after the program or inside its publisher's folder |
| Profile | Dot folders such as `.rustup`, Saved Games, Documents\My Games |
| Start menu and desktop | Shortcuts named after the program or pointing into its folder |
| Registry | `SOFTWARE` keys, Run entries, App Paths, file types, COM classes |
| System | Services, firewall rules, scheduled tasks, PATH entries |
| Caches | Crash reports, network tracing keys, MUICache and compatibility entries |

A scan takes about 0.3 seconds on the same machine.

## How a removal works

1. If the program is running, Nietono offers to close it first.
2. The program's uninstaller runs inside a job object. Nietono waits until the
   uninstaller and every process it started have exited, so NSIS uninstallers
   that copy themselves to a temp folder are not mistaken for finished.
3. The leftover scan runs. Items with strong evidence come checked. Weaker
   matches come unchecked with the reason next to them.
4. You remove what you selected. Files are moved into the backup folder,
   registry keys and tasks are exported there first.

Several programs can be removed in one go. Their uninstallers run one after
another, quietly where the installer type allows it, and one scan covers all of
them.

## Safety

- Matching is conservative. A folder or key must carry the program's exact name,
  or its content must point into the program's install folder.
- A folder that another installed program lives in, or that shares its name with
  another program, is never checked by default. Steam and the games inside its
  folder are kept apart this way.
- Windows, Program Files itself, the profile folders and the package managers'
  own folders are never offered.
- Every removal can be undone from **More > Backups** for seven days.
- Files that are in use show which program holds them. Nietono can close that
  program and retry, or remove the files at the next restart.
- An optional System Restore point can be created before uninstalling.

## Install tracing

**More > Install with tracing** runs an installer and records what it creates.
Files come from the NTFS change journal, so nothing hooks into the installer.
Registry keys come from comparing key names before and after. When that program
is removed later, the trace makes the leftover scan complete. Tracing needs
administrator rights.

## Other tools

- **More > Unclaimed folders** lists folders in the program locations that no
  installed program claims and that nothing has written to for a month.
- **More > Pick from open windows** finds the program behind a window.
- The scan depth can be set to Safe, Normal or Deep.

## Command line

The same executable works from a terminal:

```text
nietono --list
nietono --scan <name> [--deep]
nietono --orphans
nietono --uninstall <name> [--quiet] [--force] [--yes]
```

`--scan` and `--orphans` only list. `--uninstall` runs the uninstaller and scans,
and with `--yes` removes the checked traces into a backup. `--force` skips the
uninstaller. Exit codes report failures.

## Building

You need stable Rust and the Windows SDK, which comes with Visual Studio
Build Tools.

```bash
cargo build --release
```

The executable is `target\release\nietono.exe`. Run it as administrator to
remove traces under Program Files and HKLM.

## Testing

```bash
cargo test
```

`tests/integration.ps1` installs an NSIS, an Inno Setup and an MSI program,
removes them with Nietono and checks the result. CI runs it on a fresh GitHub
runner. To run it locally, use `tests/sandbox/nietono.wsb`, which starts it
inside Windows Sandbox.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

## Türkçe

Nietono, Windows için Rust ile yazılmış bir kaldırıcı. Programın kendi
kaldırıcısını çalıştırır, bitmesini bekler, sonra geride kalan klasör, kayıt
defteri anahtarı, servis, zamanlanmış görev ve diğer izleri bulur. Silmeden önce
listeyi görürsünüz. Silinen her şey yedek klasörüne gider ve yedi gün boyunca
**More > Backups** ekranından geri yüklenebilir.
