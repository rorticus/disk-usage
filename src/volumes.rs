//! Mounted volume discovery via getmntinfo/statfs.

use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::model::VolumeSpace;

const MNT_RDONLY: u32 = 0x1;
const MNT_LOCAL: u32 = 0x1000;
const MNT_DONTBROWSE: u32 = 0x0010_0000;

#[derive(Clone, Debug)]
pub struct Volume {
    pub name: String,
    pub mount: PathBuf,
    pub fs_type: String,
    pub total: u64,
    pub free: u64,
    pub read_only: bool,
    pub local: bool,
}

impl Volume {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

fn cstr(buf: &[libc::c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn space(st: &libc::statfs) -> VolumeSpace {
    let bsize = st.f_bsize as u64;
    VolumeSpace { total: st.f_blocks * bsize, free: st.f_bavail * bsize }
}

fn statfs(path: &Path) -> Option<libc::statfs> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0).then_some(st)
}

/// Capacity of the volume when `path` is exactly its mount point.
pub fn space_if_mount_point(path: &Path) -> Option<VolumeSpace> {
    let st = statfs(path)?;
    (Path::new(&cstr(&st.f_mntonname)) == path).then(|| space(&st))
}

/// Human name of a mounted volume. The boot volume's name is found through
/// the symlink to `/` that macOS keeps in /Volumes.
pub fn volume_name(mount: &Path) -> String {
    if mount == Path::new("/") {
        if let Ok(entries) = fs::read_dir("/Volumes") {
            for e in entries.flatten() {
                if fs::read_link(e.path()).is_ok_and(|t| t == Path::new("/")) {
                    return e.file_name().to_string_lossy().into_owned();
                }
            }
        }
        return "Macintosh HD".into();
    }
    mount.file_name().map_or_else(|| mount.display().to_string(), |n| n.to_string_lossy().into())
}

pub fn list() -> Vec<Volume> {
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Vec::new();
    }
    let mounts = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    let mut out: Vec<Volume> = mounts
        .iter()
        .filter_map(|st| {
            let mount = PathBuf::from(cstr(&st.f_mntonname));
            let fs_type = cstr(&st.f_fstypename);
            let user_visible = mount == Path::new("/") || mount.starts_with("/Volumes/");
            if !user_visible || st.f_flags & MNT_DONTBROWSE != 0 || matches!(fs_type.as_str(), "devfs" | "autofs" | "nullfs") {
                return None;
            }
            let s = space(st);
            // The boot volume is a sealed read-only snapshot; its data lives elsewhere.
            let read_only = st.f_flags & MNT_RDONLY != 0 && mount != Path::new("/");
            Some(Volume {
                name: volume_name(&mount),
                mount,
                fs_type,
                total: s.total,
                free: s.free,
                read_only,
                local: st.f_flags & MNT_LOCAL != 0,
            })
        })
        .collect();
    out.sort_by_key(|v| (v.mount != Path::new("/"), v.name.to_lowercase()));
    out
}
