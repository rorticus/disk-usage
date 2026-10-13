//! Scanned file tree and the parallel scanner that builds it.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::volumes;

pub type NodeId = u32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    /// A directory we were not allowed to read.
    Restricted,
    /// Space used on the volume that the scan could not attribute to any file
    /// (other APFS volumes, snapshots, purgeable caches, restricted files).
    Hidden,
}

pub struct Node {
    pub name: Box<str>,
    pub parent: Option<NodeId>,
    /// Sorted by size, largest first.
    pub children: Vec<NodeId>,
    /// Allocated bytes on disk, including all descendants.
    pub size: u64,
    /// Number of files at or below this node.
    pub items: u64,
    pub kind: Kind,
}

impl Node {
    pub fn is_real(&self) -> bool {
        matches!(self.kind, Kind::Dir | Kind::File | Kind::Restricted)
    }

    pub fn is_container(&self) -> bool {
        self.kind == Kind::Dir && !self.children.is_empty()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct VolumeSpace {
    pub total: u64,
    pub free: u64,
}

pub struct Tree {
    pub root_path: PathBuf,
    pub nodes: Vec<Node>,
    pub volume: Option<VolumeSpace>,
    pub duration: Duration,
    pub restricted_dirs: u64,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn display_name(&self) -> String {
        self.node(Self::ROOT).name.to_string()
    }

    /// Chain of ids from the root down to `id`, inclusive.
    pub fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut chain = vec![id];
        let mut cur = id;
        while let Some(p) = self.node(cur).parent {
            chain.push(p);
            cur = p;
        }
        chain.reverse();
        chain
    }

    pub fn path(&self, id: NodeId) -> PathBuf {
        let mut path = self.root_path.clone();
        for n in self.ancestry(id).into_iter().skip(1) {
            path.push(&*self.node(n).name);
        }
        path
    }

    /// True if `a` is `b` or one of its ancestors.
    pub fn contains(&self, a: NodeId, b: NodeId) -> bool {
        let mut cur = Some(b);
        while let Some(c) = cur {
            if c == a {
                return true;
            }
            cur = self.node(c).parent;
        }
        false
    }

    /// Whether the node is still attached to the tree (not deleted).
    pub fn is_attached(&self, id: NodeId) -> bool {
        let mut cur = id;
        while let Some(p) = self.node(cur).parent {
            if !self.node(p).children.contains(&cur) {
                return false;
            }
            cur = p;
        }
        true
    }

    /// Detach a node after it has been deleted from disk, updating ancestor totals.
    pub fn remove(&mut self, id: NodeId) {
        let Some(parent) = self.node(id).parent else { return };
        let (size, items) = (self.node(id).size, self.node(id).items);
        self.nodes[parent as usize].children.retain(|&c| c != id);
        let mut cur = Some(parent);
        while let Some(c) = cur {
            let n = &mut self.nodes[c as usize];
            n.size = n.size.saturating_sub(size);
            n.items = n.items.saturating_sub(items);
            cur = n.parent;
        }
        if let Some(v) = &mut self.volume {
            v.free += size;
        }
    }

    /// Depth-first walk of attached nodes below `id`, including `id`.
    pub fn walk(&self, id: NodeId, f: &mut impl FnMut(NodeId)) {
        f(id);
        for &c in &self.node(id).children {
            self.walk(c, f);
        }
    }
}

#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
}

/// Intermediate owned tree produced by the parallel walk.
struct Raw {
    name: Box<str>,
    size: u64,
    items: u64,
    kind: Kind,
    children: Vec<Raw>,
}

struct Ctx<'a> {
    progress: &'a Progress,
    /// Devices the scan may enter; prevents crossing into other mounts.
    devices: Vec<u64>,
    skip: Vec<PathBuf>,
    /// Hard-linked files are counted once.
    seen_links: Mutex<HashSet<(u64, u64)>>,
    restricted: AtomicU64,
}

fn scan_dir(path: &Path, name: Box<str>, own_size: u64, ctx: &Ctx) -> Raw {
    let mut node = Raw { name, size: own_size, items: 0, kind: Kind::Dir, children: Vec::new() };
    ctx.progress.bytes.fetch_add(own_size, Relaxed);
    if ctx.progress.cancel.load(Relaxed) {
        return node;
    }
    let entries = match fs::read_dir(path) {
        Ok(rd) => rd,
        Err(_) => {
            node.kind = Kind::Restricted;
            ctx.restricted.fetch_add(1, Relaxed);
            return node;
        }
    };

    let mut subdirs = Vec::new();
    let mut file_bytes = 0;
    for entry in entries.flatten() {
        // DirEntry::metadata does not follow symlinks on macOS.
        let Ok(md) = entry.metadata() else { continue };
        let name: Box<str> = entry.file_name().to_string_lossy().into();
        let allocated = md.blocks() * 512;
        if md.is_dir() {
            let p = entry.path();
            if !ctx.devices.contains(&md.dev()) || ctx.skip.contains(&p) {
                continue;
            }
            subdirs.push((p, name, allocated));
        } else {
            let first_link =
                md.nlink() <= 1 || ctx.seen_links.lock().unwrap().insert((md.dev(), md.ino()));
            let size = if first_link { allocated } else { 0 };
            file_bytes += size;
            node.children.push(Raw { name, size, items: 1, kind: Kind::File, children: Vec::new() });
        }
    }
    ctx.progress.files.fetch_add(node.children.len() as u64, Relaxed);
    ctx.progress.bytes.fetch_add(file_bytes, Relaxed);

    let dirs: Vec<Raw> =
        subdirs.into_par_iter().map(|(p, n, s)| scan_dir(&p, n, s, ctx)).collect();
    node.children.extend(dirs);
    node.size += node.children.iter().map(|c| c.size).sum::<u64>();
    node.items = node.children.iter().map(|c| c.items).sum();
    node
}

fn flatten(raw: Raw, parent: Option<NodeId>, nodes: &mut Vec<Node>) -> NodeId {
    let id = nodes.len() as NodeId;
    nodes.push(Node {
        name: raw.name,
        parent,
        children: Vec::new(),
        size: raw.size,
        items: raw.items,
        kind: raw.kind,
    });
    let mut kids = raw.children;
    kids.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    let ids = kids.into_iter().map(|c| flatten(c, Some(id), nodes)).collect();
    nodes[id as usize].children = ids;
    id
}

fn dev_of(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path).ok().map(|m| m.dev())
}

/// Scan `root` and build a tree. Returns `None` if cancelled.
pub fn scan(root: &Path, progress: &Progress) -> Option<Tree> {
    let started = Instant::now();
    let root_md = fs::metadata(root).ok()?;

    let mut devices = vec![root_md.dev()];
    let mut skip = Vec::new();
    if root == Path::new("/") {
        // The boot volume is split into a sealed system volume and a data
        // volume joined by firmlinks; walk both but skip the raw data mount
        // so nothing is counted twice.
        if let Some(d) = dev_of(Path::new("/System/Volumes/Data")) {
            devices.push(d);
        }
        skip.extend(["/System/Volumes", "/Volumes", "/dev"].map(PathBuf::from));
    }

    let ctx = Ctx {
        progress,
        devices,
        skip,
        seen_links: Mutex::default(),
        restricted: AtomicU64::new(0),
    };
    let volume = volumes::space_if_mount_point(root);
    let name: Box<str> = if volume.is_some() {
        volumes::volume_name(root).into()
    } else {
        root.file_name().map_or_else(|| root.to_string_lossy(), |n| n.to_string_lossy()).into()
    };

    let pool = rayon::ThreadPoolBuilder::new().stack_size(16 << 20).build().ok()?;
    let mut raw = pool.install(|| scan_dir(root, name, root_md.blocks() * 512, &ctx));
    if progress.cancel.load(Relaxed) {
        return None;
    }

    if let Some(v) = volume {
        let used = v.total.saturating_sub(v.free);
        let hidden = used.saturating_sub(raw.size);
        if hidden > 0 {
            raw.children.push(Raw {
                name: "hidden space".into(),
                size: hidden,
                items: 0,
                kind: Kind::Hidden,
                children: Vec::new(),
            });
            raw.size += hidden;
        }
    }

    let mut nodes = Vec::new();
    flatten(raw, None, &mut nodes);
    Some(Tree {
        root_path: root.to_path_buf(),
        nodes,
        volume,
        duration: started.elapsed(),
        restricted_dirs: ctx.restricted.load(Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("disk-usage-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/big.bin"), vec![1u8; 200_000]).unwrap();
        fs::write(dir.join("a/small.txt"), b"hi").unwrap();
        fs::write(dir.join("top.bin"), vec![1u8; 50_000]).unwrap();
        // A hard link must not be counted twice.
        fs::hard_link(dir.join("top.bin"), dir.join("a/link.bin")).unwrap();
        dir
    }

    #[test]
    fn scans_sorts_and_removes() {
        let dir = fixture("scan");
        let tree = scan(&dir, &Progress::default()).unwrap();
        let root = tree.node(Tree::ROOT);
        assert_eq!(root.items, 4);
        let sizes: Vec<u64> = root.children.iter().map(|&c| tree.node(c).size).collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "children sorted by size");
        let a = root.children[0];
        assert_eq!(&*tree.node(a).name, "a");
        assert_eq!(tree.path(a), dir.join("a"));

        let file_total: u64 = tree.nodes.iter().filter(|n| n.kind == Kind::File).map(|n| n.size).sum();
        let top = fs::metadata(dir.join("top.bin")).unwrap().blocks() * 512;
        let big = fs::metadata(dir.join("a/b/big.bin")).unwrap().blocks() * 512;
        let small = fs::metadata(dir.join("a/small.txt")).unwrap().blocks() * 512;
        assert_eq!(file_total, top + big + small, "hard link counted once");

        let mut tree = tree;
        let before = tree.node(Tree::ROOT).size;
        let b = tree.node(a).children.iter().copied().find(|&c| &*tree.node(c).name == "b").unwrap();
        let b_size = tree.node(b).size;
        tree.remove(b);
        assert_eq!(tree.node(Tree::ROOT).size, before - b_size);
        assert!(!tree.is_attached(b));
        assert_eq!(tree.node(Tree::ROOT).items, 3);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelled_scan_returns_none() {
        let dir = fixture("cancel");
        let p = Progress::default();
        p.cancel.store(true, Relaxed);
        assert!(scan(&dir, &p).is_none());
        fs::remove_dir_all(dir).unwrap();
    }
}
