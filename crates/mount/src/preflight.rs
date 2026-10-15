//! Mount preparation checks. They do not create or mount any directory.

use onedrive_core::{PathError, resolve_path, validate_distinct_paths};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum MountError {
    Path(PathError),
    MissingDirectory,
    NotDirectory,
    Symlink,
    NotEmpty,
    Busy,
    FuseUnavailable,
    FusePermission,
    Io(io::Error),
}

impl std::fmt::Display for MountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(error) => write!(f, "mount paths overlap or are invalid: {error}"),
            Self::MissingDirectory => write!(f, "mount directory does not exist"),
            Self::NotDirectory => write!(f, "mount point is not a directory"),
            Self::Symlink => write!(f, "mount point must not be a symlink"),
            Self::NotEmpty => write!(f, "mount directory is not empty"),
            Self::Busy => write!(f, "mount point is already occupied"),
            Self::FuseUnavailable => write!(f, "FUSE device is unavailable"),
            Self::FusePermission => write!(f, "cannot access FUSE device"),
            Self::Io(error) => write!(f, "mount preflight I/O error: {error}"),
        }
    }
}

impl std::error::Error for MountError {}

impl From<io::Error> for MountError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PathError> for MountError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

/// Validate a configured mount point against both data locations.
/// The returned path has existing ancestors resolved and is safe to pass to a mount adapter.
pub fn validate_mount_paths(
    mount: &Path,
    sync: Option<&Path>,
    cache: &Path,
) -> Result<PathBuf, MountError> {
    if let Some(sync) = sync {
        validate_distinct_paths(&[mount, sync, cache])?;
    } else {
        validate_distinct_paths(&[mount, cache])?;
    }
    let metadata = match fs::symlink_metadata(mount) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(MountError::MissingDirectory);
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Err(MountError::Symlink);
    }
    if !metadata.is_dir() {
        return Err(MountError::NotDirectory);
    }
    let resolved = resolve_path(mount)?;
    if is_mountpoint_busy(&resolved)? {
        return Err(MountError::Busy);
    }
    if fs::read_dir(mount)?.next().transpose()?.is_some() {
        return Err(MountError::NotEmpty);
    }
    Ok(resolved)
}

/// Check the kernel FUSE device separately so sync-folder mode stays independent.
pub fn check_fuse_device() -> Result<(), MountError> {
    let device = Path::new("/dev/fuse");
    let metadata = match fs::metadata(device) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(MountError::FuseUnavailable);
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            return Err(MountError::FusePermission);
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_char_device() {
        return Err(MountError::FuseUnavailable);
    }
    match OpenOptions::new().read(true).write(true).open(device) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            Err(MountError::FusePermission)
        }
        Err(error) => Err(MountError::Io(error)),
    }
}

/// A helper is needed when the kernel rejects direct mounting for an unprivileged user.
/// Its absence does not rule out direct mounting.
pub fn fuse_mount_helper_available() -> bool {
    if let Some(override_path) = std::env::var_os("FUSERMOUNT_PATH") {
        return executable_in_path(Path::new(&override_path));
    }
    ["fusermount3", "fusermount"]
        .iter()
        .any(|name| executable_in_path(Path::new(name)))
        || [
            "/sbin/fusermount3",
            "/sbin/fusermount",
            "/bin/fusermount3",
            "/bin/fusermount",
        ]
        .iter()
        .any(|name| executable_in_path(Path::new(name)))
}

fn executable_in_path(path: &Path) -> bool {
    if path.components().count() > 1 || path.is_absolute() {
        return is_executable(path);
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|directory| is_executable(&directory.join(path)))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Check an existing mount without requiring an empty directory.
pub fn is_mountpoint_busy(path: &Path) -> io::Result<bool> {
    Ok(mountinfo_contains(
        &fs::read_to_string("/proc/self/mountinfo")?,
        path,
    ))
}

fn mountinfo_contains(content: &str, path: &Path) -> bool {
    content.lines().any(|line| {
        line.split_whitespace()
            .nth(4)
            .and_then(decode_mount_path)
            .is_some_and(|mount| mount == path)
    })
}

fn decode_mount_path(encoded: &str) -> Option<PathBuf> {
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut source = encoded.as_bytes().iter().copied();
    while let Some(byte) = source.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let digits = [source.next()?, source.next()?, source.next()?];
        if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
            return None;
        }
        let value = u16::from(digits[0] - b'0') * 64
            + u16::from(digits[1] - b'0') * 8
            + u16::from(digits[2] - b'0');
        bytes.push(u8::try_from(value).ok()?);
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_overlaps_symlink_and_nonempty_mount() {
        let directory = tempfile::tempdir().unwrap();
        let sync = directory.path().join("sync");
        let cache = directory.path().join("cache");
        let mount = directory.path().join("mount");
        fs::create_dir(&sync).unwrap();
        fs::create_dir(&cache).unwrap();
        fs::create_dir(&mount).unwrap();
        assert_eq!(
            validate_mount_paths(&mount, Some(&sync), &cache).unwrap(),
            mount
        );
        assert!(matches!(
            validate_mount_paths(&sync, Some(&sync), &cache),
            Err(MountError::Path(_))
        ));
        assert!(matches!(
            validate_mount_paths(&sync.join("nested"), Some(&sync), &cache),
            Err(MountError::Path(_))
        ));
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&mount, &alias).unwrap();
        assert!(matches!(
            validate_mount_paths(&alias, Some(&sync), &cache),
            Err(MountError::Symlink)
        ));
        fs::write(mount.join("existing"), b"data").unwrap();
        assert!(matches!(
            validate_mount_paths(&mount, Some(&sync), &cache),
            Err(MountError::NotEmpty)
        ));
    }

    #[test]
    fn mountinfo_decodes_escaped_paths() {
        let info = "31 20 0:45 / /tmp/One\\040Drive\\134Linux rw - fuse.test test rw\n";
        assert!(mountinfo_contains(info, Path::new("/tmp/One Drive\\Linux")));
        assert!(!mountinfo_contains(info, Path::new("/tmp/Other")));
        assert!(!mountinfo_contains(
            "31 20 0:45 / /tmp/bad\\xyz rw - fuse.test test rw",
            Path::new("/tmp/bad")
        ));
        assert!(!mountinfo_contains(
            "31 20 0:45 / /tmp/bad\\777 rw - fuse.test test rw",
            Path::new("/tmp/bad")
        ));
    }

    #[test]
    fn helper_probe_requires_executable_file() {
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("fusermount3");
        fs::write(&helper, b"#!/bin/sh\n").unwrap();
        assert!(!is_executable(&helper));
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_executable(&helper));
        assert!(!is_executable(directory.path()));
    }
}
