//! Linux directory notifications. Events only invalidate the local snapshot;
//! callers still run a complete scan before planning any operation.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const WATCH_MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_MODIFY
    | libc::IN_ATTRIB
    | libc::IN_CLOSE_WRITE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_DONT_FOLLOW
    | libc::IN_ONLYDIR;

pub struct LocalMonitor {
    root: PathBuf,
    fd: OwnedFd,
    directories: HashMap<i32, PathBuf>,
}

impl LocalMonitor {
    pub fn new(root: &Path) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root is not a directory",
            ));
        }
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut monitor = Self {
            root: root.clone(),
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            directories: HashMap::new(),
        };
        monitor.watch_tree(&root)?;
        Ok(monitor)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True means the complete local tree should be scanned again. Rebuild
    /// watches after an overflow; the missing events are never guessed at.
    pub fn poll(&mut self) -> io::Result<bool> {
        let mut dirty = false;
        let mut overflow = false;
        let mut new_directories = Vec::new();
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let size = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if size < 0 {
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    _ => return Err(error),
                }
            }
            if size == 0 {
                break;
            }
            let size = size as usize;
            let mut offset = 0;
            while offset + std::mem::size_of::<libc::inotify_event>() <= size {
                let event = unsafe {
                    std::ptr::read_unaligned(
                        buffer.as_ptr().add(offset).cast::<libc::inotify_event>(),
                    )
                };
                let header = std::mem::size_of::<libc::inotify_event>();
                let end = offset
                    .checked_add(header)
                    .and_then(|start| start.checked_add(event.len as usize))
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid event"))?;
                if end > size {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "short event"));
                }
                dirty = true;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    overflow = true;
                }
                if event.mask & libc::IN_ISDIR != 0
                    && event.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
                    && let Some(parent) = self.directories.get(&event.wd)
                {
                    let bytes = &buffer[offset + header..end];
                    let name = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
                    if !name.is_empty() && !name.contains(&b'/') {
                        new_directories.push(parent.join(OsStr::from_bytes(name)));
                    }
                }
                if event.mask & libc::IN_IGNORED != 0 {
                    self.directories.remove(&event.wd);
                }
                offset = end;
            }
        }
        if overflow {
            *self = Self::new(&self.root)?;
            return Ok(true);
        }
        if !self.directories.values().any(|path| path == &self.root) {
            *self = Self::new(&self.root)?;
            return Ok(true);
        }
        for directory in new_directories {
            match self.watch_tree(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(dirty)
    }

    fn watch_tree(&mut self, root: &Path) -> io::Result<()> {
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            let metadata = fs::symlink_metadata(&directory)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            let name = CString::new(directory.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
            let wd =
                unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), name.as_ptr(), WATCH_MASK) };
            if wd < 0 {
                return Err(io::Error::last_os_error());
            }
            self.directories.insert(wd, directory.clone());
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.is_dir() && !metadata.file_type().is_symlink() {
                    pending.push(entry.path());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observes_existing_and_new_subdirectories() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("existing")).unwrap();
        let mut monitor = LocalMonitor::new(directory.path()).unwrap();
        assert!(!monitor.poll().unwrap());
        fs::write(directory.path().join("existing/a.txt"), b"a").unwrap();
        assert!(monitor.poll().unwrap());
        fs::create_dir(directory.path().join("new")).unwrap();
        assert!(monitor.poll().unwrap());
        fs::write(directory.path().join("new/b.txt"), b"b").unwrap();
        assert!(monitor.poll().unwrap());
    }

    #[test]
    fn does_not_watch_through_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let outside = directory.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let mut monitor = LocalMonitor::new(&root).unwrap();
        fs::write(outside.join("private.txt"), b"private").unwrap();
        assert!(!monitor.poll().unwrap());
    }
}
