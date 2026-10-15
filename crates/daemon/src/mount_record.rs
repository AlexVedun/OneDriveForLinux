use onedrive_mount::preflight;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "virtual-mount-path";

fn record_path(state_root: &Path) -> PathBuf {
    state_root.join(FILE_NAME)
}

fn read(state_root: &Path) -> io::Result<Option<PathBuf>> {
    let file = record_path(state_root);
    let metadata = match fs::symlink_metadata(&file) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mount record",
        ));
    }
    let bytes = fs::read(file)?;
    if bytes.first() != Some(&b'/') || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid mount path",
        ));
    }
    Ok(Some(PathBuf::from(OsString::from_vec(bytes))))
}

/// Return an occupied mount left by an earlier daemon. Clear an unused intent record.
pub fn pending(state_root: &Path) -> io::Result<Option<PathBuf>> {
    let Some(path) = read(state_root)? else {
        return Ok(None);
    };
    if preflight::is_mountpoint_busy(&path)? {
        Ok(Some(path))
    } else {
        clear(state_root)?;
        Ok(None)
    }
}

/// Record intent before mounting so a daemon crash cannot lose the mount path.
pub fn write(state_root: &Path, path: &Path) -> io::Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(state_root)?;
    temporary.write_all(path.as_os_str().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(record_path(state_root))?;
    File::open(state_root)?.sync_all()?;
    Ok(())
}

pub fn clear(state_root: &Path) -> io::Result<()> {
    match fs::remove_file(record_path(state_root)) {
        Ok(()) => File::open(state_root)?.sync_all(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_private_and_stale_intent_is_cleared() {
        let state = tempfile::tempdir().unwrap();
        let mount = state.path().join("mount space");
        write(state.path(), &mount).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(record_path(state.path()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(read(state.path()).unwrap(), Some(mount));
        assert_eq!(pending(state.path()).unwrap(), None);
        assert_eq!(read(state.path()).unwrap(), None);
    }

    #[test]
    fn rejects_symlink_and_invalid_record() {
        let state = tempfile::tempdir().unwrap();
        let record = record_path(state.path());
        std::os::unix::fs::symlink("/etc/passwd", &record).unwrap();
        assert_eq!(
            read(state.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fs::remove_file(&record).unwrap();
        fs::write(&record, b"relative").unwrap();
        assert_eq!(
            read(state.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn occupied_path_is_preserved_for_recovery() {
        let state = tempfile::tempdir().unwrap();
        write(state.path(), Path::new("/")).unwrap();
        assert_eq!(pending(state.path()).unwrap(), Some(PathBuf::from("/")));
        assert_eq!(read(state.path()).unwrap(), Some(PathBuf::from("/")));
    }
}
