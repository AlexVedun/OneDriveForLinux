//! Apply a remote file rename without following links or replacing local data.

use crate::{QueuedOperation, valid_name};
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

#[derive(Debug)]
pub enum RenameError {
    Changed,
    Io(io::Error),
}

impl From<io::Error> for RenameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn apply(root: &Path, operation: &QueuedOperation) -> Result<(), RenameError> {
    let source = operation.from_path.as_deref().ok_or(RenameError::Changed)?;
    let expected = operation
        .local_fingerprint
        .as_deref()
        .ok_or(RenameError::Changed)?;
    if operation.kind != "RenameLocal"
        || source == operation.path
        || expected.len() != 64
        || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RenameError::Changed);
    }
    let (source_dirs, source_name) = split(source)?;
    let (target_dirs, target_name) = split(&operation.path)?;
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let source_parent = parent(&root, &source_dirs)?;
    let target_parent = parent(&root, &target_dirs)?;
    let old = open_file(&source_parent, source_name)?;
    let new = open_file(&target_parent, target_name)?;
    match (old, new) {
        (None, Some(mut destination)) => {
            if digest(&mut destination)? == expected {
                Ok(())
            } else {
                Err(RenameError::Changed)
            }
        }
        (Some(_), Some(_)) | (None, None) => Err(RenameError::Changed),
        (Some(mut source_file), None) => {
            if digest(&mut source_file)? != expected {
                return Err(RenameError::Changed);
            }
            let before = source_file.metadata()?;
            let current = open_file(&source_parent, source_name)?.ok_or(RenameError::Changed)?;
            let now = current.metadata()?;
            if before.dev() != now.dev()
                || before.ino() != now.ino()
                || before.len() != now.len()
                || before.mtime() != now.mtime()
                || before.mtime_nsec() != now.mtime_nsec()
            {
                return Err(RenameError::Changed);
            }
            // OneDrive names are case insensitive, including on a Linux filesystem.
            let folded = target_name.to_lowercase();
            for entry in fs::read_dir(format!("/proc/self/fd/{}", target_parent.as_raw_fd()))? {
                if entry?
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.to_lowercase() == folded)
                {
                    return Err(RenameError::Changed);
                }
            }
            let old_name = CString::new(source_name).map_err(|_| RenameError::Changed)?;
            let new_name = CString::new(target_name).map_err(|_| RenameError::Changed)?;
            // SAFETY: both descriptors are open directories and both names are valid C strings.
            let result = unsafe {
                libc::renameat2(
                    source_parent.as_raw_fd(),
                    old_name.as_ptr(),
                    target_parent.as_raw_fd(),
                    new_name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if result != 0 {
                let error = io::Error::last_os_error();
                return if matches!(
                    error.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) {
                    Err(RenameError::Changed)
                } else {
                    Err(RenameError::Io(error))
                };
            }
            let mut moved = open_file(&target_parent, target_name)?.ok_or(RenameError::Changed)?;
            let after = moved.metadata()?;
            if before.dev() != after.dev()
                || before.ino() != after.ino()
                || digest(&mut moved)? != expected
            {
                return Err(RenameError::Changed);
            }
            Ok(())
        }
    }
}

pub fn fingerprint_matches(root: &Path, path: &str, expected: &str) -> bool {
    let Ok((dirs, name)) = split(path) else {
        return false;
    };
    let Ok(root) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
    else {
        return false;
    };
    let Ok(directory) = parent(&root, &dirs) else {
        return false;
    };
    let Ok(Some(mut file)) = open_file(&directory, name) else {
        return false;
    };
    digest(&mut file).is_ok_and(|fingerprint| fingerprint == expected)
}

/// Missing file is safe only when its parent remains an ordinary directory.
/// A link, directory, or inaccessible path is never interpreted as deletion.
pub fn path_is_absent(root: &Path, path: &str) -> bool {
    let Ok((dirs, name)) = split(path) else {
        return false;
    };
    let Ok(root) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
    else {
        return false;
    };
    let Ok(directory) = parent(&root, &dirs) else {
        return false;
    };
    let Ok(name) = CString::new(name) else {
        return false;
    };
    let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: directory is open, name is a valid C string, and status points
    // to writable storage for the result.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound
}

pub(crate) fn split(path: &str) -> Result<(Vec<&str>, &str), RenameError> {
    let mut parts: Vec<_> = path.split('/').collect();
    if parts.iter().any(|part| !valid_name(part)) {
        return Err(RenameError::Changed);
    }
    let name = parts.pop().ok_or(RenameError::Changed)?;
    Ok((parts, name))
}

pub(crate) fn parent(root: &File, parts: &[&str]) -> Result<File, RenameError> {
    let mut directory = root.try_clone()?;
    for part in parts {
        let name = CString::new(*part).map_err(|_| RenameError::Changed)?;
        // SAFETY: directory is an open descriptor and name is a valid C string.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(RenameError::Changed);
        }
        // SAFETY: openat returned a new owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

pub(crate) fn open_file(parent: &File, name: &str) -> Result<Option<File>, RenameError> {
    let name = CString::new(name).map_err(|_| RenameError::Changed)?;
    // SAFETY: parent is an open directory and name is a valid C string.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else if error.raw_os_error() == Some(libc::ELOOP) {
            Err(RenameError::Changed)
        } else {
            Err(RenameError::Io(error))
        };
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        return Err(RenameError::Changed);
    }
    Ok(Some(file))
}

pub(crate) fn digest(file: &mut File) -> Result<String, RenameError> {
    let before = file.metadata()?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let after = file.metadata()?;
    if before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
    {
        return Err(RenameError::Changed);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn operation(source: &str, target: &str) -> QueuedOperation {
        QueuedOperation {
            id: 1,
            kind: "RenameLocal".into(),
            path: target.into(),
            from_path: Some(source.into()),
            remote_id: Some("id".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some(format!("{:x}", Sha256::digest(b"content"))),
            attempts: 1,
        }
    }

    #[test]
    fn moves_and_recovers_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("sub")).unwrap();
        fs::write(temp.path().join("old"), b"content").unwrap();
        let op = operation("old", "sub/new");
        apply(temp.path(), &op).unwrap();
        assert!(!temp.path().join("old").exists());
        assert_eq!(fs::read(temp.path().join("sub/new")).unwrap(), b"content");
        apply(temp.path(), &op).unwrap();
    }

    #[test]
    fn rejects_changed_source_and_occupied_target() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("old"), b"changed").unwrap();
        let op = operation("old", "new");
        assert!(matches!(apply(temp.path(), &op), Err(RenameError::Changed)));
        fs::write(temp.path().join("old"), b"content").unwrap();
        fs::write(temp.path().join("new"), b"other").unwrap();
        assert!(matches!(apply(temp.path(), &op), Err(RenameError::Changed)));
        assert_eq!(fs::read(temp.path().join("new")).unwrap(), b"other");
    }

    #[test]
    fn rejects_symlink_parent_and_destination() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("old"), b"content").unwrap();
        symlink(temp.path(), temp.path().join("linked")).unwrap();
        assert!(matches!(
            apply(temp.path(), &operation("old", "linked/new")),
            Err(RenameError::Changed)
        ));
        symlink(temp.path().join("old"), temp.path().join("new")).unwrap();
        assert!(matches!(
            apply(temp.path(), &operation("old", "new")),
            Err(RenameError::Changed)
        ));
    }

    #[test]
    fn fingerprint_check_rejects_changed_file_and_link() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), b"content").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"content"));
        assert!(fingerprint_matches(temp.path(), "file", &hash));
        fs::write(temp.path().join("file"), b"changed").unwrap();
        assert!(!fingerprint_matches(temp.path(), "file", &hash));
        symlink(temp.path().join("file"), temp.path().join("link")).unwrap();
        assert!(!fingerprint_matches(temp.path(), "link", &hash));
    }

    #[test]
    fn absence_check_needs_real_parent_and_no_destination() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("real")).unwrap();
        assert!(path_is_absent(temp.path(), "real/missing"));
        assert!(!path_is_absent(temp.path(), "missing/file"));
        fs::write(temp.path().join("real/file"), b"data").unwrap();
        assert!(!path_is_absent(temp.path(), "real/file"));
        symlink(temp.path().join("real"), temp.path().join("linked")).unwrap();
        assert!(!path_is_absent(temp.path(), "linked/missing"));
        symlink(temp.path().join("real/file"), temp.path().join("real/link")).unwrap();
        assert!(!path_is_absent(temp.path(), "real/link"));
    }
}
