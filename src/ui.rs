use crate::apps::{self, App};
use crate::leftovers::{self, Confidence, Kind, Leftover};
use crate::backup;
use crate::procs;
use crate::reg;
use crate::remove::{self, Report};
use crate::theme::{self, Pal, ICONS};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Key, Margin, Rect, RichText,
    Sense, Stroke, StrokeKind, TextFormat, ThemePreference, Ui, Vec2,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

const ROW: f32 = 28.0;
const GROUP: f32 = 34.0;

const IC_SEARCH: &str = "\u{E721}";
const IC_REFRESH: &str = "\u{E72C}";
const IC_FOLDER: &str = "\u{E8B7}";
const IC_FILE: &str = "\u{E8A5}";
const IC_REG: &str = "\u{E74C}";
const IC_TASK: &str = "\u{E823}";
const IC_SHIELD: &str = "\u{EA18}";
const IC_SERVICE: &str = "\u{E912}";
const IC_SORT_UP: &str = "\u{E70E}";
const IC_SORT_DOWN: &str = "\u{E70D}";

enum Msg {
    Apps(Vec<App>),
    Size { generation: u64, index: usize, kb: u64 },
    Uninstalled(Result<(), String>),
    Scanned(Vec<Leftover>),
    Cleaned(Report),
    Warning(String),
    Backups(Vec<backup::Backup>),
    Restored(Vec<String>),
}

enum Stage {
    List,
    Uninstalling,
    Scanning,
    Review { items: Vec<Leftover>, still_installed: bool },
    Cleaning,
    Done(Report),
    Backups { list: Option<Vec<backup::Backup>>, message: Option<String> },
}

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Name,
    Publisher,
    Version,
    Size,
    Date,
}

pub struct UiApp {
    apps: Vec<App>,
    generation: u64,
    loading: bool,
    filtered: Vec<usize>,
    dirty: bool,
    search: String,
    sort: Col,
    asc: bool,
    selected: Option<usize>,
    reveal_selected: bool,
    quiet: bool,
    restore_point: bool,
    target: Option<App>,
    others: Vec<App>,
    forced: bool,
    error: Option<String>,
    /// Result of an action on the finish screen.
    notice: Option<String>,
    /// Processes from the install folder that are running, waiting for a decision.
    confirm_close: Option<(bool, Vec<procs::Proc>)>,
    stage: Stage,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    stop: Arc<AtomicBool>,
    admin: bool,
    theme: ThemePreference,
}

impl UiApp {
    pub fn new(cc: &eframe::CreationContext) -> Self {
        theme::install(&cc.egui_ctx);
        let theme = load_theme();
        cc.egui_ctx.set_theme(theme);
        let (tx, rx) = channel();
        let mut s = UiApp {
            apps: Vec::new(),
            generation: 0,
            loading: false,
            filtered: Vec::new(),
            dirty: true,
            search: String::new(),
            sort: Col::Name,
            asc: true,
            selected: None,
            reveal_selected: false,
            quiet: false,
            restore_point: false,
            target: None,
            others: Vec::new(),
            forced: false,
            error: None,
            notice: None,
            confirm_close: None,
            stage: Stage::List,
            tx,
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            admin: remove::is_admin(),
            theme,
        };
        s.reload(&cc.egui_ctx);
        std::thread::spawn(backup::purge_old);
        s
    }

    fn reload(&mut self, ctx: &egui::Context) {
        self.loading = true;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Apps(apps::load()));
            ctx.request_repaint();
        });
    }

    /// Measures the folder size of programs whose entry has no size, in the background.
    fn measure_sizes(&self, ctx: &egui::Context) {
        let jobs: Vec<(usize, PathBuf)> = self
            .apps
            .iter()
            .enumerate()
            .filter(|(_, a)| a.size_kb == 0)
            .filter_map(|(i, a)| a.location.clone().map(|l| (i, l)))
            .collect();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let generation = self.generation;
        std::thread::spawn(move || {
            use rayon::prelude::*;
            jobs.par_iter().for_each(|(i, p)| {
                let kb = leftovers::dir_size(p) / 1024;
                if kb > 0 {
                    let _ = tx.send(Msg::Size { generation, index: *i, kb });
                    ctx.request_repaint();
                }
            });
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Apps(list) => {
                    let keep = self.selected.and_then(|i| self.apps.get(i)).map(|a| (a.key_name.clone(), a.hive, a.view));
                    self.apps = list;
                    self.generation += 1;
                    self.loading = false;
                    self.dirty = true;
                    self.selected = keep.and_then(|k| self.apps.iter().position(|a| (a.key_name.clone(), a.hive, a.view) == k));
                    self.measure_sizes(ctx);
                }
                Msg::Size { generation, index, kb } => {
                    if generation == self.generation {
                        if let Some(a) = self.apps.get_mut(index) {
                            a.size_kb = kb;
                            if self.sort == Col::Size {
                                self.dirty = true;
                            }
                        }
                    }
                }
                Msg::Uninstalled(r) => {
                    if let Err(e) = r {
                        self.error = Some(e);
                    }
                    self.start_scan(ctx);
                }
                Msg::Scanned(mut items) => {
                    let app = self.target.as_ref().expect("target program");
                    let still = !self.forced && reg::key_exists(app.hive, app.view, &app.key_path());
                    if still {
                        items.iter_mut().for_each(|l| l.checked = false);
                    }
                    self.stage = Stage::Review { items, still_installed: still };
                }
                Msg::Cleaned(r) => self.stage = Stage::Done(r),
                Msg::Warning(w) => self.error = Some(w),
                Msg::Backups(l) => {
                    if let Stage::Backups { list, .. } = &mut self.stage {
                        *list = Some(l);
                    }
                }
                Msg::Restored(failed) => {
                    let message = if failed.is_empty() { "Restored.".to_string() } else { failed.join("\n") };
                    self.open_backups(ctx, Some(message));
                }
            }
        }
    }

    /// Starts an uninstall, first asking to close the program's running processes.
    fn start(&mut self, ctx: &egui::Context, forced: bool) {
        let Some(app) = self.selected.and_then(|i| self.apps.get(i)) else { return };
        let running = app.location.as_deref().filter(|_| !app.location_guessed).map(procs::running_in).unwrap_or_default();
        if running.is_empty() {
            self.start_with(ctx, forced, Vec::new());
        } else {
            self.confirm_close = Some((forced, running));
        }
    }

    fn start_with(&mut self, ctx: &egui::Context, forced: bool, close: Vec<procs::Proc>) {
        let Some(app) = self.selected.and_then(|i| self.apps.get(i)).cloned() else { return };
        self.others = self
            .apps
            .iter()
            .filter(|a| !(a.key_name == app.key_name && a.hive == app.hive && a.view == app.view))
            .cloned()
            .collect();
        self.forced = forced;
        self.error = None;
        let cmd = app.command(self.quiet);
        self.target = Some(app);
        if forced {
            if !close.is_empty() {
                procs::close(&close, std::time::Duration::from_secs(3));
            }
            self.start_scan(ctx);
            return;
        }
        let Some(cmd) = cmd else {
            self.error = Some("This program has no uninstall command. Use Force clean instead.".into());
            self.start_scan(ctx);
            return;
        };
        self.stage = Stage::Uninstalling;
        self.stop.store(false, Ordering::Relaxed);
        let (tx, stop, ctx) = (self.tx.clone(), self.stop.clone(), ctx.clone());
        let restore_point = self.restore_point.then(|| format!("Before uninstalling {}", self.target.as_ref().map(|a| a.name.as_str()).unwrap_or("")));
        std::thread::spawn(move || {
            if !close.is_empty() {
                procs::close(&close, std::time::Duration::from_secs(3));
            }
            let seq = match restore_point.map(|d| remove::begin_restore_point(&d)) {
                Some(Ok(s)) => Some(s),
                Some(Err(e)) => {
                    let _ = tx.send(Msg::Warning(e));
                    None
                }
                None => None,
            };
            let r = remove::run_uninstaller(&cmd, &stop);
            if let Some(s) = seq {
                remove::end_restore_point(s);
            }
            let _ = tx.send(Msg::Uninstalled(r));
            ctx.request_repaint();
        });
    }

    fn start_scan(&mut self, ctx: &egui::Context) {
        self.stage = Stage::Scanning;
        let app = self.target.clone().expect("target program");
        let others = std::mem::take(&mut self.others);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let items = leftovers::scan(&app, &others);
            let _ = tx.send(Msg::Scanned(items));
            ctx.request_repaint();
        });
    }

    fn start_clean(&mut self, ctx: &egui::Context, items: Vec<Leftover>) {
        self.stage = Stage::Cleaning;
        let name = self.target.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = remove::clean(&name, &items);
            let _ = tx.send(Msg::Cleaned(r));
            ctx.request_repaint();
        });
    }

    fn back_to_list(&mut self, ctx: &egui::Context) {
        self.stage = Stage::List;
        self.target = None;
        self.error = None;
        self.reload(ctx);
    }

    fn refilter(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let q = self.search.trim().to_lowercase();
        let apps = &self.apps;
        let mut v: Vec<usize> = (0..apps.len())
            .filter(|&i| q.is_empty() || apps[i].name.to_lowercase().contains(&q) || apps[i].publisher.to_lowercase().contains(&q))
            .collect();
        let key = |a: &App| -> (String, u64) {
            match self.sort {
                Col::Name => (a.name.to_lowercase(), 0),
                Col::Publisher => (a.publisher.to_lowercase(), 0),
                Col::Version => (a.version.clone(), 0),
                Col::Size => (String::new(), a.size_kb),
                Col::Date => {
                    let d = &a.install_date;
                    let iso = if d.len() == 10 { format!("{}{}{}", &d[6..], &d[3..5], &d[..2]) } else { d.clone() };
                    (iso, 0)
                }
            }
        };
        v.sort_by_cached_key(|&i| key(&apps[i]));
        if !self.asc {
            v.reverse();
        }
        self.filtered = v;
    }
}

impl eframe::App for UiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        self.refilter();
        let p = theme::pal(ctx);

        egui::TopBottomPanel::top("bar")
            .exact_height(44.0)
            .frame(Frame::new().fill(p.nav).inner_margin(Margin::symmetric(12, 8)))
            .show(ctx, |ui| self.top_bar(ui, p));

        egui::TopBottomPanel::bottom("status")
            .exact_height(23.0)
            .frame(Frame::new().fill(p.status).inner_margin(Margin::symmetric(12, 0)))
            .show(ctx, |ui| self.status_bar(ui, p));

        if matches!(self.stage, Stage::List) {
            egui::SidePanel::right("details")
                .resizable(false)
                .exact_width(340.0)
                .frame(Frame::new().fill(p.panel).inner_margin(Margin::same(16)))
                .show(ctx, |ui| self.details(ui, p));
        }

        self.close_dialog(ctx, p);

        egui::CentralPanel::default().frame(Frame::new().fill(p.content)).show(ctx, |ui| match self.stage {
            Stage::List => self.app_list(ui, p),
            Stage::Uninstalling => self.waiting(ui, p),
            Stage::Scanning => busy(ui, p, "Scanning for traces", "Searching files, the registry, shortcuts and scheduled tasks."),
            Stage::Review { .. } => self.review(ui, p),
            Stage::Cleaning => busy(ui, p, "Cleaning", "Moving files into the backup folder and exporting registry entries."),
            Stage::Done(_) => self.done(ui, p),
            Stage::Backups { .. } => self.backups(ui, p),
        });
    }
}

// ---------------------------------------------------------------- top and status bars

impl UiApp {
    fn top_bar(&mut self, ui: &mut Ui, p: &Pal) {
        ui.horizontal_centered(|ui| {
            let enabled = matches!(self.stage, Stage::List);
            ui.add_enabled_ui(enabled, |ui| {
                let id = ui.make_persistent_id("search");
                if ui.input(|i| i.modifiers.command && i.key_pressed(Key::F)) {
                    ui.memory_mut(|m| m.request_focus(id));
                }
                let r = search_box(ui, p, &mut self.search, id, 320.0);
                if r.changed() {
                    self.dirty = true;
                }
                if r.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.search.clear();
                    self.dirty = true;
                }
            });

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(enabled, |ui| {
                    if ui.add(egui::Button::new("Backups").min_size(Vec2::new(0.0, 28.0))).on_hover_text("Restore what earlier cleanups removed").clicked() {
                        self.open_backups(ui.ctx(), None);
                    }
                });
                ui.add_enabled_ui(enabled && !self.loading, |ui| {
                    if ui.add(egui::Button::new(icon(IC_REFRESH, 14.0)).min_size(Vec2::splat(28.0))).on_hover_text("Refresh (F5)").clicked()
                        || ui.input(|i| i.key_pressed(Key::F5))
                    {
                        self.reload(ui.ctx());
                    }
                });
                let label = match self.theme {
                    ThemePreference::System => "System theme",
                    ThemePreference::Dark => "Dark",
                    ThemePreference::Light => "Light",
                };
                let before = self.theme;
                egui::ComboBox::from_id_salt("theme").selected_text(label).width(130.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.theme, ThemePreference::System, "System theme");
                    ui.selectable_value(&mut self.theme, ThemePreference::Dark, "Dark");
                    ui.selectable_value(&mut self.theme, ThemePreference::Light, "Light");
                });
                if before != self.theme {
                    ui.ctx().set_theme(self.theme);
                    save_theme(self.theme);
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut Ui, p: &Pal) {
        ui.horizontal_centered(|ui| {
            let text = if self.loading && self.apps.is_empty() {
                "Reading programs".to_string()
            } else {
                let total: u64 = self.filtered.iter().map(|&i| self.apps[i].size_kb).sum();
                if self.filtered.len() == self.apps.len() {
                    format!("{} programs, {}", self.apps.len(), human(total * 1024))
                } else {
                    format!("{} of {} programs, {}", self.filtered.len(), self.apps.len(), human(total * 1024))
                }
            };
            ui.label(RichText::new(text).size(12.0).color(p.weak));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if self.admin {
                    ui.label(RichText::new("Administrator").size(12.0).color(p.weak));
                } else {
                    let b = egui::Button::new(RichText::new(format!("{IC_SHIELD}  Run as administrator")).size(12.0).family(FontFamily::Name(ICONS.into())))
                        .frame(false);
                    if ui.add(b).on_hover_text("Needed to remove traces under Program Files and HKLM").clicked() && remove::relaunch_as_admin() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            });
        });
    }
}

// ---------------------------------------------------------------- program list

impl UiApp {
    fn columns(width: f32) -> [(Col, &'static str, f32); 5] {
        let fixed = 190.0 + 110.0 + 84.0 + 92.0;
        [
            (Col::Name, "Name", (width - fixed - 12.0).max(160.0)),
            (Col::Publisher, "Publisher", 190.0),
            (Col::Version, "Version", 110.0),
            (Col::Size, "Size", 84.0),
            (Col::Date, "Installed", 92.0),
        ]
    }

    fn app_list(&mut self, ui: &mut Ui, p: &Pal) {
        let width = ui.available_width();
        let cols = Self::columns(width);

        let (hr, _) = ui.allocate_exact_size(Vec2::new(width, GROUP), Sense::hover());
        let mut x = hr.left() + 12.0;
        for (col, title, w) in cols {
            let r = Rect::from_min_size(egui::pos2(x, hr.top()), Vec2::new(w, GROUP));
            let resp = ui.interact(r, ui.id().with(title), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(r, 0.0, p.hover);
            }
            let right = col == Col::Size;
            let pos = if right { egui::pos2(r.right() - 8.0, r.center().y) } else { egui::pos2(r.left() + 4.0, r.center().y) };
            let align = if right { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
            let tr = ui.painter().text(pos, align, title, FontId::proportional(12.0), p.weak);
            if self.sort == col {
                let ic = if self.asc { IC_SORT_UP } else { IC_SORT_DOWN };
                let ip = if right { egui::pos2(tr.left() - 6.0, r.center().y) } else { egui::pos2(tr.right() + 6.0, r.center().y) };
                let ia = if right { Align2::RIGHT_CENTER } else { Align2::LEFT_CENTER };
                ui.painter().text(ip, ia, ic, FontId::new(10.0, FontFamily::Name(ICONS.into())), p.weak);
            }
            if resp.clicked() {
                if self.sort == col {
                    self.asc = !self.asc;
                } else {
                    self.sort = col;
                    self.asc = col != Col::Size;
                }
                self.dirty = true;
            }
            x += w;
        }
        ui.painter().hline(hr.x_range(), hr.bottom() - 0.5, Stroke::new(1.0, p.divider));

        if self.filtered.is_empty() {
            let msg = if self.loading { "Reading programs" } else { "No matching programs" };
            ui.add_space(24.0);
            ui.vertical_centered(|ui| ui.label(RichText::new(msg).color(p.weak)));
            return;
        }

        self.keyboard_nav(ui);
        let sel_pos = self.selected.and_then(|s| self.filtered.iter().position(|&i| i == s));
        let mut clicked = None;
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
        if self.reveal_selected {
            if let Some(pos) = sel_pos {
                let view_h = ui.available_height();
                let top = pos as f32 * (ROW + ui.spacing().item_spacing.y);
                let current = ui.data(|d| d.get_temp::<f32>(ui.id().with("scroll_y"))).unwrap_or(0.0);
                if top < current {
                    scroll = scroll.vertical_scroll_offset(top);
                } else if top + ROW > current + view_h {
                    scroll = scroll.vertical_scroll_offset(top + ROW - view_h + 4.0);
                }
            }
            self.reveal_selected = false;
        }
        let out = scroll.show_rows(ui, ROW, self.filtered.len(), |ui, range| {
            for pos in range {
                let i = self.filtered[pos];
                let a = &self.apps[i];
                let (r, resp) = ui.allocate_exact_size(Vec2::new(width, ROW), Sense::click());
                let sel = self.selected == Some(i);
                paint_row(ui, p, r, sel, resp.hovered());
                let mut x = r.left() + 12.0;
                let size = if a.size_kb > 0 { human(a.size_kb * 1024) } else { String::new() };
                for (col, _, w) in cols {
                    let (t, c): (&str, Color32) = match col {
                        Col::Name => (&a.name, p.text),
                        Col::Publisher => (&a.publisher, p.weak),
                        Col::Version => (&a.version, p.weak),
                        Col::Size => (&size, p.weak),
                        Col::Date => (&a.install_date, p.weak),
                    };
                    let g = elided(ui, t, FontId::proportional(14.0), c, w - 12.0);
                    let gx = if col == Col::Size { x + w - 8.0 - g.size().x } else { x + 4.0 };
                    ui.painter().galley(egui::pos2(gx, r.center().y - g.size().y / 2.0), g, c);
                    x += w;
                }
                if resp.clicked() {
                    clicked = Some(i);
                }
                if resp.double_clicked() {
                    clicked = Some(i);
                }
            }
        });
        ui.data_mut(|d| d.insert_temp(ui.id().with("scroll_y"), out.state.offset.y));
        if let Some(i) = clicked {
            self.selected = Some(i);
        }
    }

    fn keyboard_nav(&mut self, ui: &Ui) {
        if ui.ctx().wants_keyboard_input() && !ui.ctx().memory(|m| m.focused() == Some(ui.make_persistent_id("search"))) {
            return;
        }
        let (down, up, home, end) = ui.input(|i| {
            (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Home), i.key_pressed(Key::End))
        });
        if !(down || up || home || end) {
            return;
        }
        let n = self.filtered.len();
        let cur = self.selected.and_then(|s| self.filtered.iter().position(|&i| i == s));
        let next = match cur {
            _ if home => 0,
            _ if end => n - 1,
            None => 0,
            Some(c) if down => (c + 1).min(n - 1),
            Some(c) => c.saturating_sub(1),
        };
        self.selected = Some(self.filtered[next]);
        self.reveal_selected = true;
    }

    fn details(&mut self, ui: &mut Ui, p: &Pal) {
        let Some(a) = self.selected.and_then(|i| self.apps.get(i)).cloned() else {
            ui.add_space(8.0);
            ui.label(RichText::new("Select a program to uninstall.").color(p.weak));
            return;
        };
        ui.label(RichText::new(&a.name).size(20.0).color(p.text));
        ui.add_space(12.0);

        let field = |ui: &mut Ui, k: &str, v: &str| {
            if v.is_empty() {
                return;
            }
            ui.label(RichText::new(k).size(12.0).color(p.weak));
            ui.add(egui::Label::new(RichText::new(v).color(p.label)).wrap());
            ui.add_space(6.0);
        };
        field(ui, "Publisher", &a.publisher);
        field(ui, "Version", &a.version);
        field(ui, "Installed", &a.install_date);
        if a.size_kb > 0 {
            field(ui, "Size", &human(a.size_kb * 1024));
        }
        if let Some(loc) = &a.location {
            ui.label(RichText::new(if a.location_guessed { "Location, guessed" } else { "Location" }).size(12.0).color(p.weak));
            let link = ui.add(egui::Link::new(RichText::new(loc.display().to_string()).color(p.accent_text))).on_hover_text("Open in Explorer");
            if link.clicked() {
                remove::reveal(loc);
            }
            ui.add_space(6.0);
        }
        field(ui, "Registry", &reg::display_path(a.hive, a.view, &a.key_path()));
        field(ui, "Scope", a.scope());

        ui.add_space(10.0);
        let has_quiet = a.quiet_uninstall.is_some() || (a.msi && apps::is_guid(&a.key_name));
        if has_quiet {
            checkbox(ui, p, &mut self.quiet, "Quiet uninstall");
        }
        ui.add_enabled_ui(self.admin, |ui| {
            checkbox(ui, p, &mut self.restore_point, "Create a restore point first")
                .on_disabled_hover_text("Needs administrator rights");
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if accent_button(ui, p, "Uninstall").clicked() {
                self.start(ui.ctx(), false);
            }
            if ui
                .add(egui::Button::new("Force clean").min_size(Vec2::new(0.0, 28.0)))
                .on_hover_text("Scans for traces without running the uninstaller. For programs whose uninstaller is broken or gone.")
                .clicked()
            {
                self.start(ui.ctx(), true);
            }
        });
        ui.add_space(10.0);
        ui.add(
            egui::Label::new(
                RichText::new("When the uninstaller finishes, Tracewipe scans for what is left. You review the list before anything is removed.")
                    .size(12.0)
                    .color(p.weak),
            )
            .wrap(),
        );
    }
}

// ---------------------------------------------------------------- backups

impl UiApp {
    fn open_backups(&mut self, ctx: &egui::Context, message: Option<String>) {
        self.stage = Stage::Backups { list: None, message };
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Backups(backup::list()));
            ctx.request_repaint();
        });
    }

    fn backups(&mut self, ui: &mut Ui, p: &Pal) {
        let ctx = ui.ctx().clone();
        let Stage::Backups { list, message } = &mut self.stage else { return };
        let mut back = false;
        let mut restore = None;
        let mut discard = None;
        Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| {
            ui.label(RichText::new("Backups").size(20.0).color(p.text));
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Each cleanup is kept for {} days, then deleted. Restoring puts files, registry entries and tasks back where they were.", backup::KEEP_DAYS))
                    .color(p.weak),
            );
            if let Some(m) = message {
                ui.add_space(6.0);
                ui.label(RichText::new(m.as_str()).color(p.label));
            }
            ui.add_space(8.0);
            if ui.add(egui::Button::new("Back to list").min_size(Vec2::new(0.0, 28.0))).clicked() {
                back = true;
            }
        });
        ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, p.divider));
        match list {
            None => busy(ui, p, "Reading backups", ""),
            Some(l) if l.is_empty() => {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| ui.label(RichText::new("No backups yet.").color(p.weak)));
            }
            Some(l) => {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let width = ui.available_width();
                    for (i, b) in l.iter().enumerate() {
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(width, ROW + 16.0), Sense::hover());
                        paint_row(ui, p, r, false, resp.hovered());
                        let age = b.when.elapsed().map(|d| d.as_secs()).unwrap_or(0);
                        let when = if age < 3600 { format!("{} min ago", age / 60) } else if age < 86400 { format!("{} h ago", age / 3600) } else { format!("{} days ago", age / 86400) };
                        ui.painter().text(egui::pos2(r.left() + 16.0, r.top() + 12.0), Align2::LEFT_CENTER, &b.name, FontId::proportional(14.0), p.text);
                        ui.painter().text(
                            egui::pos2(r.left() + 16.0, r.top() + 31.0),
                            Align2::LEFT_CENTER,
                            format!("{when}, {} items, {}", b.items, human(b.size)),
                            FontId::proportional(12.0),
                            p.weak,
                        );
                        let rr = Rect::from_min_size(egui::pos2(r.right() - 230.0, r.center().y - 14.0), Vec2::new(90.0, 28.0));
                        if ui.put(rr, egui::Button::new("Restore")).clicked() {
                            restore = Some(i);
                        }
                        let dr = Rect::from_min_size(egui::pos2(r.right() - 132.0, r.center().y - 14.0), Vec2::new(116.0, 28.0));
                        if ui.put(dr, egui::Button::new("Delete for good")).on_hover_text("Frees the space now. This cannot be undone.").clicked() {
                            discard = Some(i);
                        }
                    }
                });
            }
        }
        let dir_of = |i: usize| match &self.stage {
            Stage::Backups { list: Some(l), .. } => l.get(i).map(|b| b.dir.clone()),
            _ => None,
        };
        if back {
            self.back_to_list(&ctx);
        } else if let Some(dir) = restore.and_then(dir_of) {
            self.stage = Stage::Backups { list: None, message: Some("Restoring".into()) };
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(Msg::Restored(backup::restore(&dir)));
                ctx.request_repaint();
            });
        } else if let Some(dir) = discard.and_then(dir_of) {
            let message = backup::discard(&dir).err().map(|e| e.to_string());
            self.open_backups(&ctx, message);
        }
    }
}

// ---------------------------------------------------------------- uninstall steps

impl UiApp {
    fn close_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some((forced, running)) = self.confirm_close.clone() else { return };
        let mut choice = None;
        let modal = egui::Modal::new(egui::Id::new("close-running")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(RichText::new("The program is running").size(20.0).color(p.text));
            ui.add_space(6.0);
            ui.label(RichText::new("These processes run from its folder. Uninstalling while they run can leave files behind.").color(p.weak));
            ui.add_space(6.0);
            for r in &running {
                ui.label(RichText::new(format!("{}  {}", r.name, r.pid)).color(p.label));
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if accent_button(ui, p, "Close them and continue").clicked() {
                    choice = Some(Some(running.clone()));
                }
                if ui.add(egui::Button::new("Continue anyway").min_size(Vec2::new(0.0, 28.0))).clicked() {
                    choice = Some(Some(Vec::new()));
                }
                if ui.add(egui::Button::new("Cancel").min_size(Vec2::new(0.0, 28.0))).clicked() {
                    choice = Some(None);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(None);
        }
        if let Some(c) = choice {
            self.confirm_close = None;
            if let Some(close) = c {
                self.start_with(ctx, forced, close);
            }
        }
    }

    fn waiting(&mut self, ui: &mut Ui, p: &Pal) {
        let name = self.target.as_ref().map(|a| a.name.as_str()).unwrap_or("");
        centered(ui, |ui| {
            ui.add(egui::Spinner::new().size(24.0).color(p.accent_text));
            ui.add_space(12.0);
            ui.label(RichText::new(format!("Uninstalling {name}")).size(20.0).color(p.text));
            ui.add_space(6.0);
            ui.label(RichText::new("Finish the steps in the uninstaller window. The scan starts when the uninstaller and every process it started have exited.").color(p.weak));
            ui.add_space(16.0);
            if ui.button("Stop waiting and scan").clicked() {
                self.stop.store(true, Ordering::Relaxed);
            }
        });
    }

    fn review(&mut self, ui: &mut Ui, p: &Pal) {
        let name = self.target.as_ref().map(|a| a.name.clone()).unwrap_or_default();
        let ctx = ui.ctx().clone();
        let Stage::Review { items, still_installed } = &mut self.stage else { return };

        let mut go_back = false;
        let mut clean = false;
        Frame::new().inner_margin(Margin { left: 16, right: 16, top: 14, bottom: 8 }).show(ui, |ui| {
            ui.label(RichText::new(format!("{name} izleri")).size(20.0).color(p.text));
            ui.add_space(4.0);
            let sel: Vec<&Leftover> = items.iter().filter(|l| l.checked).collect();
            let bytes: u64 = sel.iter().map(|l| l.size).sum();
            let summary = if items.is_empty() {
                "Nothing was left behind.".to_string()
            } else {
                format!("{} traces found, {} selected, {}", items.len(), sel.len(), human(bytes))
            };
            ui.label(RichText::new(summary).color(p.weak));
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(p.error_text));
            }
            if *still_installed {
                ui.add(
                    egui::Label::new(
                        RichText::new("The program still looks installed. The uninstaller may have been cancelled or may be waiting for a restart. Nothing is checked.")
                            .color(p.error_text),
                    )
                    .wrap(),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let n = items.iter().filter(|l| l.checked).count();
                ui.add_enabled_ui(n > 0, |ui| {
                    if danger_button(ui, p, &format!("Remove selected ({n})")).clicked() {
                        clean = true;
                    }
                });
                if ui.add(egui::Button::new(if items.is_empty() { "Back to list" } else { "Cancel" }).min_size(Vec2::new(0.0, 28.0))).clicked() {
                    go_back = true;
                }
            });
        });
        ui.painter().hline(ui.max_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, p.divider));

        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let width = ui.available_width();
            for (g, title) in [(0u8, "Files and folders"), (1, "Registry"), (2, "Scheduled tasks"), (3, "Services and firewall rules")] {
                let idx: Vec<usize> = (0..items.len()).filter(|&i| items[i].group() == g).collect();
                if idx.is_empty() {
                    continue;
                }
                let all = idx.iter().all(|&i| items[i].checked);
                let (r, _) = ui.allocate_exact_size(Vec2::new(width, GROUP), Sense::hover());
                ui.painter().text(egui::pos2(r.left() + 16.0, r.center().y), Align2::LEFT_CENTER, format!("{title}  {}", idx.len()), FontId::proportional(14.0), p.label);
                let br = Rect::from_min_size(egui::pos2(r.right() - 112.0, r.center().y - 12.0), Vec2::new(96.0, 24.0));
                let b = ui.put(br, egui::Button::new(RichText::new(if all { "Select none" } else { "Select all" }).size(12.0)).frame(false));
                if b.clicked() {
                    idx.iter().for_each(|&i| items[i].checked = !all);
                }
                for &i in &idx {
                    leftover_row(ui, p, width, &mut items[i]);
                }
            }
        });

        if clean {
            let chosen: Vec<Leftover> = items.iter().filter(|l| l.checked).cloned().collect();
            self.start_clean(&ctx, chosen);
        } else if go_back {
            self.back_to_list(&ctx);
        }
    }

    fn done(&mut self, ui: &mut Ui, p: &Pal) {
        let Stage::Done(r) = &self.stage else { return };
        let mut back = false;
        let mut retry = false;
        let mut at_restart = false;
        let backup = r.backup.clone();
        let stuck: Vec<Leftover> = r.failed.iter().filter_map(|f| f.item.clone()).collect();
        let lockers: Vec<procs::Proc> = r.failed.iter().flat_map(|f| f.lockers.iter().cloned()).collect();
        Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| {
            ui.label(RichText::new("Cleanup finished").size(20.0).color(p.text));
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{} traces removed. You can restore them from Backups for {} days.", r.removed, crate::backup::KEEP_DAYS)).color(p.weak));
            if backup.exists() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Registry and task backup:").color(p.weak));
                    if ui.add(egui::Link::new(RichText::new(backup.display().to_string()).color(p.accent_text))).clicked() {
                        remove::reveal(&backup);
                    }
                });
            }
            if !r.failed.is_empty() {
                ui.add_space(10.0);
                ui.label(RichText::new(format!("{} traces could not be removed", r.failed.len())).color(p.error_text));
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for f in &r.failed {
                        ui.label(RichText::new(&f.label).color(p.label));
                        ui.label(RichText::new(&f.reason).size(12.0).color(p.error_text));
                        ui.add_space(4.0);
                    }
                });
            }
            if let Some(msg) = &self.notice {
                ui.add_space(8.0);
                ui.label(RichText::new(msg).color(p.weak));
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if accent_button(ui, p, "Back to list").clicked() {
                    back = true;
                }
                if !lockers.is_empty() {
                    let names: Vec<&str> = lockers.iter().map(|l| l.name.as_str()).collect();
                    if ui
                        .add(egui::Button::new(format!("Close {} and retry", names.join(", "))).min_size(Vec2::new(0.0, 28.0)))
                        .on_hover_text("Ends those programs. Unsaved work in them is lost.")
                        .clicked()
                    {
                        retry = true;
                    }
                }
                if !stuck.is_empty()
                    && ui
                        .add(egui::Button::new("Remove at next restart").min_size(Vec2::new(0.0, 28.0)))
                        .on_hover_text("Windows deletes these files before anything can lock them again. Needs administrator rights.")
                        .clicked()
                {
                    at_restart = true;
                }
            });
        });
        let ctx = ui.ctx().clone();
        if back {
            self.notice = None;
            self.back_to_list(&ctx);
        } else if retry {
            procs::terminate(&lockers);
            std::thread::sleep(std::time::Duration::from_millis(300));
            self.notice = None;
            self.start_clean(&ctx, stuck);
        } else if at_restart {
            let failed = remove::remove_at_restart(&stuck);
            self.notice = Some(if failed.is_empty() {
                format!("{} items will be removed when Windows restarts.", stuck.len())
            } else {
                failed[0].reason.clone()
            });
        }
    }
}

fn leftover_row(ui: &mut Ui, p: &Pal, width: f32, l: &mut Leftover) {
    let (r, resp) = ui.allocate_exact_size(Vec2::new(width, ROW + 8.0), Sense::click());
    paint_row(ui, p, r, false, resp.hovered());
    if resp.clicked() {
        l.checked = !l.checked;
    }
    if resp.double_clicked() {
        if let Kind::Path { path, .. } = &l.kind {
            remove::reveal(path.parent().unwrap_or(path));
        }
    }
    let box_r = Rect::from_center_size(egui::pos2(r.left() + 26.0, r.center().y), Vec2::splat(18.0));
    paint_check(ui, p, box_r, l.checked);

    let (ic, ic_color) = match &l.kind {
        Kind::Path { dir: true, .. } => (IC_FOLDER, Color32::from_rgb(0xE8, 0xB8, 0x4A)),
        Kind::Path { .. } => (IC_FILE, p.weak),
        Kind::Key { .. } | Kind::Value { .. } => (IC_REG, p.weak),
        Kind::Task { .. } => (IC_TASK, p.weak),
        Kind::Service { .. } => (IC_SERVICE, p.weak),
        Kind::Firewall { .. } => (IC_SHIELD, p.weak),
        Kind::PathEntry { .. } => (IC_REG, p.weak),
    };
    ui.painter().text(egui::pos2(r.left() + 48.0, r.center().y), Align2::LEFT_CENTER, ic, FontId::new(16.0, FontFamily::Name(ICONS.into())), ic_color);

    let size = if l.size > 0 { human(l.size) } else { String::new() };
    let text_x = r.left() + 76.0;
    let size_w = 90.0;
    let tw = (r.right() - text_x - size_w - 16.0).max(80.0);
    let g = elided(ui, &l.label(), FontId::proportional(14.0), p.text, tw);
    ui.painter().galley(egui::pos2(text_x, r.top() + 3.0), g, p.text);
    let reason_color = if l.confidence == Confidence::Medium { p.error_text } else { p.weak };
    let g2 = elided(ui, l.reason, FontId::proportional(12.0), reason_color, tw);
    ui.painter().galley(egui::pos2(text_x, r.top() + 21.0), g2, reason_color);
    ui.painter().text(egui::pos2(r.right() - 16.0, r.center().y), Align2::RIGHT_CENTER, size, FontId::proportional(12.0), p.weak);
}

// ---------------------------------------------------------------- widgets

fn icon(s: &str, size: f32) -> RichText {
    RichText::new(s).family(FontFamily::Name(ICONS.into())).size(size)
}

fn paint_row(ui: &Ui, p: &Pal, r: Rect, selected: bool, hovered: bool) {
    let fill = match (selected, hovered) {
        (true, true) => Some(p.sel_hover),
        (true, false) => Some(p.sel),
        (false, true) => Some(p.hover),
        _ => None,
    };
    if let Some(f) = fill {
        ui.painter().rect_filled(r.shrink2(Vec2::new(4.0, 1.0)), 4.0, f);
    }
    if selected {
        let s = Rect::from_min_size(egui::pos2(r.left() + 4.0, r.center().y - 8.0), Vec2::new(3.0, 16.0));
        ui.painter().rect_filled(s, 1.5, p.accent_text);
    }
}

fn paint_check(ui: &Ui, p: &Pal, r: Rect, on: bool) {
    let painter = ui.painter();
    if on {
        painter.rect_filled(r, 4.0, p.accent_text);
        let pts = [egui::pos2(r.left() + 4.0, r.center().y), egui::pos2(r.left() + 7.5, r.bottom() - 5.0), egui::pos2(r.right() - 4.0, r.top() + 5.0)];
        painter.line(pts.to_vec(), Stroke::new(1.6, p.on_accent));
    } else {
        painter.rect_stroke(r, 4.0, Stroke::new(1.0, p.control_stroke), StrokeKind::Inside);
    }
}

fn checkbox(ui: &mut Ui, p: &Pal, v: &mut bool, text: &str) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(14.0), p.text);
    let (r, mut resp) = ui.allocate_exact_size(Vec2::new(26.0 + g.size().x, 26.0), Sense::click());
    if resp.clicked() {
        *v = !*v;
        resp.mark_changed();
    }
    let br = Rect::from_center_size(egui::pos2(r.left() + 9.0, r.center().y), Vec2::splat(18.0));
    paint_check(ui, p, br, *v);
    ui.painter().galley(egui::pos2(r.left() + 26.0, r.center().y - g.size().y / 2.0), g, p.text);
    if resp.has_focus() {
        ui.painter().rect_stroke(br.expand(3.0), 5.0, Stroke::new(2.0, p.text), StrokeKind::Outside);
    }
    resp
}

fn accent_button(ui: &mut Ui, p: &Pal, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(Color32::from_rgb(0xFF, 0xFF, 0xFF))).fill(p.accent).stroke(Stroke::NONE).min_size(Vec2::new(88.0, 28.0)))
}

fn danger_button(ui: &mut Ui, p: &Pal, text: &str) -> egui::Response {
    let id = ui.next_auto_id();
    let pressed = ui.ctx().read_response(id).is_some_and(|r| r.is_pointer_button_down_on());
    let fill = if pressed { p.danger_pressed } else { p.danger };
    ui.add(egui::Button::new(RichText::new(text).color(Color32::from_rgb(0xFF, 0xFF, 0xFF))).fill(fill).stroke(Stroke::NONE).min_size(Vec2::new(88.0, 28.0)))
}

fn search_box(ui: &mut Ui, p: &Pal, text: &mut String, id: egui::Id, width: f32) -> egui::Response {
    let out = Frame::new().fill(p.field).corner_radius(CornerRadius::same(4)).inner_margin(Margin::symmetric(8, 3)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(icon(IC_SEARCH, 13.0).color(p.weak));
            ui.add(
                egui::TextEdit::singleline(text)
                    .id(id)
                    .frame(false)
                    .hint_text(RichText::new("Search programs or publishers").color(p.weak))
                    .desired_width(width - 40.0)
                    .text_color(p.text),
            )
        })
        .inner
    });
    let r = out.response.rect;
    let focused = out.inner.has_focus();
    if !ui.ctx().theme().default_visuals().dark_mode {
        ui.painter().rect_stroke(r, 4.0, Stroke::new(1.0, p.line), StrokeKind::Inside);
    }
    let (w, c) = if focused { (2.0, p.accent_text) } else { (1.0, p.control_stroke) };
    ui.painter().hline(r.x_range().shrink(2.0), r.bottom() - w / 2.0, Stroke::new(w, c));
    out.inner
}

fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), TextFormat::simple(font, color));
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts(|f| f.layout_job(job))
}

fn centered(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space((ui.available_height() * 0.3).max(24.0));
    ui.vertical_centered(|ui| {
        ui.set_max_width(520.0);
        add(ui)
    });
}

fn busy(ui: &mut Ui, p: &Pal, title: &str, sub: &str) {
    centered(ui, |ui| {
        ui.add(egui::Spinner::new().size(24.0).color(p.accent_text));
        ui.add_space(12.0);
        ui.label(RichText::new(title).size(20.0).color(p.text));
        ui.add_space(6.0);
        ui.label(RichText::new(sub).color(p.weak));
    });
}

pub fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let s = if i == 0 || v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    format!("{} {}", s.replace('.', ","), U[i])
}

// ---------------------------------------------------------------- theme preference

fn theme_file() -> PathBuf {
    crate::backup::root().parent().map(|p| p.join("theme.txt")).unwrap_or_else(|| "theme.txt".into())
}

fn load_theme() -> ThemePreference {
    match std::fs::read_to_string(theme_file()).unwrap_or_default().trim() {
        "dark" => ThemePreference::Dark,
        "light" => ThemePreference::Light,
        _ => ThemePreference::System,
    }
}

fn save_theme(t: ThemePreference) {
    let f = theme_file();
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let v = match t {
        ThemePreference::Dark => "dark",
        ThemePreference::Light => "light",
        ThemePreference::System => "system",
    };
    let _ = std::fs::write(f, v);
}
