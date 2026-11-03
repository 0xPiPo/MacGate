//! The helper's and the command line's log file: owner-only, no symlinks.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

/// A log file past this size is moved to `<name>.1` when the next run opens it.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// Appends to (or creates) a log file with mode 0600, fixing the mode of an older file.
/// A file over [`MAX_LOG_BYTES`] is rotated first (one previous file is kept), so the log
/// cannot grow without bound across runs. A symlink at `path` is refused, so nobody can
/// redirect the log onto another file.
pub fn open_private_log(path: &std::path::Path) -> std::io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    rotate_if_large(path, MAX_LOG_BYTES);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Moves a regular file over `max` bytes to `<path>.1`, replacing an older one.
fn rotate_if_large(path: &std::path::Path, max: u64) {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return };
    if meta.file_type().is_file() && meta.len() > max {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, rotated);
    }
}

/// `~/Library/Logs/macrdp`, created owner-only (0700).
pub fn default_log_dir() -> std::io::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| std::io::Error::other("no home directory"))?;
    log_dir_in(&home)
}

fn log_dir_in(home: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join("Library/Logs/macrdp");
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_log_is_rotated_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("macrdp.log");
        std::fs::write(&path, vec![b'x'; 100]).unwrap();
        rotate_if_large(&path, 1000);
        assert!(path.exists(), "small files stay");
        rotate_if_large(&path, 10);
        assert!(!path.exists());
        assert_eq!(std::fs::read(dir.path().join("macrdp.log.1")).unwrap().len(), 100);
    }

    #[test]
    fn symlinked_log_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        std::fs::write(&target, "keep").unwrap();
        let link = dir.path().join("macrdp.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_private_log(&link).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    }

    #[test]
    fn log_dir_is_under_library_logs_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = log_dir_in(home.path()).unwrap();
        assert_eq!(dir, home.path().join("Library/Logs/macrdp"));
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn log_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_private_log(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Earlier runs stay readable for diagnosis.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    }
}
