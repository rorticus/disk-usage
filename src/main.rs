mod app;
mod model;
mod sunburst;
mod util;
mod volumes;

use eframe::egui;

/// `disk-usage --scan <path>` prints the largest items without opening a window.
fn scan_cli(path: &str) {
    let tree = model::scan(std::path::Path::new(path), &model::Progress::default())
        .expect("scan failed");
    let root = tree.node(model::Tree::ROOT);
    println!(
        "{}: {} in {} files, scanned in {} ({} restricted folders)",
        tree.display_name(),
        util::fmt_size(root.size),
        util::fmt_count(root.items),
        util::fmt_duration(tree.duration),
        tree.restricted_dirs
    );
    if let Some(v) = tree.volume {
        println!("volume: {} total, {} free", util::fmt_size(v.total), util::fmt_size(v.free));
    }
    for &c in root.children.iter().take(15) {
        let n = tree.node(c);
        println!("{:>10}  {}{}", util::fmt_size(n.size), n.name, if n.kind == model::Kind::Dir { "/" } else { "" });
    }
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, path] = args.as_slice() {
        if flag == "--scan" {
            scan_cli(path);
            return Ok(());
        }
    }
    let pending = args.get(1).map(std::path::PathBuf::from).filter(|p| p.is_dir());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Disk Usage")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([760.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("Disk Usage", options, Box::new(|cc| Ok(Box::new(app::App::new(cc, pending)))))
}
