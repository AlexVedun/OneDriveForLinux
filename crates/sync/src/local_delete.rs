//! Keep approved local deletions recoverable in the private state directory.
//! A descriptor-anchored rename makes the move atomic on one filesystem.

use crate::QueuedOperation;
use crate::local_rename::{RenameError, digest, open_file, parent, path_is_absent, split};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

#[derive(Debug)]
pub enum DeleteError {
    Changed,
    CrossDevice,
    Io(io::Error),
}

impl From<RenameError> for DeleteError {
    fn from(error: RenameError) -> Self {
        match error {
            RenameError::Changed => Self::Changed,
            RenameError::Io(error) => Self::Io(error),
        }
    }
}
impl From<io::Error> for DeleteError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn quarantine(
    root: &Path,
    state: &Path,
    operation: &QueuedOperation,
) -> Result<(), DeleteError> {
    let expected = operation
        .local_fingerprint
        .as_deref()
        .ok_or(DeleteError::Changed)?;
    if operation.kind != "DeleteLocal"
        || operation.id <= 0
        || operation.from_path.is_some()
        || operation.remote_id.as_deref().is_none_or(str::is_empty)
        || operation.expected_etag.as_deref().is_none_or(str::is_empty)
        || expected.len() != 64
        || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DeleteError::Changed);
    }
    let (dirs, name) = split(&operation.path)?;
    let root_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let source_parent = parent(&root_dir, &dirs)?;
    let state_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state)?;
    let recovery_dir = recovery_directory(&state_dir)?;
    let target_name = operation.id.to_string();
    let source = open_file(&source_parent, name)?;
    let target = open_file(&recovery_dir, &target_name)?;
    match (source, target) {
        (None, Some(mut saved)) => {
            if !path_is_absent(root, &operation.path) || digest(&mut saved)? != expected {
                return Err(DeleteError::Changed);
            }
            Ok(())
        }
        (None, None) | (Some(_), Some(_)) => Err(DeleteError::Changed),
        (Some(mut source_file), None) => {
            if digest(&mut source_file)? != expected {
                return Err(DeleteError::Changed);
            }
            let before = source_file.metadata()?;
            let current = open_file(&source_parent, name)?.ok_or(DeleteError::Changed)?;
            let now = current.metadata()?;
            if before.dev() != now.dev()
                || before.ino() != now.ino()
                || before.len() != now.len()
                || before.mtime() != now.mtime()
                || before.mtime_nsec() != now.mtime_nsec()
            {
                return Err(DeleteError::Changed);
            }
            let old_name = CString::new(name).map_err(|_| DeleteError::Changed)?;
            let new_name = CString::new(target_name).map_err(|_| DeleteError::Changed)?;
            // SAFETY: both descriptors are open directories and names are valid C strings.
            let result = unsafe {
                libc::renameat2(
                    source_parent.as_raw_fd(),
                    old_name.as_ptr(),
                    recovery_dir.as_raw_fd(),
                    new_name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if result != 0 {
                let error = io::Error::last_os_error();
                return match error.raw_os_error() {
                    Some(libc::EXDEV) => Err(DeleteError::CrossDevice),
                    Some(libc::EEXIST) | Some(libc::ENOENT) => Err(DeleteError::Changed),
                    _ => Err(DeleteError::Io(error)),
                };
            }
            let mut saved =
                open_file(&recovery_dir, &operation.id.to_string())?.ok_or(DeleteError::Changed)?;
            let after = saved.metadata()?;
            if before.dev() != after.dev()
                || before.ino() != after.ino()
                || digest(&mut saved)? != expected
            {
                return Err(DeleteError::Changed);
            }
            source_parent.sync_all()?;
            recovery_dir.sync_all()?;
            Ok(())
        }
    }
}

fn recovery_directory(state: &File) -> Result<File, DeleteError> {
    let name = CString::new("deleted-files").expect("static directory name");
    // SAFETY: state is an open directory and name is a valid C string.
    let made = unsafe { libc::mkdirat(state.as_raw_fd(), name.as_ptr(), 0o700) };
    if made != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(DeleteError::Io(error));
        }
    }
    // SAFETY: state is an open directory and name is a valid C string.
    let fd = unsafe {
        libc::openat(
            state.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(DeleteError::Changed);
    }
    // SAFETY: openat returned a new owned descriptor.
    let directory = unsafe { File::from_raw_fd(fd) };
    let metadata = directory.metadata()?;
    // SAFETY: geteuid reads the current process identity.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(DeleteError::Changed);
    }
    if made == 0 {
        state.sync_all()?;
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::os::unix::fs::symlink;

    fn operation(path: &str) -> QueuedOperation {
        QueuedOperation {
            id: 7,
            kind: "DeleteLocal".into(),
            path: path.into(),
            from_path: None,
            remote_id: Some("id".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some(format!("{:x}", Sha256::digest(b"content"))),
            attempts: 1,
        }
    }

    #[test]
    fn moves_to_private_recovery_and_recovers_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sync");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(root.join("parent")).unwrap();
        fs::write(root.join("parent/file"), b"content").unwrap();
        let op = operation("parent/file");
        quarantine(&root, &state, &op).unwrap();
        assert!(!root.join("parent/file").exists());
        assert_eq!(fs::read(state.join("deleted-files/7")).unwrap(), b"content");
        quarantine(&root, &state, &op).unwrap();
    }

    #[test]
    fn refuses_changed_source_occupied_recovery_and_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sync");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(root.join("file"), b"changed").unwrap();
        assert!(matches!(
            quarantine(&root, &state, &operation("file")),
            Err(DeleteError::Changed)
        ));
        fs::write(root.join("file"), b"content").unwrap();
        fs::write(state.join("deleted-files/7"), b"other").unwrap();
        assert!(matches!(
            quarantine(&root, &state, &operation("file")),
            Err(DeleteError::Changed)
        ));
        fs::remove_file(state.join("deleted-files/7")).unwrap();
        symlink(root.join("file"), root.join("link")).unwrap();
        assert!(matches!(
            quarantine(&root, &state, &operation("link")),
            Err(DeleteError::Changed)
        ));
        assert_eq!(fs::read(root.join("file")).unwrap(), b"content");
    }

    #[test]
    fn refuses_recovery_folder_with_open_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sync");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(state.join("deleted-files")).unwrap();
        fs::set_permissions(
            state.join("deleted-files"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::write(root.join("file"), b"content").unwrap();
        assert!(matches!(
            quarantine(&root, &state, &operation("file")),
            Err(DeleteError::Changed)
        ));
        assert!(root.join("file").exists());
    }
}
