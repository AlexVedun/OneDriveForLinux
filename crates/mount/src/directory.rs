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
}

impl LocalDirectory {
    fn validate(&self) -> io::Result<()> {
        if self.drive_id.is_empty()
            || !self.item_id.starts_with("local-dir:")
            || self.parent_id.is_empty()
            || !NewFileLocation::valid_name(&self.name)
        {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(())
    }
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
        let mut changed = directory.clone();
        changed.parent_id = parent_id.to_owned();
        changed.name = name.to_owned();
        changed.validate()?;
        self.save(&changed, false)
    }

    pub fn remove(&self, directory: &LocalDirectory) -> io::Result<()> {
        fs::remove_file(self.path(&directory.item_id))?;
        File::open(&self.root)?.sync_all()
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
}
