//! Durable local directories created through the virtual drive.

use crate::draft::NewFileLocation;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDirectory {
    pub drive_id: String,
    pub item_id: String,
    pub parent_id: String,
    pub name: String,
    /// ID returned by folder creation, before a separate metadata check succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_remote_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
    #[serde(default)]
    pub seen_remote: bool,
}

impl LocalDirectory {
    fn validate(&self) -> io::Result<()> {
        if self.drive_id.is_empty()
            || !self.item_id.starts_with("local-dir:")
            || self.parent_id.is_empty()
            || !NewFileLocation::valid_name(&self.name)
            || self
                .created_remote_id
                .as_deref()
                .is_some_and(|id| !valid_remote_id(id))
            || self
                .remote_id
                .as_deref()
                .is_some_and(|id| !valid_remote_id(id))
            || self.created_remote_id.is_some() && self.remote_id.is_some()
            || (self.seen_remote && self.remote_id.is_none())
        {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(())
    }
}

fn valid_remote_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with("local:") && !id.starts_with("local-dir:")
}

pub struct DirectoryStore {
    root: PathBuf,
}

impl DirectoryStore {
    pub fn open(draft_root: &Path) -> io::Result<Self> {
        let root = draft_root.join("directories");
        match fs::create_dir(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&root)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root })
    }

    fn path(&self, item_id: &str) -> PathBuf {
        let digest = Sha256::digest(item_id.as_bytes());
        self.root.join(format!("{digest:x}.json"))
    }

    pub fn list(&self) -> io::Result<Vec<LocalDirectory>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
            {
                continue;
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(entry.path())?;
            if !file.metadata()?.is_file() || file.metadata()?.len() > 4096 {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let directory: LocalDirectory = serde_json::from_slice(&bytes)?;
            directory.validate()?;
            if self.path(&directory.item_id) != entry.path() {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            entries.push(directory);
        }
        Ok(entries)
    }

    pub fn create(
        &self,
        drive_id: &str,
        parent_id: &str,
        name: &str,
    ) -> io::Result<LocalDirectory> {
        let temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        let random = temporary.path().file_name().unwrap().to_string_lossy();
        let directory = LocalDirectory {
            drive_id: drive_id.to_owned(),
            item_id: format!("local-dir:{random}"),
            parent_id: parent_id.to_owned(),
            name: name.to_owned(),
            created_remote_id: None,
            remote_id: None,
            seen_remote: false,
        };
        directory.validate()?;
        drop(temporary);
        self.save(&directory, true)?;
        Ok(directory)
    }

    pub fn relocate(
        &self,
        directory: &LocalDirectory,
        parent_id: &str,
        name: &str,
    ) -> io::Result<()> {
        if directory.remote_id.is_some() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if directory.created_remote_id.is_some() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let mut changed = directory.clone();
        changed.parent_id = parent_id.to_owned();
        changed.name = name.to_owned();
        changed.validate()?;
        self.save(&changed, false)
    }

    pub fn remove(&self, directory: &LocalDirectory) -> io::Result<()> {
        if directory.remote_id.is_some() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if directory.created_remote_id.is_some() {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        fs::remove_file(self.path(&directory.item_id))?;
        File::open(&self.root)?.sync_all()
    }

    pub fn record_created(
        &self,
        directory: &LocalDirectory,
        remote_id: &str,
    ) -> io::Result<LocalDirectory> {
        if directory.remote_id.is_some()
            || directory.created_remote_id.is_some()
            || !valid_remote_id(remote_id)
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        self.require_current(directory)?;
        let mut created = directory.clone();
        created.created_remote_id = Some(remote_id.to_owned());
        self.save(&created, false)?;
        Ok(created)
    }

    pub fn mark_uploaded(&self, directory: &LocalDirectory, remote_id: &str) -> io::Result<()> {
        if directory.remote_id.is_some()
            || directory.created_remote_id.as_deref() != Some(remote_id)
            || !valid_remote_id(remote_id)
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        self.require_current(directory)?;
        let mut uploaded = directory.clone();
        uploaded.created_remote_id = None;
        uploaded.remote_id = Some(remote_id.to_owned());
        self.save(&uploaded, false)
    }

    fn require_current(&self, directory: &LocalDirectory) -> io::Result<()> {
        if self
            .list()?
            .into_iter()
            .find(|current| current.item_id == directory.item_id)
            .as_ref()
            != Some(directory)
        {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Ok(())
    }

    pub fn mark_seen(&self, directory: &LocalDirectory) -> io::Result<()> {
        if directory.remote_id.is_none() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut seen = directory.clone();
        seen.seen_remote = true;
        self.save(&seen, false)
    }

    pub fn remote_parent_id(&self, item_id: &str) -> io::Result<Option<String>> {
        Ok(self
            .list()?
            .into_iter()
            .find(|directory| directory.item_id == item_id)
            .and_then(|directory| directory.remote_id))
    }

    fn save(&self, directory: &LocalDirectory, new: bool) -> io::Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temporary, directory)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        if new {
            temporary
                .persist_noclobber(self.path(&directory.item_id))
                .map_err(|error| error.error)?;
        } else {
            temporary
                .persist(self.path(&directory.item_id))
                .map_err(|error| error.error)?;
        }
        File::open(&self.root)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_survives_move_and_disappears_after_remove() {
        let root = tempfile::tempdir().unwrap();
        let store = DirectoryStore::open(root.path()).unwrap();
        let dir = store.create("drive", "root", "Новая папка").unwrap();
        assert_eq!(
            DirectoryStore::open(root.path())
                .unwrap()
                .list()
                .unwrap()
                .first(),
            Some(&dir)
        );
        store.relocate(&dir, "other", "Moved").unwrap();
        let moved = DirectoryStore::open(root.path())
            .unwrap()
            .list()
            .unwrap()
            .remove(0);
        assert_eq!(
            (moved.parent_id.as_str(), moved.name.as_str()),
            ("other", "Moved")
        );
        store.remove(&moved).unwrap();
        assert!(
            DirectoryStore::open(root.path())
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn uploaded_directory_mapping_survives_restart() {
        let root = tempfile::tempdir().unwrap();
        let store = DirectoryStore::open(root.path()).unwrap();
        let local = store.create("drive", "root", "folder").unwrap();
        let created = store.record_created(&local, "remote-folder").unwrap();
        let store = DirectoryStore::open(root.path()).unwrap();
        assert_eq!(
            store.list().unwrap()[0].created_remote_id.as_deref(),
            Some("remote-folder")
        );
        assert_eq!(store.remote_parent_id(&local.item_id).unwrap(), None);
        assert!(store.relocate(&created, "root", "changed").is_err());
        assert!(store.remove(&created).is_err());
        store.mark_uploaded(&created, "remote-folder").unwrap();
        assert_eq!(
            store.remote_parent_id(&local.item_id).unwrap().as_deref(),
            Some("remote-folder")
        );
        let uploaded = store.list().unwrap().remove(0);
        assert!(store.remove(&uploaded).is_err());
        store.mark_seen(&uploaded).unwrap();
        assert!(store.list().unwrap()[0].seen_remote);
    }

    #[test]
    fn upload_mapping_does_not_overwrite_a_moved_or_removed_directory() {
        let root = tempfile::tempdir().unwrap();
        let store = DirectoryStore::open(root.path()).unwrap();
        let original = store.create("drive", "root", "folder").unwrap();
        store.relocate(&original, "root", "renamed").unwrap();
        assert_eq!(
            store
                .record_created(&original, "remote-folder")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        let moved = store.list().unwrap().remove(0);
        assert_eq!(moved.name, "renamed");
        store.remove(&moved).unwrap();
        assert_eq!(
            store
                .record_created(&original, "remote-folder")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn ignores_uncommitted_temporary_directory_record() {
        let root = tempfile::tempdir().unwrap();
        let store = DirectoryStore::open(root.path()).unwrap();
        let pending = tempfile::NamedTempFile::new_in(root.path().join("directories")).unwrap();
        std::fs::write(pending.path(), b"partial").unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}
