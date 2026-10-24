//! Every OS call whose behaviour differs between macOS and Linux, and the crate's only `unsafe`.
//!
//! Each function has one signature and two `#[cfg]` bodies inside it; no other module mentions a
//! target OS. The e2e suite exercises whichever body the host compiles.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("onemodel supports macOS and Linux only");

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

pub enum CloneError {
    /// The filesystem cannot clone (ENOTSUP, EOPNOTSUPP, EXDEV, EINVAL from FICLONE on ext4/tmpfs).
    Unsupported {
        errno_name: &'static str,
    },
    Io(io::Error),
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has a NUL"))
}

fn clone_error(e: io::Error) -> CloneError {
    match e.raw_os_error() {
        Some(libc::ENOTSUP) => CloneError::Unsupported { errno_name: "ENOTSUP" },
        Some(libc::EOPNOTSUPP) => CloneError::Unsupported { errno_name: "EOPNOTSUPP" },
        Some(libc::EXDEV) => CloneError::Unsupported { errno_name: "EXDEV" },
        #[cfg(target_os = "linux")]
        Some(libc::EINVAL) => CloneError::Unsupported { errno_name: "EINVAL" },
        _ => CloneError::Io(e),
    }
}

/// Create `dst` (must not exist) as a copy-on-write clone of `src`.
/// macOS: `clonefile(src, dst, CLONE_NOFOLLOW | CLONE_NOOWNERCOPY)`. Note it copies the SOURCE's
/// mode, ACL and xattrs; `copy_metadata` must run after it.
/// Linux: open src O_RDONLY, create dst O_WRONLY|O_CREAT|O_EXCL 0600, `ioctl(dst, FICLONE, src)`;
/// on failure unlink dst.
pub fn clone_file(src: &Path, dst: &Path) -> Result<(), CloneError> {
    #[cfg(target_os = "macos")]
    {
        const CLONE_NOFOLLOW: u32 = 0x0001;
        const CLONE_NOOWNERCOPY: u32 = 0x0002;
        let (s, d) = (c_path(src).map_err(CloneError::Io)?, c_path(dst).map_err(CloneError::Io)?);
        // SAFETY: `s` and `d` are NUL-terminated and outlive the call.
        let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW | CLONE_NOOWNERCOPY) };
        if rc == 0 { Ok(()) } else { Err(clone_error(io::Error::last_os_error())) }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let s = File::open(src).map_err(CloneError::Io)?;
        let d = OpenOptions::new().write(true).create_new(true).mode(0o600).open(dst).map_err(CloneError::Io)?;
        // SAFETY: both descriptors are open for the duration of the call; FICLONE takes an fd argument.
        let rc = unsafe { libc::ioctl(d.as_raw_fd(), libc::FICLONE, s.as_raw_fd()) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        drop(d);
        let _ = fs::remove_file(dst);
        Err(clone_error(err))
    }
}

/// Facts about the volume holding `path`.
pub struct VolumeInfo {
    pub dev: u64,
    /// macOS: statfs `f_mntonname`. Linux: found by walking parents until `st_dev` changes.
    pub mount: PathBuf,
    /// macOS: statfs `f_fstypename` ("apfs", "hfs"). Linux: name for the statfs `f_type` magic.
    pub fs_type: String,
    /// apfs; btrfs; xfs (reflink may still be off: the clone call decides).
    pub clones: bool,
    /// What an unprivileged writer can actually use.
    pub free: u64,
}

pub fn volume(path: &Path) -> io::Result<VolumeInfo> {
    let dev = fs::symlink_metadata(path)?.dev();
    #[cfg(target_os = "macos")]
    {
        let st = statfs(path)?;
        // SAFETY: the kernel NUL-terminates both fixed-size name arrays.
        let (fs_type, mount) = unsafe {
            (
                std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()).to_string_lossy().into_owned(),
                PathBuf::from(std::ffi::OsStr::from_bytes(
                    std::ffi::CStr::from_ptr(st.f_mntonname.as_ptr()).to_bytes(),
                )),
            )
        };
        let clones = fs_type == "apfs";
        Ok(VolumeInfo { dev, mount, fs_type, clones, free: st.f_bavail * u64::from(st.f_bsize) })
    }
    #[cfg(target_os = "linux")]
    {
        let st = statfs(path)?;
        let (fs_type, clones) = match st.f_type as u64 & 0xffff_ffff {
            0x9123_683e => ("btrfs".to_string(), true),
            0x5846_5342 => ("xfs".to_string(), true),
            0xef53 => ("ext4".to_string(), false),
            0x0102_1994 => ("tmpfs".to_string(), false),
            0x2fc1_2fc1 => ("zfs".to_string(), false),
            other => (format!("0x{other:x}"), false),
        };
        let mut mount = fs::canonicalize(path)?;
        while let Some(parent) = mount.parent() {
            if fs::metadata(parent)?.dev() != dev {
                break;
            }
            mount = parent.to_path_buf();
        }
        Ok(VolumeInfo { dev, mount, fs_type, clones, free: free_bytes(path)? })
    }
}

fn statfs(path: &Path) -> io::Result<libc::statfs> {
    let c = c_path(path)?;
    let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `c` is NUL-terminated; `st` is a valid out-pointer that statfs fills on success.
    let rc = unsafe { libc::statfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs returned 0, so it initialized `st`.
    Ok(unsafe { st.assume_init() })
}

/// Free bytes for an unprivileged writer. The only source of the "freed" number onemodel reports.
/// macOS: statfs `f_bavail * f_bsize` (statvfs counts are 32-bit there). Linux: statvfs
/// `f_bavail * f_frsize`.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    #[cfg(target_os = "macos")]
    {
        let st = statfs(path)?;
        Ok(st.f_bavail * u64::from(st.f_bsize))
    }
    #[cfg(target_os = "linux")]
    {
        let c = c_path(path)?;
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `c` is NUL-terminated; `st` is a valid out-pointer that statvfs fills on success.
        let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: statvfs returned 0, so it initialized `st`.
        let st = unsafe { st.assume_init() };
        Ok(st.f_bavail as u64 * st.f_frsize as u64)
    }
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
