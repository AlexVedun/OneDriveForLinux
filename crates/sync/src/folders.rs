//! Prepare local directories from a complete remote catalog. All filesystem
//! operations are anchored to open directory descriptors and never follow
//! symbolic links.

use crate::{SyncError, valid_name};
use onedrive_catalog::CatalogItem;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct FolderPreparation {
    pub created: Vec<String>,
    pub skipped_item_ids: Vec<String>,
}

/// Enumerate local directories through directory descriptors. A link that
/// appears during traversal cannot redirect the scan outside the sync root.
pub fn scan_local_directories(root: &Path) -> Result<Vec<String>, SyncError> {
    let root_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let mut pending = vec![(root_dir, String::new())];
    let mut paths = Vec::new();
    while let Some((directory, prefix)) = pending.pop() {
        for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !valid_name(&name) || !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(child) = open_existing(&directory, &name)? {
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                paths.push(path.clone());
                pending.push((child, path));
            }
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn prepare(
    root: &Path,
    items: &[CatalogItem],
    root_id: &str,
) -> Result<FolderPreparation, SyncError> {
    let root_dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let by_id: HashMap<_, _> = items
        .iter()
        .map(|item| (item.item_id.as_str(), item))
        .collect();
    let mut candidates = Vec::new();
    let mut result = FolderPreparation::default();
    for item in items
        .iter()
        .filter(|item| !item.deleted && item.kind == "folder" && item.item_id != root_id)
    {
        if let Some(parts) = folder_path(item, items, &by_id, root_id) {
            candidates.push((item.item_id.clone(), parts));
        } else {
            result.skipped_item_ids.push(item.item_id.clone());
        }
    }
    candidates.sort_by(|a, b| (a.1.len(), &a.1).cmp(&(b.1.len(), &b.1)));
    for (id, parts) in candidates {
        let mut directory = root_dir.try_clone()?;
        let mut created = false;
        let mut safe = true;
        for part in &parts {
            match descend(&directory, part)? {
                Some((child, made)) => {
                    directory = child;
                    created |= made;
                }
                None => {
                    safe = false;
                    break;
                }
            }
        }
        if safe {
            if created {
                result.created.push(parts.join("/"));
            }
        } else {
            result.skipped_item_ids.push(id);
        }
    }
    result.skipped_item_ids.sort();
    Ok(result)
}

fn folder_path<'a>(
    folder: &'a CatalogItem,
    items: &'a [CatalogItem],
    by_id: &HashMap<&str, &'a CatalogItem>,
    root_id: &str,
) -> Option<Vec<String>> {
    let mut current = folder;
    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    loop {
        if !seen.insert(current.item_id.as_str()) {
            return None;
        }
        let name = current.name.as_deref()?;
        let parent_id = current.parent_id.as_deref()?;
        if !valid_name(name) || !unique_remote_name(items, parent_id, name) {
            return None;
        }
        parts.push(name.to_owned());
        if parent_id == root_id {
            break;
        }
        current = by_id.get(parent_id).copied()?;
        if current.deleted || current.kind != "folder" {
            return None;
        }
    }
    parts.reverse();
    Some(parts)
}

fn unique_remote_name(items: &[CatalogItem], parent_id: &str, name: &str) -> bool {
    let folded = name.to_lowercase();
    items
        .iter()
        .filter(|item| {
            !item.deleted
                && item.parent_id.as_deref() == Some(parent_id)
                && item
                    .name
                    .as_deref()
                    .is_some_and(|other| other.to_lowercase() == folded)
        })
        .count()
        == 1
}

fn descend(parent: &File, name: &str) -> io::Result<Option<(File, bool)>> {
    match existing_entry(parent, name)? {
        Some(false) => return Ok(None),
        Some(true) => {
            return open_existing(parent, name).map(|value| value.map(|file| (file, false)));
        }
        None => {}
    }
    let c_name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid folder name"))?;
    // SAFETY: parent is an open directory descriptor and name is a valid C string.
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) };
    if created != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        if existing_entry(parent, name)? != Some(true) {
            return Ok(None);
        }
    }
    open_existing(parent, name).map(|value| value.map(|file| (file, created == 0)))
}

fn existing_entry(parent: &File, name: &str) -> io::Result<Option<bool>> {
    let entries = fs::read_dir(format!("/proc/self/fd/{}", parent.as_raw_fd()))?;
    let mut exact = false;
    let folded = name.to_lowercase();
    for entry in entries {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|other| other.to_lowercase() == folded)
        {
            if entry.file_name() != name {
                return Ok(Some(false));
            }
            exact = true;
        }
    }
    Ok(exact.then_some(true))
}

fn open_existing(parent: &File, name: &str) -> io::Result<Option<File>> {
    match open_child(parent, name) {
        Ok(child) => Ok(Some(child)),
        Err(error) if is_unsafe_entry(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn open_child(parent: &File, name: &str) -> io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid folder name"))?;
    // SAFETY: the returned descriptor is owned by File; O_NOFOLLOW rejects links.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn is_unsafe_entry(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ELOOP | libc::ENOTDIR | libc::ENOENT)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyncStore;

    fn folder(id: &str, parent: &str, name: &str) -> CatalogItem {
        CatalogItem {
            drive_id: "drive".into(),
            item_id: id.into(),
            parent_id: Some(parent.into()),
            name: Some(name.into()),
            kind: "folder".into(),
            size: None,
            etag: None,
            ctag: None,
            modified: None,
            deleted: false,
        }
    }

    #[test]
    fn creates_nested_and_empty_remote_directories_once() {
        let temp = tempfile::tempdir().unwrap();
        let items = [
            folder("docs", "root", "Docs"),
            folder("drafts", "docs", "Drafts"),
        ];
        let first = prepare(temp.path(), &items, "root").unwrap();
        assert_eq!(first.created, ["Docs", "Docs/Drafts"]);
        assert!(first.skipped_item_ids.is_empty());
        assert!(temp.path().join("Docs/Drafts").is_dir());
        assert!(
            prepare(temp.path(), &items, "root")
                .unwrap()
                .created
                .is_empty()
        );
    }

    #[test]
    fn prepared_parent_enables_nested_download_queue() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sync");
        let state = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        let items = [
            folder("docs", "root", "Docs"),
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "file".into(),
                parent_id: Some("docs".into()),
                name: Some("report.txt".into()),
                kind: "file".into(),
                size: Some(3),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
        ];
        let mut store = SyncStore::in_memory().unwrap();
        store.set_root(&root, &state).unwrap();
        assert_eq!(store.enqueue_safe_downloads(&items, "root").unwrap(), 0);
        assert_eq!(
            store.prepare_local_folders(&items, "root").unwrap().created,
            ["Docs"]
        );
        assert_eq!(store.enqueue_safe_downloads(&items, "root").unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn queues_only_directories_with_existing_exact_remote_parent() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("sync");
        let state = temp.path().join("state");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(root.join("New")).unwrap();
        fs::create_dir(root.join("New/Child")).unwrap();
        fs::create_dir(root.join("Docs")).unwrap();
        fs::create_dir(root.join("Docs/Ready")).unwrap();
        fs::create_dir(root.join("Case")).unwrap();
        fs::create_dir(root.join("Taken")).unwrap();
        symlink(&outside, root.join("Linked")).unwrap();
        let mut taken = folder("taken", "root", "Taken");
        taken.kind = "file".into();
        let items = [
            folder("docs", "root", "Docs"),
            folder("case", "root", "case"),
            taken,
        ];
        let database = state.join("sync.sqlite3");
        let mut store = SyncStore::open(&database).unwrap();
        store.set_root(&root, &state).unwrap();
        assert_eq!(
            store.enqueue_safe_remote_folders(&items, "root").unwrap(),
            2
        );
        assert_eq!(
            store.enqueue_safe_remote_folders(&items, "root").unwrap(),
            0
        );
        drop(store);
        let mut store = SyncStore::open(&database).unwrap();
        let first = store.claim_next().unwrap().unwrap();
        let second = store.claim_next().unwrap().unwrap();
        let queued = [
            (first.path, first.remote_id),
            (second.path, second.remote_id),
        ];
        assert!(queued.contains(&("New".into(), Some("root".into()))));
        assert!(queued.contains(&("Docs/Ready".into(), Some("docs".into()))));
        assert!(store.claim_next().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_sync_root() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let link = temp.path().join("link");
        symlink(temp.path(), &link).unwrap();
        assert!(prepare(&link, &[folder("docs", "root", "Docs")], "root").is_err());
        assert!(!temp.path().join("Docs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_links_or_replace_files_or_case_variants() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), temp.path().join("Linked")).unwrap();
        fs::write(temp.path().join("Occupied"), b"local").unwrap();
        fs::create_dir(temp.path().join("docs")).unwrap();
        let items = [
            folder("link", "root", "Linked"),
            folder("nested", "link", "Nested"),
            folder("file", "root", "Occupied"),
            folder("case", "root", "Docs"),
        ];
        let result = prepare(temp.path(), &items, "root").unwrap();
        assert!(result.created.is_empty());
        assert_eq!(result.skipped_item_ids, ["case", "file", "link", "nested"]);
        assert!(!outside.path().join("Nested").exists());
        assert_eq!(fs::read(temp.path().join("Occupied")).unwrap(), b"local");
    }

    #[test]
    fn rejects_ambiguous_remote_names_or_broken_parent_chain() {
        let temp = tempfile::tempdir().unwrap();
        let items = [
            folder("one", "root", "Docs"),
            folder("two", "root", "docs"),
            folder("child", "one", "Nested"),
            folder("orphan", "missing", "Lost"),
        ];
        let result = prepare(temp.path(), &items, "root").unwrap();
        assert!(result.created.is_empty());
        assert_eq!(result.skipped_item_ids, ["child", "one", "orphan", "two"]);
    }
}
