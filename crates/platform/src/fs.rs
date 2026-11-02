//! File safety: atomic writes (temp file, fsync, rename), optionally private (0600 on Unix),
//! confined paths (no symlinks or reparse points), and per-record write locks.

use craft_anything_evidence::ToolError;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `path` atomically, keeping the existing file's permissions when it exists.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    write_impl(path, bytes, false)
}

/// Write `bytes` to `path` atomically with owner-only permissions (0600 on Unix). Used for
/// configuration files that may hold paths or tokens, and for the key fallback file.
pub fn write_atomic_private(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    write_impl(path, bytes, true)
}

fn write_impl(path: &Path, bytes: &[u8], private: bool) -> Result<(), ToolError> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        Some(_) => PathBuf::from("."),
        None => return Err(ToolError::invalid(format!("{} has no parent folder", path.display()))),
    };
    std::fs::create_dir_all(&dir).map_err(|e| ToolError::io(dir.display(), e))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let existing_perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    let (tmp, mut f) = create_temp(&dir, &name)?;
    let result = (|| {
        if private {
            set_private(&tmp)?;
        } else if let Some(p) = existing_perms {
            std::fs::set_permissions(&tmp, p).map_err(|e| ToolError::io(tmp.display(), e))?;
        }
        f.write_all(bytes).map_err(|e| ToolError::io(tmp.display(), e))?;
        f.sync_all().map_err(|e| ToolError::io(tmp.display(), e))?;
        drop(f);
        rename_with_retry(&tmp, path).map_err(|e| ToolError::io(path.display(), e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A sibling temp file that no other writer can collide with: `create_new` plus a process-wide
/// counter, retried if a stale temp file from a crashed run is still there.
fn create_temp(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File), ToolError> {
    for _ in 0..1024 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(ToolError::io(tmp.display(), e)),
        }
    }
    Err(ToolError::io(dir.display(), std::io::Error::new(std::io::ErrorKind::AlreadyExists, "too many stale temporary files")))
}

/// On Windows a file another program has open for a moment (an antivirus scan, the search
/// indexer) can't be replaced: try again briefly before giving up.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| ToolError::io(path.display(), e))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<(), ToolError> {
    // Windows: files under the user's profile are already private to the user by default ACLs.
    Ok(())
}

/// Copy `from` to `to` only if `to` does not exist yet (the first backup is never overwritten).
/// Returns true when a copy was made, false when a backup already existed. The backup appears
/// atomically: a private temp file is written and fsynced first, then hard-linked into place, so a
/// crash never leaves a half-written backup and two callers can't both think they made it.
pub fn backup_once(from: &Path, to: &Path) -> Result<bool, ToolError> {
    if to.exists() {
        return Ok(false);
    }
    let bytes = std::fs::read(from).map_err(|e| ToolError::io(from.display(), e))?;
    let dir = to.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "backup".into());
    let (tmp, mut f) = create_temp(&dir, &name)?;
    let result = (|| {
        set_private(&tmp)?;
        f.write_all(&bytes).map_err(|e| ToolError::io(tmp.display(), e))?;
        f.sync_all().map_err(|e| ToolError::io(tmp.display(), e))?;
        drop(f);
        match std::fs::hard_link(&tmp, to) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(ToolError::io(to.display(), e)),
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Whether a metadata record is a symlink or a Windows reparse point (junctions included).
pub fn metadata_is_link(m: &std::fs::Metadata) -> bool {
    if m.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        // FILE_ATTRIBUTE_REPARSE_POINT
        if m.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

/// Whether `path` itself is a symlink or reparse point (does not follow it).
pub fn path_is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|m| metadata_is_link(&m)).unwrap_or(false)
}

/// The canonical path when it exists, else the lexical one. On Windows the `\\?\` verbatim prefix
/// is removed so the result compares equal to ordinary paths in policy checks.
pub fn real_path(path: &Path) -> PathBuf {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(windows)]
    {
        let text = real.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    real
}

/// Resolve `rel` under `root`, refusing to follow symlinks or Windows reparse points. `root` is
/// canonicalized first; a missing tail is allowed only under components that were checked. This is
/// a best-effort check-then-use guard: callers that create directories should re-check afterwards.
pub fn confine(root: &Path, rel: &Path) -> Result<PathBuf, ToolError> {
    let real_root = std::fs::canonicalize(root).map_err(|e| ToolError::io(root.display(), e))?;
    let mut cur = real_root;
    for c in rel.components() {
        match c {
            Component::Normal(n) => {
                cur.push(n);
                match std::fs::symlink_metadata(&cur) {
                    Ok(m) if metadata_is_link(&m) => {
                        return Err(ToolError::invalid(format!("{} is a link; Craft Anything doesn't follow links inside a project.", cur.display())));
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(ToolError::io(cur.display(), e)),
                }
            }
            Component::CurDir => {}
            _ => return Err(ToolError::invalid(format!("{} escapes the project folder.", rel.display()))),
        }
    }
    // Return the caller's spelling (the checked path is the same file) so relative display stays
    // relative even when the root itself is reached through a symlink or a macOS /var alias.
    Ok(root.join(rel))
}

/// Serialize read-modify-write cycles for one record (a manifest, a settings file) across threads.
/// The lock is keyed by the record's path; a deadlock is broken by `platform::sync::Mutex`.
pub fn with_record_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    static LOCKS: crate::sync::Mutex<BTreeMap<PathBuf, Arc<crate::sync::Mutex<()>>>> = crate::sync::Mutex::new(BTreeMap::new());
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let lock = {
        let mut locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
        locks.entry(key).or_insert_with(|| Arc::new(crate::sync::Mutex::new(()))).clone()
    };
    let _guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_and_private_is_0600() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("a.txt");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let q = dir.path().join("secret");
        write_atomic_private(&q, b"k").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // no temp files left behind
        assert_eq!(std::fs::read_dir(dir.path().join("sub")).unwrap().count(), 1);
    }

    #[test]
    fn backup_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("a.bak");
        std::fs::write(&a, "first").unwrap();
        assert!(backup_once(&a, &b).unwrap());
        std::fs::write(&a, "second").unwrap();
        assert!(!backup_once(&a, &b).unwrap());
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "first");
    }

    #[test]
    fn confine_refuses_links_and_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("app")).unwrap();
        assert!(confine(&root, Path::new("app/new/file.txt")).is_ok());
        assert!(confine(&root, Path::new("../outside")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("outside"), root.join("app/link")).unwrap();
            assert!(confine(&root, Path::new("app/link/file.txt")).is_err());
            assert!(path_is_link(&root.join("app/link")));
        }
    }

    #[test]
    fn record_lock_serializes_writers() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("record");
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = p.clone();
            let c = counter.clone();
            handles.push(std::thread::spawn(move || {
                with_record_lock(&p, || {
                    let n = c.load(Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    c.store(n + 1, Ordering::SeqCst);
                });
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 8);
    }
}
