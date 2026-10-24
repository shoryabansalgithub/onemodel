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
    Unsupported,
    Io(io::Error),
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has a NUL"))
}

fn clone_error(e: io::Error) -> CloneError {
    match e.raw_os_error() {
        Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::EXDEV) => CloneError::Unsupported,
        #[cfg(target_os = "linux")]
        Some(libc::EINVAL) => CloneError::Unsupported,
        _ => CloneError::Io(e),
    }
}

/// macOS: `clonefile(src, dst, CLONE_NOFOLLOW | CLONE_NOOWNERCOPY)`. Note it copies the SOURCE's
/// mode, ACL and xattrs; `copy_metadata` must run after it.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Volume {
    pub dev: u64,
    pub mount: PathBuf,
    pub fs_type: String,
    pub clones: bool,
    #[serde(rename = "free_before")]
    pub free: u64,
}

pub fn volume(path: &Path) -> io::Result<Volume> {
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
        Ok(Volume { dev, mount, fs_type, clones, free: free_bytes(path)? })
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
        Ok(Volume { dev, mount, fs_type, clones, free: free_bytes(path)? })
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

/// xattrs the kernel owns. macOS attaches `com.apple.provenance` to every file a process creates;
/// removing or setting it silently has no effect, so it is neither copied nor compared.
pub const KERNEL_XATTRS: &[&str] = &["com.apple.provenance"];

pub fn copy_metadata(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    // xattr writes need write access, and a clone starts with the keeper's mode (HF blobs are 0444).
    fs::set_permissions(to, fs::Permissions::from_mode(0o600))?;
    let wanted = xattrs(from)?;
    for (name, value) in xattrs(to)? {
        if wanted.get(&name) != Some(&value) {
            xattr::remove(to, &name)?;
        }
    }
    for (name, value) in &wanted {
        if let Err(e) = xattr::set(to, name, value)
            && xattr::get(to, name)?.as_ref() != Some(value)
        {
            return Err(e);
        }
    }
    fs::set_permissions(to, fs::Permissions::from_mode(meta.mode() & 0o7777))?;
    set_times(to, &meta)
}

pub fn set_mode_and_mtime(to: &Path, mode: u32, mtime_ns: i64) -> io::Result<()> {
    fs::set_permissions(to, fs::Permissions::from_mode(mode & 0o7777))?;
    let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(mtime_ns.max(0) as u64);
    File::open(to)?.set_times(FileTimes::new().set_modified(mtime))
}

fn set_times(to: &Path, meta: &fs::Metadata) -> io::Result<()> {
    File::open(to)?.set_times(FileTimes::new().set_accessed(meta.accessed()?).set_modified(meta.modified()?))
}

pub fn xattrs(path: &Path) -> io::Result<BTreeMap<std::ffi::OsString, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for name in xattr::list(path)? {
        if KERNEL_XATTRS.iter().any(|k| name == *k) {
            continue;
        }
        if let Some(value) = xattr::get(path, &name)? {
            out.insert(name, value);
        }
    }
    Ok(out)
}

/// Never `std::fs::copy`: on macOS it uses fclonefileat and would produce a clone.
pub fn real_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let mut input = File::open(src)?;
    let mut output = OpenOptions::new().write(true).create_new(true).mode(0o600).open(dst)?;
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        output.write_all(&buf[..n])?;
    }
}

/// fsync a file, or a directory so a rename or create inside it is durable (F_FULLFSYNC on macOS,
/// via std).
pub fn fsync(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

pub fn has_acl(path: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::{c_char, c_int, c_void};
        const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
        const ACL_FIRST_ENTRY: c_int = 0;
        unsafe extern "C" {
            fn acl_get_link_np(path: *const c_char, ty: c_int) -> *mut c_void;
            fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
            fn acl_free(obj: *mut c_void) -> c_int;
        }
        let c = c_path(path)?;
        // SAFETY: `c` is NUL-terminated and outlives the call.
        let acl = unsafe { acl_get_link_np(c.as_ptr(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(libc::ENOENT) && fs::symlink_metadata(path).is_ok() {
                Ok(false)
            } else {
                Err(e)
            };
        }
        let mut entry = std::ptr::null_mut();
        // SAFETY: `acl` is a live ACL returned above; `entry` is a valid out-pointer.
        let has_entry = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) } == 0;
        // SAFETY: `acl` came from acl_get_link_np and is freed exactly once.
        unsafe { acl_free(acl) };
        Ok(has_entry)
    }
    #[cfg(target_os = "linux")]
    {
        fs::symlink_metadata(path).map(|_| false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub command: String,
}

pub fn open_holders(lsof: &Path, paths: &[&Path]) -> Result<BTreeMap<PathBuf, Vec<Holder>>, String> {
    // Matched by inode, because lsof prints kernel paths (`/private/var/...`) that need not match ours.
    let mut by_ino: BTreeMap<u64, Vec<&Path>> = BTreeMap::new();
    for p in paths {
        let meta = fs::symlink_metadata(p).map_err(|e| format!("cannot stat {}: {e}", p.display()))?;
        by_ino.entry(meta.ino()).or_default().push(p);
    }
    let mut found: BTreeMap<PathBuf, Vec<Holder>> = BTreeMap::new();
    if paths.is_empty() {
        return Ok(found);
    }
    let out = Command::new(lsof)
        .args(["-w", "-n", "-P", "-F", "pcin", "--"])
        .args(paths)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", lsof.display()))?;
    // lsof exits 1 whenever any named file is not open.
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(format!("{} failed ({})", lsof.display(), out.status));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if let Some(line) = stderr.lines().find(|l| !l.trim().is_empty()) {
        return Err(format!("{} reported: {}", lsof.display(), line.trim()));
    }
    let (mut pid, mut command, mut ino) = (None, String::new(), None);
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let (tag, value) = (line.get(..1).unwrap_or(""), line.get(1..).unwrap_or(""));
        match tag {
            "p" => {
                pid = Some(value.parse::<u32>().map_err(|_| format!("unparseable lsof line {line:?}"))?);
                command.clear();
                ino = None;
            }
            "c" => command = value.to_string(),
            "f" => ino = None,
            "i" => ino = Some(value.parse::<u64>().map_err(|_| format!("unparseable lsof line {line:?}"))?),
            "n" => {
                let (Some(pid), Some(ino)) = (pid, ino) else { continue };
                for p in by_ino.get(&ino).into_iter().flatten() {
                    let holders = found.entry(p.to_path_buf()).or_default();
                    if !holders.iter().any(|h| h.pid == pid) {
                        holders.push(Holder { pid, command: command.clone() });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

pub fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is open for the duration of the call.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) { Ok(false) } else { Err(e) }
}

pub fn default_state_dir(home: &Path, xdg_state_home: Option<&str>) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let _ = xdg_state_home;
        home.join("Library/Application Support/onemodel")
    }
    #[cfg(target_os = "linux")]
    {
        match xdg_state_home {
            Some(dir) if Path::new(dir).is_absolute() => Path::new(dir).join("onemodel"),
            _ => home.join(".local/state/onemodel"),
        }
    }
}

pub fn user_cache_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Caches")
    }
    #[cfg(target_os = "linux")]
    {
        home.join(".cache")
    }
}

pub fn default_lsof() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/usr/sbin/lsof")
    }
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("lsof")
    }
}

pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

pub fn sigkill_self() -> ! {
    // SAFETY: kill and getpid have no memory-safety preconditions.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    unreachable!("SIGKILL cannot be caught")
}
