#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apps;
mod backup;
mod deep;
mod icons;
mod leftovers;
mod pkgmgr;
mod procs;
mod reg;
mod store;
mod remove;
mod theme;
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
            .with_title("Tracewipe")
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([960.0, 520.0])
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap_or_default()),
        ..Default::default()
    };
    eframe::run_native("Tracewipe", options, Box::new(|cc| Ok(Box::new(ui::UiApp::new(cc)))))
}

/// Command line mode. Lists only, never deletes.
fn cli(args: &[String]) {
    let t = std::time::Instant::now();
    let list = apps::load();
    match args[0].as_str() {
        "--list" => {
            for a in &list {
                let loc = a.location.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                println!("{}\t{}\t{}\t{}\t{}", a.name, a.version, a.publisher, a.scope(), loc);
            }
            eprintln!("{} programs, {:?}", list.len(), t.elapsed());
        }
        "--scan" if args.len() > 1 => {
            let deep = args.iter().any(|a| a == "--deep");
            let depth = if deep { leftovers::Depth::Deep } else { leftovers::Depth::Normal };
            let q = args[1..].iter().filter(|a| *a != "--deep").cloned().collect::<Vec<_>>().join(" ").to_lowercase();
            let low = |a: &&apps::App| a.name.to_lowercase();
            let found = list
                .iter()
                .find(|a| low(a) == q)
                .or_else(|| list.iter().find(|a| low(a).starts_with(&q)))
                .or_else(|| list.iter().find(|a| low(a).contains(&q)));
            let Some(app) = found else {
                eprintln!("No program matches: {q}");
                return;
            };
            let others: Vec<_> = list.iter().filter(|a| !std::ptr::eq(*a, app)).cloned().collect();
            println!("{} {}", app.name, app.version);
            println!("names: {:?}  publisher: {:?}", leftovers::name_variants(app), leftovers::publisher_variants(app));
            for l in leftovers::scan(app, &others, depth) {
                let mark = if l.checked { "[x]" } else { "[ ]" };
                println!("{mark} {:?}\t{}\t{}\t{}", l.confidence, l.label(), l.reason, ui::human(l.size));
            }
            eprintln!("{:?}", t.elapsed());
        }
        "--orphans" => {
            for l in leftovers::orphans(&list) {
                println!("{}\t{}", l.label(), ui::human(l.size));
            }
            eprintln!("{:?}", t.elapsed());
        }
        _ => eprintln!("usage: tracewipe [--list | --scan <name> [--deep] | --orphans]"),
    }
}
