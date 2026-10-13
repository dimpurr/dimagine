//! Symlink-free filesystem access under an already-open library root.
//!
//! Two strategies, per platform (ADR-018 §4):
//!
//! - Linux: `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`, one call
//!   for the whole chain, when the kernel supports it.
//! - Everywhere (and the Linux fallback): a component-by-component walk where
//!   every step is an `openat` with `O_NOFOLLOW`, so a symlink at any depth —
//!   directory or leaf — is an error rather than a redirect.
//!
//! After a directory is resolved its descriptor is the only handle used: a
//! later swap of an ancestor into a symlink cannot redirect writes, because
//! the descriptor still names the original inode.

use std::ffi::{CString, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;

use crate::error::{Error, PathReject};

/// Prefix of every temporary file this crate creates. Dot-prefixed, so an
/// ordinary directory listing never shows it, and recognisable, so the
/// startup sweep can remove leftovers (ADR-016 §4).
pub const TMP_PREFIX: &str = ".dimagine.tmp.";

pub(crate) const O_FLAGS_NOFOLLOW: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// The `.`-prefixed name of a temporary file for `new_bytes` in a directory.
pub(crate) fn temp_name(nonce: &str) -> String {
    format!("{TMP_PREFIX}{nonce}")
}

pub(crate) fn cname(name: &str) -> Result<CString, Error> {
    CString::new(name).map_err(|_| Error::BadPath {
        path: name.to_string(),
        reject: PathReject::Traversal,
    })
}

fn errno() -> io::Error {
    io::Error::last_os_error()
}

/// Is this errno "the path hit a symlink"? macOS is documented to answer
/// `ELOOP`, some platforms answer `ENOTSUP` for `O_NOFOLLOW`.
fn is_symlink_error(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::ENOTSUP) | Some(libc::EMLINK) | Some(97) /* ENOTSUP on older macOS */
    )
}

fn symlink_err(path: &str) -> Error {
    Error::BadPath {
        path: path.to_string(),
        reject: PathReject::Symlink,
    }
}

pub(crate) fn io_failed(context: &str, path: impl Into<PathBuf>, e: io::Error) -> Error {
    crate::error::io_err(format!("{context}: {e}"), path, e)
}

/// Open the library root. The returned descriptor is the base every later
/// resolution starts from; the root itself may legitimately be a symlink,
/// because the user chose it.
pub(crate) fn open_root(path: &std::path::Path) -> Result<File, Error> {
    let c = cname(
        path.to_str()
            .ok_or_else(|| Error::BadRoot(path.to_string_lossy().into_owned()))?,
    )?;
    let raw = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        let e = errno();
        return Err(io_failed("open library root", path, e));
    }
    Ok(unsafe { File::from_raw_fd(raw) })
}

/// Resolve a chain of directory components under `root`, optionally creating
/// what is missing. Refuses every symlink on the way.
pub(crate) fn resolve_dir(root: &File, components: &[String], create: bool) -> Result<File, Error> {
    if components.is_empty() {
        return Ok(dup_dir(root));
    }
    if !create {
        let joined = components.join("/");
        if let Some(result) = openat2_beneath(root, &joined, O_FLAGS_NOFOLLOW, 0) {
            return result;
        }
    }
    let mut dir = dup_dir(root);
    for comp in components {
        dir = match open_dir_child(&dir, comp) {
            Ok(f) => f,
            Err(e) if create && missing(&e) => {
                mkdir_child(&dir, comp)?;
                open_dir_child(&dir, comp)?
            }
            Err(e) => return Err(e),
        };
    }
    Ok(dir)
}

fn open_dir_child(dir: &File, comp: &str) -> Result<File, Error> {
    let c = cname(comp)?;
    let raw = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), O_FLAGS_NOFOLLOW) };
    if raw < 0 {
        let e = errno();
        if is_symlink_error(&e) {
            return Err(symlink_err(comp));
        }
        // A component that exists but is not a directory, or a file where a
        // directory is expected: check whether it is a symlink (macOS openat
        // with O_DIRECTORY|O_NOFOLLOW returns ENOTDIR for symlinks).
        if e.raw_os_error() == Some(libc::ENOTDIR) {
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::fstatat(
                    dir.as_raw_fd(),
                    c.as_ptr(),
                    &mut st,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            if rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
                return Err(symlink_err(comp));
            }
            return Err(Error::BadPath {
                path: comp.to_string(),
                reject: PathReject::Traversal,
            });
        }
        return Err(io_failed("open directory component", comp, e));
    }
    Ok(unsafe { File::from_raw_fd(raw) })
}

fn mkdir_child(dir: &File, comp: &str) -> Result<(), Error> {
    let c = cname(comp)?;
    let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o755) };
    if rc < 0 {
        let e = errno();
        // A concurrent creator is not a failure: the open that follows wins.
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(io_failed("create directory component", comp, e));
    }
    Ok(())
}

/// Open `name` inside an already-resolved directory. `O_NOFOLLOW` makes a
/// symlinked leaf an error rather than a redirect.
pub(crate) fn open_leaf(
    dir: &File,
    name: &str,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Result<File, Error> {
    let c = cname(name)?;
    let raw = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            flags | libc::O_NOFOLLOW,
            mode as libc::c_uint,
        )
    };
    if raw < 0 {
        let e = errno();
        if is_symlink_error(&e) {
            return Err(symlink_err(name));
        }
        return Err(io_failed("open file", name, e));
    }
    let file = unsafe { File::from_raw_fd(raw) };
    Ok(file)
}

/// `true` when the error means "not there yet".
fn missing(e: &Error) -> bool {
    matches!(
        e,
        Error::Io { source, .. } if source.raw_os_error() == Some(libc::ENOENT)
    )
}

/// Metadata of an open descriptor: what the fingerprint is taken from, so the
/// identity checked before a rename is the identity that was read.
pub(crate) fn fd_metadata(file: &File) -> Result<std::fs::Metadata, Error> {
    file.metadata().map_err(|e| io_failed("stat", "<fd>", e))
}

/// Read a leaf's bytes, refusing a symlinked leaf and a non-regular file.
pub(crate) fn read_leaf(dir: &File, name: &str) -> Result<(Vec<u8>, std::fs::Metadata), Error> {
    let file = open_leaf(dir, name, libc::O_RDONLY, 0)?;
    let meta = fd_metadata(&file)?;
    if !meta.is_file() {
        return Err(Error::BadPath {
            path: name.to_string(),
            reject: PathReject::Traversal,
        });
    }
    use std::io::Read;
    let mut buf = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0).min(1 << 20));
    let mut file = file;
    file.read_to_end(&mut buf)
        .map_err(|e| io_failed("read", name, e))?;
    Ok((buf, meta))
}

/// Does a leaf exist at all? Used by `create_exclusive`'s callers only to
/// report a conflict; creation itself never depends on it.
pub(crate) fn leaf_exists(dir: &File, name: &str) -> bool {
    let Ok(c) = cname(name) else {
        return false;
    };
    unsafe {
        libc::faccessat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::F_OK,
            libc::AT_SYMLINK_NOFOLLOW,
        ) == 0
    }
}

pub(crate) fn fsync_file(file: &File) -> Result<(), Error> {
    file.sync_data()
        .map_err(|e| io_failed("fsync file", "<fd>", e))
}

/// Set a file's mode bits exactly (the durable update copies the original
/// file's mode onto its temp file; `open`'s mode argument is masked by the
/// process umask, so an explicit `fchmod` is the only exact way).
pub(crate) fn fchmod(file: &File, mode: u32) -> Result<(), Error> {
    let rc = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
    if rc < 0 {
        return Err(io_failed("fchmod", "<fd>", errno()));
    }
    Ok(())
}

/// fsync a directory so a rename is durable.
pub(crate) fn fsync_dir(dir: &File) -> Result<(), Error> {
    let rc = unsafe { libc::fsync(dir.as_raw_fd()) };
    if rc < 0 {
        let e = errno();
        // macOS answers EINVAL or unsupported for directory fds; there the
        // sync is best-effort (ADR-016 §4).
        if cfg!(target_os = "macos") {
            return Ok(());
        }
        return Err(io_failed("fsync directory", "<dir>", e));
    }
    Ok(())
}

/// The outcome of a "rename only if the target is absent" attempt.
pub(crate) enum Renamed {
    Done,
    /// The target already existed.
    #[allow(dead_code)]
    Occupied,
    /// The kernel or filesystem has no such primitive: the caller must fall
    /// back to an exclusive create.
    Unsupported,
}

/// Rename `from` to `to` inside `dir`, failing if `to` exists.
pub(crate) fn rename_noreplace(dir: &File, from: &str, to: &str) -> Result<Renamed, Error> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        const RENAME_NOREPLACE: libc::c_uint = 1;
        let (cf, ct) = (cname(from)?, cname(to)?);
        let rc = unsafe {
            libc::renameat2(
                dir.as_raw_fd(),
                cf.as_ptr(),
                dir.as_raw_fd(),
                ct.as_ptr(),
                RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(Renamed::Done);
        }
        let e = errno();
        return Err(classify_rename(e, to));
    }
    #[cfg(target_os = "macos")]
    {
        // `renamex_np` takes paths, not descriptors: build them from the
        // directory's own resolved path, so it cannot be re-pointed by name.
        let Some(base) = fd_path(dir) else {
            return Ok(Renamed::Unsupported);
        };
        let from_path = base.join(from);
        let to_path = base.join(to);
        let (cf, ct) = (cname_abs(&from_path)?, cname_abs(&to_path)?);
        let rc = unsafe { libc::renamex_np(cf.as_ptr(), ct.as_ptr(), libc::RENAME_EXCL) };
        if rc == 0 {
            return Ok(Renamed::Done);
        }
        let e = errno();
        return Err(classify_rename(e, to));
    }
    #[allow(unreachable_code)]
    Ok(Renamed::Unsupported)
}

fn classify_rename(e: io::Error, to: &str) -> Error {
    if e.raw_os_error() == Some(libc::EEXIST) {
        return Error::Exists(crate::error::Exists {
            path: PathBuf::from(to),
        });
    }
    io_failed("rename", to, e)
}

fn cname_abs(path: &std::path::Path) -> Result<CString, Error> {
    let s = path.to_str().ok_or_else(|| Error::BadPath {
        path: path.to_string_lossy().into_owned(),
        reject: PathReject::NonUtf8,
    })?;
    cname(s)
}

/// Rename `from` to `to`, replacing an existing target (used only by the
/// durable update of a note, where the precondition has already been checked).
pub(crate) fn rename_replace(dir: &File, from: &str, to: &str) -> Result<(), Error> {
    let (cf, ct) = (cname(from)?, cname(to)?);
    let rc = unsafe { libc::renameat(dir.as_raw_fd(), cf.as_ptr(), dir.as_raw_fd(), ct.as_ptr()) };
    if rc < 0 {
        let e = errno();
        return Err(io_failed("rename", to, e));
    }
    Ok(())
}

/// Remove a name inside a resolved directory.
pub(crate) fn unlink(dir: &File, name: &str) -> Result<(), Error> {
    let c = cname(name)?;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) };
    if rc < 0 {
        let e = errno();
        if e.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(io_failed("remove", name, e));
    }
    Ok(())
}

/// Remove an empty directory inside a resolved directory (the journal's
/// op directory, after every step file and the committed marker are gone).
pub(crate) fn remove_dir_at(dir: &File, name: &str) -> Result<(), Error> {
    let c = cname(name)?;
    let rc = unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), libc::AT_REMOVEDIR) };
    if rc < 0 {
        let e = errno();
        if e.raw_os_error() == Some(libc::ENOENT) {
            return Ok(());
        }
        return Err(io_failed("remove directory", name, e));
    }
    Ok(())
}

/// Rename across directories, refusing to clobber `to`. The two sides are
/// already resolved descriptors.
pub(crate) fn rename_between(
    from_dir: &File,
    from_name: &str,
    to_dir: &File,
    to_name: &str,
    noreplace: bool,
) -> Result<Renamed, Error> {
    if noreplace {
        let same = from_dir.as_raw_fd() == to_dir.as_raw_fd();
        if same {
            return rename_noreplace(from_dir, from_name, to_name);
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            const RENAME_NOREPLACE: libc::c_uint = 1;
            let (cf, ct) = (cname(from_name)?, cname(to_name)?);
            let rc = unsafe {
                libc::renameat2(
                    from_dir.as_raw_fd(),
                    cf.as_ptr(),
                    to_dir.as_raw_fd(),
                    ct.as_ptr(),
                    RENAME_NOREPLACE,
                )
            };
            if rc == 0 {
                return Ok(Renamed::Done);
            }
            let e = errno();
            return Err(classify_rename(e, to_name));
        }
        #[cfg(target_os = "macos")]
        {
            let (Some(bf), Some(bt)) = (fd_path(from_dir), fd_path(to_dir)) else {
                return Ok(Renamed::Unsupported);
            };
            let (cf, ct) = (
                cname_abs(&bf.join(from_name))?,
                cname_abs(&bt.join(to_name))?,
            );
            let rc = unsafe { libc::renamex_np(cf.as_ptr(), ct.as_ptr(), libc::RENAME_EXCL) };
            if rc == 0 {
                return Ok(Renamed::Done);
            }
            let e = errno();
            return Err(classify_rename(e, to_name));
        }
        #[allow(unreachable_code)]
        Ok(Renamed::Unsupported)
    } else {
        rename_between_replace(from_dir, from_name, to_dir, to_name).map(|()| Renamed::Done)
    }
}

fn rename_between_replace(
    from_dir: &File,
    from_name: &str,
    to_dir: &File,
    to_name: &str,
) -> Result<(), Error> {
    let (cf, ct) = (cname(from_name)?, cname(to_name)?);
    let rc = unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            cf.as_ptr(),
            to_dir.as_raw_fd(),
            ct.as_ptr(),
        )
    };
    if rc < 0 {
        let e = errno();
        return Err(io_failed("move", to_name, e));
    }
    Ok(())
}

/// File names in a resolved directory, without following anything.
///
/// Always reads from the start: the descriptor is `dup`ed, and a `dup`
/// shares its offset with the original, so without a rewind the second
/// listing of the same directory would come back empty.
pub(crate) fn list_names(dir: &File) -> Result<Vec<String>, Error> {
    let dup = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup < 0 {
        return Err(io_failed("dup", "<dir>", errno()));
    }
    let dp = unsafe { libc::fdopendir(dup) };
    if dp.is_null() {
        let e = errno();
        unsafe {
            libc::close(dup);
        }
        return Err(io_failed("fdopendir", "<dir>", e));
    }
    unsafe {
        libc::rewinddir(dp);
    }
    let mut out = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(dp) };
        if entry.is_null() {
            break;
        }
        let name = unsafe {
            let ptr = std::ptr::addr_of!((*entry).d_name) as *const libc::c_char;
            std::ffi::CStr::from_ptr(ptr)
        };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if let Ok(s) = OsString::from_vec(bytes.to_vec()).into_string() {
            out.push(s);
        }
    }
    unsafe {
        libc::closedir(dp);
    }
    Ok(out)
}

/// The path an open descriptor currently refers to, for the separator-boundary
/// root check. `None` when the platform cannot say.
pub(crate) fn fd_path(file: &File) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
        let rc = unsafe {
            libc::fcntl(
                file.as_raw_fd(),
                libc::F_GETPATH,
                buf.as_mut_ptr() as *mut libc::c_char,
            )
        };
        if rc < 0 {
            return None;
        }
        let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let s = String::from_utf8_lossy(&buf[..len]).into_owned();
        return Some(PathBuf::from(s));
    }
    #[cfg(target_os = "linux")]
    {
        let link = format!("/proc/self/fd/{}", file.as_raw_fd());
        let p = std::fs::read_link(link).ok()?;
        return Some(p);
    }
    #[allow(unreachable_code)]
    None
}

/// Does the descriptor sit at or under `root`, cut at a separator boundary?
/// `library-other` must not pass for `library/` (ADR-018 §4).
pub(crate) fn under_root(
    file: &File,
    root: &std::path::Path,
    path_text: &str,
) -> Result<(), Error> {
    let Some(found) = fd_path(file) else {
        return Ok(());
    };
    let base = found.to_string_lossy().into_owned();
    let root_text = root.to_string_lossy().into_owned();
    let inside = base == root_text
        || (base.starts_with(&root_text)
            && (root_text.ends_with('/') || base.as_bytes().get(root_text.len()) == Some(&b'/')));
    if inside {
        return Ok(());
    }
    Err(Error::BadPath {
        path: format!("{path_text} -> {base}"),
        reject: PathReject::OutsideRoot,
    })
}

/// Linux fast path: `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`.
/// `None` means "unsupported here", and the caller falls back to the walk.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn openat2_beneath(
    dir: &File,
    path: &str,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> Option<Result<File, Error>> {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0 = unprobed, 1 = works, 2 = unsupported.
    static STATE: AtomicU8 = AtomicU8::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 2 {
        return None;
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_BENEATH: u64 = 0x01;
    const RESOLVE_NO_SYMLINKS: u64 = 0x02;

    let c = match cname(path) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    let how = OpenHow {
        flags: u64::try_from(flags).unwrap_or(0),
        mode: u64::from(mode),
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    let raw = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd(),
            c.as_ptr(),
            std::ptr::addr_of!(how),
            std::mem::size_of::<OpenHow>(),
        )
    };
    if raw >= 0 {
        if state == 0 {
            state = 1;
            STATE.store(1, Ordering::Relaxed);
        }
        return Some(Ok(unsafe { File::from_raw_fd(raw as i32) }));
    }
    let e = errno();
    match e.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(95) /* EOPNOTSUPP */ => {
            STATE.store(2, Ordering::Relaxed);
            None
        }
        Some(libc::ELOOP) => Some(Err(symlink_err(path))),
        _ => {
            if state == 0 {
                // A genuine error (ENOENT, ENOTDIR) still proves openat2 ran.
                STATE.store(1, Ordering::Relaxed);
            }
            Some(Err(match e.raw_os_error() {
                Some(libc::ENOTDIR) => Error::BadPath {
                    path: path.to_string(),
                    reject: PathReject::Traversal,
                },
                _ => io_failed("open", path, e),
            }))
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(crate) fn openat2_beneath(
    _dir: &File,
    _path: &str,
    _flags: libc::c_int,
    _mode: libc::mode_t,
) -> Option<Result<File, Error>> {
    None
}

pub(crate) fn dup_dir(root: &File) -> File {
    let raw = unsafe { libc::dup(root.as_raw_fd()) };
    if raw < 0 {
        // `dup` of a live descriptor only fails out of file descriptor
        // exhaustion; the caller's next open will fail visibly.
        panic!("dup of the library root failed: {}", errno());
    }
    unsafe { File::from_raw_fd(raw) }
}
