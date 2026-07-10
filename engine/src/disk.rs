//! Best-effort free-disk-space probe.
//!
//! Used to pause disk-heavy work (RAG/memory indexing) when the volume is nearly
//! full, so the agent never tips the machine into a swap-thrash hang by writing
//! the embeddings DB onto a disk that has no room left.

use std::path::Path;

/// Free bytes available to an unprivileged user on the filesystem holding `path`
/// (or its nearest existing ancestor, since the target file may not exist yet).
/// `None` when it can't be determined — callers treat that as "not low".
pub fn free_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let mut probe = path;
        while !probe.exists() {
            match probe.parent() {
                Some(parent) => probe = parent,
                None => break,
            }
        }
        let c_path = CString::new(probe.as_os_str().as_bytes()).ok()?;
        // SAFETY: `statvfs` fills a zeroed struct; we only read scalar fields.
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
                return None;
            }
            let block = if stat.f_frsize != 0 {
                stat.f_frsize as u64
            } else {
                stat.f_bsize as u64
            };
            Some((stat.f_bavail as u64).saturating_mul(block))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Threshold below which disk-heavy work should pause. Default 1 GiB, overridable
/// with `GETAIBD_MIN_FREE_DISK_MB`.
pub fn min_free_bytes() -> u64 {
    std::env::var("GETAIBD_MIN_FREE_DISK_MB")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(1024)
        .saturating_mul(1024 * 1024)
}

/// True when the volume holding `path` is below the safe free-space threshold.
pub fn is_low(path: &Path) -> bool {
    free_bytes(path)
        .map(|free| free < min_free_bytes())
        .unwrap_or(false)
}
