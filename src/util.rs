//! Formatting helpers and macOS integration (Finder, Quick Look, notifications).

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::model::{Kind, NodeId, Tree};

/// Decimal (1000-based) sizes, matching how Finder reports capacity.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = "B";
    for u in UNITS {
        if v < 1000.0 {
            break;
        }
        v /= 1000.0;
        unit = u;
    }
    if v < 100.0 { format!("{v:.1} {unit}") } else { format!("{v:.0} {unit}") }
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0 => format!("{} ms", d.as_millis()),
        1..=59 => format!("{s} sec"),
        60..=3599 => format!("{} min {} sec", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn reveal_in_finder(path: &Path) {
    let _ = Command::new("open").arg("-R").arg(path).spawn();
}

pub fn open_path(path: &Path) {
    let _ = Command::new("open").arg(path).spawn();
}

pub fn quick_look(path: &Path) {
    let _ = Command::new("qlmanage").arg("-p").arg(path).spawn();
}

pub fn open_full_disk_access_settings() {
    let _ = Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn();
}

pub fn open_disk_utility() {
    let _ = Command::new("open").args(["-b", "com.apple.DiskUtility"]).spawn();
}

pub fn eject(mount: &Path) {
    let _ = Command::new("diskutil").arg("eject").arg(mount).spawn();
}

pub fn notify(title: &str, message: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script =
        format!("display notification \"{}\" with title \"{}\"", esc(message), esc(title));
    let _ = Command::new("osascript").arg("-e").arg(script).spawn();
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.into() }
}

/// Write every node below `from` as `path,kind,size_bytes,items`.
pub fn export_csv(tree: &Tree, from: NodeId, dest: &Path) -> io::Result<()> {
    let mut w = BufWriter::new(File::create(dest)?);
    writeln!(w, "path,kind,size_bytes,items")?;
    let mut result = Ok(());
    tree.walk(from, &mut |id| {
        if result.is_err() {
            return;
        }
        let n = tree.node(id);
        let kind = match n.kind {
            Kind::Dir => "folder",
            Kind::File => "file",
            Kind::Restricted => "restricted",
            Kind::Hidden => "hidden",
        };
        let path = if n.kind == Kind::Hidden {
            n.name.to_string()
        } else {
            tree.path(id).to_string_lossy().into_owned()
        };
        result = writeln!(w, "{},{kind},{},{}", csv_field(&path), n.size, n.items);
    });
    result?;
    w.flush()
}
