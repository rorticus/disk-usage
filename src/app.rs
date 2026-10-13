//! Application state and UI: disk picker, scan progress, chart report and collector.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, DragAndDrop, FontId, Id, Key, LayerId, Layout,
    Order, RichText, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use crate::model::{self, Kind, NodeId, Progress, Tree};
use crate::sunburst::{self, Geometry, Hit, Target};
use crate::util::{self, fmt_count, fmt_duration, fmt_size};
use crate::volumes::{self, Volume};

const ROW_H: f32 = 26.0;
const DELETE_COUNTDOWN: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct DragItem(NodeId);

pub struct App {
    volumes: Vec<Volume>,
    volumes_at: Instant,
    screen: Screen,
    /// Finished scans kept around so they can be viewed again without rescanning.
    saved: HashMap<PathBuf, Report>,
    status: Option<String>,
    /// Folder passed on the command line, scanned on the first frame.
    pending: Option<PathBuf>,
}

enum Screen {
    Start,
    Scanning(ScanJob),
    Report(Box<Report>),
}

struct ScanJob {
    root: PathBuf,
    label: String,
    progress: Arc<Progress>,
    rx: Receiver<Option<Tree>>,
    started: Instant,
    /// Bytes in use on the volume, used to estimate progress for whole-disk scans.
    expected: Option<u64>,
}

enum Deletion {
    Countdown(Instant),
    Running(Receiver<DeleteOutcome>),
}

struct DeleteOutcome {
    removed: Vec<NodeId>,
    failed: Vec<(NodeId, String)>,
}

struct Report {
    tree: Tree,
    focus: NodeId,
    back: Vec<NodeId>,
    forward: Vec<NodeId>,
    hovered: Option<Target>,
    menu_target: Option<Target>,
    expand_smaller: bool,
    collector: Vec<NodeId>,
    deletion: Option<Deletion>,
    permanent: bool,
    freed: u64,
    banner: Option<String>,
    errors: Vec<String>,
}

/// Requests from the report UI that need the whole app.
enum Exit {
    Home,
    Rescan(PathBuf),
}

enum Act {
    Navigate(NodeId),
    Back,
    Forward,
    Up,
    Collect(NodeId),
    Uncollect(NodeId),
    ToggleSmaller,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, pending: Option<PathBuf>) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        Self {
            volumes: volumes::list(),
            volumes_at: Instant::now(),
            screen: Screen::Start,
            saved: HashMap::new(),
            status: None,
            pending,
        }
    }

    fn start_scan(&mut self, ctx: &egui::Context, root: PathBuf) {
        if let Screen::Report(r) = std::mem::replace(&mut self.screen, Screen::Start) {
            if r.deletion.is_some() {
                self.screen = Screen::Report(r);
                return;
            }
        }
        self.saved.remove(&root);
        let progress = Arc::new(Progress::default());
        let (tx, rx) = mpsc::channel();
        let (p, r, c) = (progress.clone(), root.clone(), ctx.clone());
        thread::spawn(move || {
            let _ = tx.send(model::scan(&r, &p));
            c.request_repaint();
        });
        let expected = volumes::space_if_mount_point(&root).map(|v| v.total - v.free);
        let label = if expected.is_some() {
            volumes::volume_name(&root)
        } else {
            root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into())
        };
        self.status = None;
        self.screen = Screen::Scanning(ScanJob {
            root,
            label,
            progress,
            rx,
            started: Instant::now(),
            expected,
        });
    }

    fn go_home(&mut self) {
        if let Screen::Report(r) = std::mem::replace(&mut self.screen, Screen::Start) {
            self.saved.insert(r.tree.root_path.clone(), *r);
        }
        self.volumes = volumes::list();
        self.volumes_at = Instant::now();
    }

    fn poll_scan(&mut self) {
        let Screen::Scanning(job) = &self.screen else { return };
        let Ok(result) = job.rx.try_recv() else { return };
        match result {
            Some(tree) => {
                let msg = format!("{} scanned in {}", job.label, fmt_duration(tree.duration));
                util::notify("Scan completed", &msg);
                self.screen = Screen::Report(Box::new(Report::new(tree)));
            }
            None => {
                self.status = Some(format!("Scan of {} was cancelled", job.label));
                self.screen = Screen::Start;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_scan();
        if let Some(p) = self.pending.take() {
            self.start_scan(&ctx, p);
        }

        // Dropping a folder or disk anywhere on the window scans it.
        let dropped: Option<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).find(|p| p.is_dir())
        });
        if let Some(p) = dropped {
            if !matches!(self.screen, Screen::Scanning(_)) {
                self.start_scan(&ctx, p);
            }
        }

        match &mut self.screen {
            Screen::Start => {
                if self.volumes_at.elapsed() > Duration::from_secs(5) {
                    self.volumes = volumes::list();
                    self.volumes_at = Instant::now();
                }
                if let Some(path) = self.start_ui(ui) {
                    match path {
                        StartAct::Scan(p) => self.start_scan(&ctx, p),
                        StartAct::View(p) => {
                            if let Some(r) = self.saved.remove(&p) {
                                self.screen = Screen::Report(Box::new(r));
                            }
                        }
                        StartAct::Forget(p) => {
                            self.saved.remove(&p);
                        }
                    }
                }
                ctx.request_repaint_after(Duration::from_secs(5));
            }
            Screen::Scanning(job) => {
                if scanning_ui(ui, job) {
                    job.progress.cancel.store(true, Relaxed);
                }
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Screen::Report(report) => match report.ui(ui) {
                Some(Exit::Home) => self.go_home(),
                Some(Exit::Rescan(p)) => self.start_scan(&ctx, p),
                None => {}
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Start screen

enum StartAct {
    Scan(PathBuf),
    View(PathBuf),
    Forget(PathBuf),
}

impl App {
    fn start_ui(&mut self, ui: &mut Ui) -> Option<StartAct> {
        let mut act = None;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                ui.heading(RichText::new("Disks and Folders").size(24.0).strong());
                ui.label(RichText::new("Pick a disk to see what's taking up space").weak());
            });
            ui.add_space(16.0);

            egui::ScrollArea::vertical().show(ui, |ui| {
                let width = ui.available_width().min(720.0);
                ui.vertical_centered(|ui| {
                    ui.set_max_width(width);
                    for v in &self.volumes {
                        if let Some(a) = volume_card(ui, v, self.saved.contains_key(&v.mount)) {
                            act = Some(a);
                        }
                        ui.add_space(8.0);
                    }

                    let folders: Vec<&Report> = self
                        .saved
                        .values()
                        .filter(|r| r.tree.volume.is_none())
                        .collect();
                    if !folders.is_empty() {
                        ui.add_space(8.0);
                        ui.label(RichText::new("Scanned folders").strong());
                        for r in folders {
                            if let Some(a) = folder_card(ui, &r.tree) {
                                act = Some(a);
                            }
                            ui.add_space(6.0);
                        }
                    }

                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        if ui.button("Scan Folder…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                                act = Some(StartAct::Scan(p));
                            }
                        }
                        ui.label(RichText::new("or drop a folder onto this window").weak());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Full Disk Access…").on_hover_text(
                                "Without Full Disk Access some folders can't be read and show up as hidden space",
                            ).clicked() {
                                util::open_full_disk_access_settings();
                            }
                        });
                    });
                    if let Some(s) = &self.status {
                        ui.add_space(8.0);
                        ui.label(RichText::new(s).weak());
                    }
                });
            });
        });
        act
    }
}

fn usage_bar(ui: &mut Ui, used: u64, total: u64) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 5.0, ui.visuals().extreme_bg_color);
    let frac = if total > 0 { used as f32 / total as f32 } else { 0.0 };
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    let color = if frac > 0.9 {
        Color32::from_rgb(230, 90, 80)
    } else if frac > 0.75 {
        Color32::from_rgb(235, 180, 60)
    } else {
        Color32::from_rgb(90, 170, 120)
    };
    p.rect_filled(fill, 5.0, color);
}

fn volume_card(ui: &mut Ui, v: &Volume, has_result: bool) -> Option<StartAct> {
    let mut act = None;
    let frame = egui::Frame::group(ui.style()).inner_margin(12.0).corner_radius(8.0);
    let resp = frame
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&v.name).size(16.0).strong());
                    let mut detail = format!("{} · {}", v.mount.display(), v.fs_type);
                    if v.read_only {
                        detail.push_str(" · read-only");
                    }
                    if !v.local {
                        detail.push_str(" · network");
                    }
                    ui.label(RichText::new(detail).weak().small());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if has_result {
                        if ui.button("Rescan").clicked() {
                            act = Some(StartAct::Scan(v.mount.clone()));
                        }
                        if ui.button(RichText::new("View").strong()).clicked() {
                            act = Some(StartAct::View(v.mount.clone()));
                        }
                    } else if ui.button(RichText::new("Scan").strong()).clicked() {
                        act = Some(StartAct::Scan(v.mount.clone()));
                    }
                });
            });
            ui.add_space(6.0);
            usage_bar(ui, v.used(), v.total);
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} used", fmt_size(v.used()))).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{} free of {}", fmt_size(v.free), fmt_size(v.total)))
                            .small(),
                    );
                });
            });
        })
        .response;
    resp.context_menu(|ui| {
        if ui.button("Scan").clicked() {
            act = Some(StartAct::Scan(v.mount.clone()));
            ui.close();
        }
        if has_result && ui.button("Forget Scan Result").clicked() {
            act = Some(StartAct::Forget(v.mount.clone()));
            ui.close();
        }
        ui.separator();
        if ui.button("Show in Finder").clicked() {
            util::open_path(&v.mount);
            ui.close();
        }
        if v.mount != Path::new("/") && ui.button(format!("Eject “{}”", v.name)).clicked() {
            util::eject(&v.mount);
            ui.close();
        }
        if ui.button("Launch Disk Utility…").clicked() {
            util::open_disk_utility();
            ui.close();
        }
    });
    act
}

fn folder_card(ui: &mut Ui, tree: &Tree) -> Option<StartAct> {
    let mut act = None;
    let root = &tree.root_path;
    egui::Frame::group(ui.style()).inner_margin(10.0).corner_radius(8.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(tree.display_name()).strong());
                ui.label(RichText::new(root.display().to_string()).weak().small());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Forget scan result").clicked() {
                    act = Some(StartAct::Forget(root.clone()));
                }
                if ui.button("Rescan").clicked() {
                    act = Some(StartAct::Scan(root.clone()));
                }
                if ui.button(RichText::new("View").strong()).clicked() {
                    act = Some(StartAct::View(root.clone()));
                }
                ui.label(fmt_size(tree.node(Tree::ROOT).size));
            });
        });
    });
    act
}

// ---------------------------------------------------------------------------
// Scanning screen

/// Returns true when the user asks to cancel.
fn scanning_ui(ui: &mut Ui, job: &ScanJob) -> bool {
    let mut cancel = false;
    egui::CentralPanel::default().show(ui, |ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.vertical_centered(|ui| {
            ui.set_max_width(460.0);
            ui.heading(format!("Scanning “{}”…", job.label));
            ui.label(RichText::new(job.root.display().to_string()).weak().small());
            ui.add_space(12.0);

            let bytes = job.progress.bytes.load(Relaxed);
            let files = job.progress.files.load(Relaxed);
            let elapsed = job.started.elapsed();
            match job.expected.filter(|&e| e > 0) {
                Some(expected) => {
                    let frac = (bytes as f32 / expected as f32).min(0.99);
                    ui.add(egui::ProgressBar::new(frac).show_percentage());
                    if frac > 0.02 && elapsed > Duration::from_secs(2) {
                        let left = elapsed.mul_f32((1.0 - frac) / frac);
                        ui.label(format!("about {} left", fmt_duration(left)));
                    }
                }
                None => {
                    ui.add(egui::ProgressBar::new(0.0).animate(true));
                }
            }
            ui.add_space(6.0);
            ui.label(format!(
                "{} files · {} · {}",
                fmt_count(files),
                fmt_size(bytes),
                fmt_duration(elapsed)
            ));
            ui.add_space(12.0);
            if ui.button("Cancel Scanning").clicked() {
                cancel = true;
            }
        });
    });
    cancel
}

// ---------------------------------------------------------------------------
// Report screen

impl Report {
    fn new(tree: Tree) -> Self {
        Self {
            tree,
            focus: Tree::ROOT,
            back: Vec::new(),
            forward: Vec::new(),
            hovered: None,
            menu_target: None,
            expand_smaller: false,
            collector: Vec::new(),
            deletion: None,
            permanent: false,
            freed: 0,
            banner: None,
            errors: Vec::new(),
        }
    }

    fn apply(&mut self, act: Act) {
        match act {
            Act::Navigate(id) if id != self.focus => {
                self.back.push(self.focus);
                self.forward.clear();
                self.focus = id;
                self.expand_smaller = false;
            }
            Act::Navigate(_) => {}
            Act::Back => {
                if let Some(id) = self.back.pop() {
                    self.forward.push(self.focus);
                    self.focus = id;
                    self.expand_smaller = false;
                }
            }
            Act::Forward => {
                if let Some(id) = self.forward.pop() {
                    self.back.push(self.focus);
                    self.focus = id;
                    self.expand_smaller = false;
                }
            }
            Act::Up => {
                if let Some(p) = self.tree.node(self.focus).parent {
                    self.apply(Act::Navigate(p));
                }
            }
            Act::Collect(id) => self.collect(id),
            Act::Uncollect(id) => self.collector.retain(|&c| c != id),
            Act::ToggleSmaller => self.expand_smaller = !self.expand_smaller,
        }
    }

    fn collect(&mut self, id: NodeId) {
        let n = self.tree.node(id);
        if id == Tree::ROOT || !n.is_real() || self.deletion.is_some() {
            return;
        }
        if self.collector.iter().any(|&c| self.tree.contains(c, id)) {
            return;
        }
        let tree = &self.tree;
        self.collector.retain(|&c| !tree.contains(id, c));
        self.collector.push(id);
        self.banner = None;
    }

    fn is_collected(&self, id: NodeId) -> bool {
        self.collector.iter().any(|&c| self.tree.contains(c, id))
    }

    fn target_info(&self, t: Target) -> (String, u64, u64) {
        match t {
            Target::Node(id) => {
                let n = self.tree.node(id);
                (n.name.to_string(), n.size, n.items)
            }
            Target::Smaller { parent, first } => {
                let kids = &self.tree.node(parent).children[first..];
                let size = kids.iter().map(|&k| self.tree.node(k).size).sum();
                let items = kids.iter().map(|&k| self.tree.node(k).items).sum();
                (format!("smaller objects ({})", kids.len()), size, items)
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui) -> Option<Exit> {
        let ctx = ui.ctx().clone();
        let mut acts = Vec::new();
        let mut exit = None;
        self.poll_deletion(&ctx);

        ctx.input(|i| {
            if i.modifiers.command && i.key_pressed(Key::OpenBracket) {
                acts.push(Act::Back);
            }
            if i.modifiers.command && i.key_pressed(Key::CloseBracket) {
                acts.push(Act::Forward);
            }
            if (i.modifiers.command && i.key_pressed(Key::ArrowUp)) || i.key_pressed(Key::Backspace) {
                acts.push(Act::Up);
            }
        });

        let segments = sunburst::layout(&self.tree, self.focus);
        let mut hover: Option<Target> = None;

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let busy = self.deletion.is_some();
                if ui.add_enabled(!busy, egui::Button::new("◀ Disks")).clicked() {
                    exit = Some(Exit::Home);
                }
                ui.separator();
                if ui.add_enabled(!self.back.is_empty(), egui::Button::new("‹")).on_hover_text("Back (⌘[)").clicked() {
                    acts.push(Act::Back);
                }
                if ui.add_enabled(!self.forward.is_empty(), egui::Button::new("›")).on_hover_text("Forward (⌘])").clicked() {
                    acts.push(Act::Forward);
                }
                ui.add_space(6.0);
                let chain = self.tree.ancestry(self.focus);
                for (i, &id) in chain.iter().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new("›").weak());
                    }
                    let name = self.tree.node(id).name.to_string();
                    let text = if id == self.focus { RichText::new(name).strong() } else { RichText::new(name) };
                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                        acts.push(Act::Navigate(id));
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Rescan")).clicked() {
                        exit = Some(Exit::Rescan(self.tree.root_path.clone()));
                    }
                    if ui.button("Export CSV…").clicked() {
                        self.export_csv();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("collector").resizable(false).exact_size(104.0).show(ui, |ui| {
            self.collector_ui(ui, &mut acts);
        });

        egui::Panel::left("list").resizable(true).default_size(340.0).min_size(240.0).show(ui, |ui| {
            if let Some(h) = self.list_ui(ui, &segments, &mut acts) {
                hover = Some(h);
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            self.summary_ui(ui);
            if let Some(h) = self.chart_ui(ui, &segments, &mut acts) {
                hover = hover.or(Some(h));
            }
        });

        // Ghost label that follows the pointer while dragging an item.
        if let Some(item) = DragAndDrop::payload::<DragItem>(&ctx) {
            if let Some(pos) = ctx.pointer_latest_pos() {
                let n = self.tree.node(item.0);
                let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("drag-ghost")));
                let text = format!("{}  {}", n.name, fmt_size(n.size));
                let galley = painter.layout_no_wrap(text, FontId::proportional(13.0), Color32::WHITE);
                let rect = egui::Rect::from_min_size(pos + Vec2::new(14.0, 6.0), galley.size())
                    .expand(6.0);
                painter.rect_filled(rect, 6.0, Color32::from_black_alpha(200));
                painter.galley(rect.min + Vec2::splat(6.0), galley, Color32::WHITE);
            }
        }

        self.hovered = hover;
        for a in acts {
            self.apply(a);
        }
        exit
    }

    fn summary_ui(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            let root = self.tree.node(Tree::ROOT);
            if let Some(v) = self.tree.volume {
                ui.label(format!("{} used", fmt_size(v.total.saturating_sub(v.free))));
                ui.label(RichText::new("·").weak());
                ui.label(format!("{} free", fmt_size(v.free)));
                ui.label(RichText::new("·").weak());
            } else {
                ui.label(format!("{} total", fmt_size(root.size)));
                ui.label(RichText::new("·").weak());
            }
            ui.label(
                RichText::new(format!(
                    "{} files scanned in {}",
                    fmt_count(root.items),
                    fmt_duration(self.tree.duration)
                ))
                .weak(),
            );
            if self.tree.restricted_dirs > 0 {
                ui.label(RichText::new("·").weak());
                ui.label(
                    RichText::new(format!("{} restricted folders", fmt_count(self.tree.restricted_dirs)))
                        .color(Color32::from_rgb(235, 180, 60)),
                )
                .on_hover_text(
                    "These folders couldn't be read. Give this app (or the terminal running it) Full Disk Access, then rescan.",
                );
                if ui.small_button("Full Disk Access…").clicked() {
                    util::open_full_disk_access_settings();
                }
            }
        });
    }

    fn rows(&self, segments: &[sunburst::Segment]) -> Vec<(Target, Color32)> {
        let mut rows = Vec::new();
        for s in segments.iter().filter(|s| s.depth == 0) {
            rows.push((s.target, s.color));
            if let (true, Target::Smaller { parent, first }) = (self.expand_smaller, s.target) {
                for &c in &self.tree.node(parent).children[first..] {
                    rows.push((Target::Node(c), sunburst::smaller_color(1)));
                }
            }
        }
        rows
    }

    fn list_ui(&mut self, ui: &mut Ui, segments: &[sunburst::Segment], acts: &mut Vec<Act>) -> Option<Target> {
        let mut hover = None;
        let focus = self.tree.node(self.focus);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&*focus.name).size(16.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(fmt_size(focus.size)).size(16.0));
            });
        });
        if focus.parent.is_some() {
            ui.label(RichText::new(self.tree.path(self.focus).display().to_string()).weak().small());
        }
        ui.separator();

        let rows = self.rows(segments);
        if rows.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.label(RichText::new("This folder is empty").weak()));
            return None;
        }
        let total = focus.size.max(1);
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for (target, color) in &rows[range] {
                if let Some(h) = self.row_ui(ui, *target, *color, total, acts) {
                    hover = Some(h);
                }
            }
        });
        hover
    }

    fn row_ui(&mut self, ui: &mut Ui, target: Target, color: Color32, total: u64, acts: &mut Vec<Act>) -> Option<Target> {
        let (name, size, _) = self.target_info(target);
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click_and_drag());
        let node = match target {
            Target::Node(id) => Some(self.tree.node(id)),
            Target::Smaller { .. } => None,
        };
        let collected = matches!(target, Target::Node(id) if self.is_collected(id));
        let p = ui.painter();
        if resp.hovered() || self.hovered == Some(target) {
            p.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
        }
        let text_color = if collected { ui.visuals().weak_text_color() } else { ui.visuals().text_color() };
        p.circle_filled(rect.left_center() + Vec2::new(10.0, 0.0), 5.0, color);

        let size_text = fmt_size(size);
        let size_w = 70.0;
        let name_rect = egui::Rect::from_min_max(
            rect.min + Vec2::new(22.0, 0.0),
            egui::pos2(rect.right() - size_w - 18.0, rect.bottom()),
        );
        let mut label = name;
        if node.is_some_and(|n| n.kind == Kind::Restricted) {
            label.push_str("  🔒");
        }
        p.with_clip_rect(name_rect).text(
            name_rect.left_center() - Vec2::new(0.0, 2.0),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            text_color,
        );
        // Thin bar showing share of the current folder.
        let frac = size as f32 / total as f32;
        let bar = egui::Rect::from_min_size(
            egui::pos2(name_rect.left(), rect.bottom() - 5.0),
            Vec2::new(name_rect.width() * frac.clamp(0.0, 1.0), 2.0),
        );
        p.rect_filled(bar, 1.0, color.gamma_multiply(0.7));
        p.text(
            egui::pos2(rect.right() - 18.0, rect.center().y),
            Align2::RIGHT_CENTER,
            size_text,
            FontId::proportional(12.5),
            text_color,
        );
        if node.is_some_and(|n| n.is_container()) || matches!(target, Target::Smaller { .. }) {
            let glyph = match (target, self.expand_smaller) {
                (Target::Smaller { .. }, true) => "⏷",
                _ => "⏵",
            };
            p.text(rect.right_center() - Vec2::new(4.0, 0.0), Align2::RIGHT_CENTER, glyph, FontId::proportional(11.0), ui.visuals().weak_text_color());
        }

        let resp = match node.map(|n| n.kind) {
            Some(Kind::Hidden) => resp.on_hover_text(
                "Space in use on this disk that the scan couldn't attribute to files: other APFS volumes, local snapshots, purgeable caches and folders that couldn't be read.",
            ),
            Some(Kind::Restricted) => resp.on_hover_text("Restricted folder: grant Full Disk Access and rescan to see inside."),
            _ => resp,
        };

        self.interact(&resp, target, acts);
        if resp.double_clicked() {
            if let Some(Target::Node(id)) = Some(target) {
                if self.tree.node(id).kind == Kind::File {
                    util::open_path(&self.tree.path(id));
                }
            }
        }
        resp.hovered().then_some(target)
    }

    /// Shared click / drag / context-menu handling for list rows and chart segments.
    fn interact(&mut self, resp: &egui::Response, target: Target, acts: &mut Vec<Act>) {
        if resp.clicked() {
            match target {
                Target::Node(id) if self.tree.node(id).is_container() => acts.push(Act::Navigate(id)),
                Target::Smaller { .. } => acts.push(Act::ToggleSmaller),
                _ => {}
            }
        }
        if resp.drag_started() {
            if let Target::Node(id) = target {
                if self.tree.node(id).is_real() && id != Tree::ROOT {
                    DragAndDrop::set_payload(&resp.ctx, DragItem(id));
                }
            }
        }
        if resp.secondary_clicked() {
            self.menu_target = Some(target);
        }
        resp.context_menu(|ui| {
            if let Some(t) = self.menu_target {
                self.context_menu(ui, t, acts);
            }
        });
    }

    fn context_menu(&self, ui: &mut Ui, target: Target, acts: &mut Vec<Act>) {
        let id = match target {
            Target::Node(id) => id,
            Target::Smaller { .. } => {
                let label = if self.expand_smaller { "Collapse “smaller objects”" } else { "Expand “smaller objects”" };
                if ui.button(label).clicked() {
                    acts.push(Act::ToggleSmaller);
                    ui.close();
                }
                return;
            }
        };
        let n = self.tree.node(id);
        if n.kind == Kind::Hidden {
            if ui.button("Open “Full Disk Access”").clicked() {
                util::open_full_disk_access_settings();
                ui.close();
            }
            return;
        }
        let path = self.tree.path(id);
        if n.is_container() && id != self.focus && ui.button(format!("Zoom into “{}”", n.name)).clicked() {
            acts.push(Act::Navigate(id));
            ui.close();
        }
        if ui.button("Open").clicked() {
            util::open_path(&path);
            ui.close();
        }
        if ui.button("Show in Finder").clicked() {
            util::reveal_in_finder(&path);
            ui.close();
        }
        if ui.button("Quick Look").clicked() {
            util::quick_look(&path);
            ui.close();
        }
        if ui.button("Copy Path").clicked() {
            ui.ctx().copy_text(path.to_string_lossy().into_owned());
            ui.close();
        }
        ui.separator();
        if self.collector.contains(&id) {
            if ui.button(format!("Remove “{}” from Collector", n.name)).clicked() {
                acts.push(Act::Uncollect(id));
                ui.close();
            }
        } else {
            let can = id != Tree::ROOT && self.deletion.is_none() && !self.is_collected(id);
            if ui.add_enabled(can, egui::Button::new(format!("Move “{}” to Collector", n.name))).clicked() {
                acts.push(Act::Collect(id));
                ui.close();
            }
        }
    }

    fn chart_ui(&mut self, ui: &mut Ui, segments: &[sunburst::Segment], acts: &mut Vec<Act>) -> Option<Target> {
        let rect = ui.available_rect_before_wrap();
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());
        let g = Geometry::new(rect);

        let hit = resp.hover_pos().and_then(|p| g.hit(segments, p));
        let hover = match hit {
            Some(Hit::Segment(i)) => Some(segments[i].target),
            _ => None,
        };
        let painter = ui.painter_at(rect);
        sunburst::paint(&painter, &g, &self.tree, segments, self.hovered.or(hover));

        // Centre disc: shows what's under the pointer, or the current folder.
        let center_hovered = matches!(hit, Some(Hit::Center));
        let fill = if center_hovered && self.focus != Tree::ROOT {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().extreme_bg_color
        };
        painter.circle_filled(g.center, g.core - 1.0, fill);
        let shown = self.hovered.or(hover).unwrap_or(Target::Node(self.focus));
        let (mut name, size, items) = self.target_info(shown);
        if name.chars().count() > 22 {
            name = name.chars().take(21).collect::<String>() + "…";
        }
        let text = ui.visuals().text_color();
        painter.text(g.center - Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, name, FontId::proportional(13.0), text);
        painter.text(g.center + Vec2::new(0.0, 4.0), Align2::CENTER_CENTER, fmt_size(size), FontId::proportional(18.0), text);
        if items > 1 {
            painter.text(
                g.center + Vec2::new(0.0, 22.0),
                Align2::CENTER_CENTER,
                format!("{} files", fmt_count(items)),
                FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
        }
        if center_hovered && self.focus != Tree::ROOT {
            resp.clone().on_hover_text("Click to go up a level");
        }

        match hit {
            Some(Hit::Segment(i)) => self.interact(&resp, segments[i].target, acts),
            Some(Hit::Center) => {
                if resp.clicked() {
                    acts.push(Act::Up);
                }
                if resp.secondary_clicked() {
                    self.menu_target = Some(Target::Node(self.focus));
                }
                resp.context_menu(|ui| {
                    if let Some(t) = self.menu_target {
                        self.context_menu(ui, t, acts);
                    }
                });
            }
            None => {
                resp.context_menu(|ui| {
                    if let Some(t) = self.menu_target {
                        self.context_menu(ui, t, acts);
                    }
                });
            }
        }
        hover
    }

    // -- Collector ----------------------------------------------------------

    fn collector_ui(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        let area = ui.max_rect();
        let total: u64 = self.collector.iter().map(|&c| self.tree.node(c).size).sum();

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Collector").strong());
            let summary = if let Some(b) = &self.banner {
                RichText::new(b).color(Color32::from_rgb(110, 200, 130))
            } else if self.collector.is_empty() {
                RichText::new("Drag files and folders here to collect them for deletion").weak()
            } else {
                RichText::new(format!("{} collected", fmt_size(total)))
            };
            ui.label(summary);

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| match &self.deletion {
                None => {
                    let enabled = !self.collector.is_empty();
                    let btn = egui::Button::new(RichText::new("Delete").strong().color(Color32::WHITE))
                        .fill(Color32::from_rgb(200, 60, 55));
                    if ui.add_enabled(enabled, btn).clicked() {
                        self.deletion = Some(Deletion::Countdown(Instant::now() + DELETE_COUNTDOWN));
                        self.banner = None;
                        self.errors.clear();
                    }
                    ui.checkbox(&mut self.permanent, "Delete permanently")
                        .on_hover_text("When off, items are moved to the Trash");
                }
                Some(Deletion::Countdown(at)) => {
                    let left = at.saturating_duration_since(Instant::now()).as_secs() + 1;
                    if ui.button("Stop").clicked() {
                        self.deletion = None;
                    }
                    let fate = if self.permanent { "deleted forever!" } else { "moved to the Trash." };
                    ui.label(
                        RichText::new(format!("{left} seconds to start. The files will be {fate}"))
                            .color(Color32::from_rgb(235, 180, 60)),
                    );
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                }
                Some(Deletion::Running(_)) => {
                    ui.spinner();
                    ui.label("Deletion in progress…");
                }
            });
        });

        egui::ScrollArea::horizontal().id_salt("collector-items").show(ui, |ui| {
            ui.horizontal(|ui| {
                for &id in &self.collector.clone() {
                    let n = self.tree.node(id);
                    let resp = egui::Frame::new()
                        .fill(ui.visuals().widgets.inactive.weak_bg_fill)
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(6.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(if n.kind == Kind::File { "📄" } else { "📁" });
                                ui.label(&*n.name);
                                ui.label(RichText::new(fmt_size(n.size)).weak());
                                if self.deletion.is_none() && ui.small_button("✕").on_hover_text("Remove from Collector").clicked() {
                                    acts.push(Act::Uncollect(id));
                                }
                            });
                        })
                        .response
                        .on_hover_text(self.tree.path(id).display().to_string());
                    resp.context_menu(|ui| self.context_menu(ui, Target::Node(id), acts));
                }
            });
        });
        for e in self.errors.iter().take(2) {
            ui.label(RichText::new(e).small().color(Color32::from_rgb(230, 90, 80)));
        }

        // The whole panel is a drop target.
        let drop = ui.interact(area, Id::new("collector-drop"), Sense::hover());
        if drop.dnd_hover_payload::<DragItem>().is_some() {
            ui.painter().rect_stroke(
                area.shrink(2.0),
                8.0,
                Stroke::new(2.0, Color32::from_rgb(90, 160, 240)),
                StrokeKind::Inside,
            );
        }
        if let Some(item) = drop.dnd_release_payload::<DragItem>() {
            acts.push(Act::Collect(item.0));
        }
    }

    fn poll_deletion(&mut self, ctx: &egui::Context) {
        match &self.deletion {
            Some(Deletion::Countdown(at)) if Instant::now() >= *at => {
                let jobs: Vec<(NodeId, PathBuf)> =
                    self.collector.iter().map(|&id| (id, self.tree.path(id))).collect();
                let permanent = self.permanent;
                let (tx, rx) = mpsc::channel();
                let c = ctx.clone();
                thread::spawn(move || {
                    let mut out = DeleteOutcome { removed: Vec::new(), failed: Vec::new() };
                    for (id, path) in jobs {
                        let result = if permanent {
                            remove_permanently(&path)
                        } else {
                            trash::delete(&path).map_err(|e| e.to_string())
                        };
                        match result {
                            Ok(()) => out.removed.push(id),
                            Err(e) => out.failed.push((id, format!("{}: {e}", path.display()))),
                        }
                    }
                    let _ = tx.send(out);
                    c.request_repaint();
                });
                self.deletion = Some(Deletion::Running(rx));
            }
            Some(Deletion::Running(rx)) => {
                let Ok(out) = rx.try_recv() else { return };
                let mut freed = 0;
                for &id in &out.removed {
                    freed += self.tree.node(id).size;
                    self.tree.remove(id);
                }
                self.freed += freed;
                self.collector.retain(|c| out.failed.iter().any(|(f, _)| f == c));
                self.errors = out.failed.into_iter().map(|(_, e)| e).collect();
                self.deletion = None;
                if freed > 0 {
                    self.banner = Some(format!("Nice! You’ve freed up {}.", fmt_size(freed)));
                }
                // Keep navigation pointing at nodes that still exist.
                while !self.tree.is_attached(self.focus) {
                    self.focus = self.tree.node(self.focus).parent.unwrap_or(Tree::ROOT);
                }
                let tree = &self.tree;
                self.back.retain(|&id| tree.is_attached(id));
                self.forward.retain(|&id| tree.is_attached(id));
                self.hovered = None;
                self.menu_target = None;
            }
            _ => {}
        }
    }

    fn export_csv(&self) {
        let name = format!("{}.csv", self.tree.node(self.focus).name);
        if let Some(dest) = rfd::FileDialog::new().set_file_name(name).add_filter("CSV", &["csv"]).save_file() {
            if let Err(e) = util::export_csv(&self.tree, self.focus, &dest) {
                rfd::MessageDialog::new()
                    .set_title("Export failed")
                    .set_description(e.to_string())
                    .show();
            }
        }
    }
}

fn remove_permanently(path: &Path) -> Result<(), String> {
    let md = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if md.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }.map_err(|e| e.to_string())
}
