#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apps;
mod backup;
mod deep;
mod icons;
mod leftovers;
mod pkgmgr;
mod procs;
mod reg;
mod remove;
mod store;
mod theme;
mod trace;
mod ui;

use eframe::egui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        unsafe { windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS) };
        cli(&args);
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Nietono")
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([960.0, 520.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native("Nietono", options, Box::new(|cc| Ok(Box::new(ui::UiApp::new(cc)))))
}

const USAGE: &str = "usage:
  nietono --list
  nietono --scan <name> [--deep]
  nietono --orphans
  nietono --uninstall <name> [--quiet] [--force] [--yes]

--scan and --orphans only list. --uninstall runs the uninstaller, then
scans. With --yes it also removes the checked traces into a backup.
--force skips the uninstaller.";

/// Best match for a name: exact, then prefix, then substring. Uninstall
/// entries are tried before packages, since Notepad++ for example also
/// ships a Store package named Notepad++ for its context menu.
fn find<'a>(list: &'a [apps::App], q: &str) -> Option<&'a apps::App> {
    let tests: [&dyn Fn(&str) -> bool; 3] = [&|n| n == q, &|n| n.starts_with(q), &|n| n.contains(q)];
    let pick = |registry_only: bool| {
        tests.iter().find_map(|t| list.iter().find(|a| (!registry_only || a.source == apps::Source::Registry) && t(&a.name.to_lowercase())))
    };
    pick(true).or_else(|| pick(false))
}

fn print_items(items: &[leftovers::Leftover]) {
    for l in items {
        let mark = if l.checked { "[x]" } else { "[ ]" };
        println!("{mark} {:?}\t{}\t{}\t{}", l.confidence, l.label(), l.reason, ui::human(l.size));
    }
}

/// Command line mode, for scripts and tests.
fn cli(args: &[String]) {
    let t = std::time::Instant::now();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let query = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect::<Vec<_>>().join(" ").to_lowercase();
    let list = apps::load();
    match args[0].as_str() {
        "--list" => {
            for a in &list {
                let loc = a.location.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                println!("{}\t{}\t{}\t{}\t{}", a.name, a.version, a.publisher, a.scope(), loc);
            }
            eprintln!("{} programs, {:?}", list.len(), t.elapsed());
        }
        "--scan" if !query.is_empty() => {
            let depth = if flag("--deep") { leftovers::Depth::Deep } else { leftovers::Depth::Normal };
            let Some(app) = find(&list, &query) else {
                eprintln!("No program matches: {query}");
                std::process::exit(2);
            };
            let others: Vec<_> = list.iter().filter(|a| !std::ptr::eq(*a, app)).cloned().collect();
            println!("{} {}", app.name, app.version);
            println!("names: {:?}  publisher: {:?}", leftovers::name_variants(app), leftovers::publisher_variants(app));
            print_items(&leftovers::scan(app, &others, depth));
            eprintln!("{:?}", t.elapsed());
        }
        "--orphans" => {
            for l in leftovers::orphans(&list) {
                println!("{}\t{}", l.label(), ui::human(l.size));
            }
            eprintln!("{:?}", t.elapsed());
        }
        "--uninstall" if !query.is_empty() => {
            let Some(app) = find(&list, &query) else {
                eprintln!("No program matches: {query}");
                std::process::exit(2);
            };
            let others: Vec<_> = list.iter().filter(|a| !std::ptr::eq(*a, app)).cloned().collect();
            println!("{} {}", app.name, app.version);
            if !flag("--force") {
                let stop = std::sync::atomic::AtomicBool::new(false);
                let res = match &app.source {
                    apps::Source::Store { full_name, .. } => store::remove(full_name),
                    _ => match app.command(flag("--quiet") && app.can_be_quiet()) {
                        Some(cmd) => {
                            println!("running: {cmd}");
                            remove::run_uninstaller(&cmd, &stop)
                        }
                        None => Err("no uninstall command".into()),
                    },
                };
                if let Err(e) = res {
                    eprintln!("uninstaller: {e}");
                    std::process::exit(1);
                }
                if app.still_installed() {
                    eprintln!("still installed after the uninstaller ran");
                    std::process::exit(1);
                }
            }
            let items = leftovers::scan(app, &others, leftovers::Depth::Normal);
            print_items(&items);
            if flag("--yes") {
                let chosen: Vec<_> = items.into_iter().filter(|l| l.checked).collect();
                let r = remove::clean(&app.name, &chosen);
                println!("removed {}, backup {}", r.removed, r.backup.display());
                for f in &r.failed {
                    eprintln!("failed: {}: {}", f.label, f.reason);
                }
                if !r.failed.is_empty() {
                    std::process::exit(1);
                }
            }
            eprintln!("{:?}", t.elapsed());
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}
