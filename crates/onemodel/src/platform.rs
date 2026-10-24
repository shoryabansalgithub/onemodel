//! Every OS call whose behaviour differs between macOS and Linux, and the crate's only `unsafe`.
//!
//! Each function has one signature and two `#[cfg]` bodies inside it; no other module mentions a
//! target OS. The e2e suite exercises whichever body the host compiles; CI runs it on macOS (APFS,
//! plus hdiutil-made HFS+ and second-APFS volumes) and Linux (tmpfs at /dev/shm as the
//! no-clone / other-volume case; btrfs only when `ONEMODEL_TEST_BTRFS` names a mounted dir).

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub enum CloneError {
    /// The filesystem cannot clone (ENOTSUP, EOPNOTSUPP, EXDEV, EINVAL from FICLONE on ext4/tmpfs).
    Unsupported {
        errno_name: &'static str,
    },
    Io(io::Error),
}

/// Create `dst` (must not exist) as a copy-on-write clone of `src`.
/// macOS: `clonefile(src, dst, CLONE_NOFOLLOW | CLONE_NOOWNERCOPY)`. Note it copies the SOURCE's
/// mode, ACL and xattrs; `copy_metadata` must run after it.
/// Linux: open src O_RDONLY, create dst O_WRONLY|O_CREAT|O_EXCL 0600, `ioctl(dst, FICLONE, src)`;
/// on failure unlink dst.
pub fn clone_file(src: &Path, dst: &Path) -> Result<(), CloneError> {
    let _ = (src, dst);
    todo!()
}

/// Facts about the volume holding `path`.
pub struct VolumeInfo {
    pub dev: u64,
    /// Found portably by walking parents until `st_dev` changes.
    pub mount: PathBuf,
    /// macOS: statfs `f_fstypename` ("apfs", "hfs"). Linux: name for the statfs `f_type` magic.
    pub fs_type: String,
    /// apfs; btrfs (0x9123683e); xfs (0x58465342, reflink may still be off: the clone call decides).
    pub clones: bool,
    /// statvfs `f_bavail * f_frsize`: what an unprivileged writer can actually use.
    pub free: u64,
}

pub fn volume(path: &Path) -> io::Result<VolumeInfo> {
    let _ = path;
    todo!()
}

/// statvfs `f_bavail * f_frsize`. The only source of the "freed" number onemodel reports.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let _ = path;
    todo!()
}

/// Make `to` carry `from`'s metadata: remove every xattr on `to`, set every xattr of `from`
/// (nofollow), then mode, then atime/mtime last (xattr writes do not touch mtime, but ordering it
/// last makes that irrelevant). Ownership is not copied (plan refuses foreign-owned targets).
/// Linux: an xattr outside the `user.` namespace that cannot be set is tolerated only if `to`
/// already carries the same value (e.g. `security.selinux` inherited from the directory); otherwise
/// the error propagates and the op is refused. Never silently drop metadata.
pub fn copy_metadata(from: &Path, to: &Path) -> io::Result<()> {
    let _ = (from, to);
    todo!()
}

/// A real, unshared byte copy: create `dst` O_EXCL, read/write loop with an 8 MiB buffer.
/// Never `std::fs::copy`: on macOS it uses fclonefileat and would produce a clone.
pub fn real_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let _ = (src, dst);
    todo!()
}

pub fn fsync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// fsync a directory so a rename or create inside it is durable.
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub command: String,
}

/// Which of `paths` are open or mmapped by any process we can see.
///
/// One `lsof -w -n -P -F pcn -- <paths...>` invocation (fds and memory maps both report as `n`).
/// lsof exits 1 both for "none of these are open" and for errors, so: exit 0 -> parse; exit 1 with
/// empty stdout and empty stderr -> nothing open; anything else (missing binary, signal, stderr
/// output, unparseable field) -> `Err(detail)`, which every caller turns into a refusal. Fail closed.
pub fn open_holders(lsof: &Path, paths: &[&Path]) -> Result<BTreeMap<PathBuf, Vec<Holder>>, String> {
    let _ = (lsof, paths);
    todo!()
}

/// flock(LOCK_EX | LOCK_NB). `Ok(false)` = someone else holds it.
pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    let _ = file;
    todo!()
}

pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// `kill(getpid(), SIGKILL)`. Used only by `journal::crash_point`.
pub fn sigkill_self() -> ! {
    // SAFETY: kill and getpid have no memory-safety preconditions.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    unreachable!("SIGKILL cannot be caught")
}
