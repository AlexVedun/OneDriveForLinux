//! Durable virtual-drive metadata operations awaiting Graph confirmation.

use crate::draft::NewFileLocation;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationKind {
    Delete,
    Rename {
        target_parent_id: String,
        target_name: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationStatus {
    Pending,
    Applied,
    Conflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mutation {
    pub drive_id: String,
    pub item_id: String,
    pub parent_id: String,
    pub name: String,
    pub etag: String,
    pub directory: bool,
    pub kind: MutationKind,
    pub status: MutationStatus,
}

impl Mutation {
    fn validate(&self) -> io::Result<()> {
        if self.drive_id.is_empty()
            || self.item_id.is_empty()
            || self.parent_id.is_empty()
            || self.etag.is_empty()
            || !NewFileLocation::valid_name(&self.name)
            || matches!(&self.kind, MutationKind::Rename { target_parent_id, target_name }
                if target_parent_id.is_empty() || !NewFileLocation::valid_name(target_name))
        {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        Ok(())
    }
}

pub struct MutationStore {
    root: PathBuf,
}

impl MutationStore {
    pub fn open(draft_root: &Path) -> io::Result<Self> {
        let root = draft_root.join("mutations");
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

    fn path(&self, drive_id: &str, item_id: &str) -> PathBuf {
        let mut hash = Sha256::new();
        for part in [drive_id, item_id] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        self.root.join(format!("{:x}.json", hash.finalize()))
    }

    fn lock(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.root.join(".lock"))?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }

    pub fn list(&self) -> io::Result<Vec<Mutation>> {
        let mut result = Vec::new();
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
            if !file.metadata()?.is_file() || file.metadata()?.len() > 8192 {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            let mutation: Mutation = serde_json::from_slice(&bytes)?;
            mutation.validate()?;
            if self.path(&mutation.drive_id, &mutation.item_id) != entry.path() {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            result.push(mutation);
        }
        result.sort_by(|a, b| (&a.drive_id, &a.item_id).cmp(&(&b.drive_id, &b.item_id)));
        Ok(result)
    }

    pub fn add(&self, mutation: &Mutation) -> io::Result<()> {
        mutation.validate()?;
        let _lock = self.lock()?;
        self.write(mutation, true)
    }

    pub fn update_status(&self, mutation: &Mutation, status: MutationStatus) -> io::Result<()> {
        let _lock = self.lock()?;
        let current = self
            .list()?
            .into_iter()
            .find(|entry| entry.drive_id == mutation.drive_id && entry.item_id == mutation.item_id)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if current != *mutation {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        self.write(&Mutation { status, ..current }, false)
    }

    pub fn remove(&self, mutation: &Mutation) -> io::Result<()> {
        let _lock = self.lock()?;
        fs::remove_file(self.path(&mutation.drive_id, &mutation.item_id))?;
        File::open(&self.root)?.sync_all()
    }

    /// Cancel only a conflict. Pending work may already be in flight in Graph.
    pub fn remove_conflict(&self, drive_id: &str, item_id: &str) -> io::Result<()> {
        let _lock = self.lock()?;
        let current = self
            .list()?
            .into_iter()
            .find(|entry| entry.drive_id == drive_id && entry.item_id == item_id)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if current.status != MutationStatus::Conflict {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        fs::remove_file(self.path(drive_id, item_id))?;
        File::open(&self.root)?.sync_all()
    }

    fn approval_path(&self) -> PathBuf {
        self.root.join("approved-deletes.approval")
    }

    fn pending_deletes(mutations: &[Mutation]) -> Vec<(String, String, String)> {
        mutations
            .iter()
            .filter(|entry| {
                entry.status == MutationStatus::Pending && entry.kind == MutationKind::Delete
            })
            .map(|entry| {
                (
                    entry.drive_id.clone(),
                    entry.item_id.clone(),
                    entry.etag.clone(),
                )
            })
            .collect()
    }

    pub fn approve_deletions(&self, expected_count: usize) -> io::Result<()> {
        let _lock = self.lock()?;
        let deletes = Self::pending_deletes(&self.list()?);
        if deletes.len() <= 10 || deletes.len() != expected_count {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temporary, &deletes)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.approval_path())
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()
    }

    pub fn deletions_approved(&self, mutations: &[Mutation]) -> io::Result<bool> {
        let pending = Self::pending_deletes(mutations);
        if pending.len() <= 10 {
            return Ok(true);
        }
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.approval_path())
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() || file.metadata()?.len() > 64 * 1024 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let approved: Vec<(String, String, String)> = serde_json::from_slice(&bytes)?;
        Ok(pending.iter().all(|entry| approved.contains(entry)))
    }

    fn write(&self, mutation: &Mutation, new: bool) -> io::Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temporary, mutation)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        if new {
            temporary
                .persist_noclobber(self.path(&mutation.drive_id, &mutation.item_id))
                .map_err(|error| error.error)?;
        } else {
            temporary
                .persist(self.path(&mutation.drive_id, &mutation.item_id))
                .map_err(|error| error.error)?;
        }
        File::open(&self.root)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_survives_restart_and_refuses_duplicate() {
        let root = tempfile::tempdir().unwrap();
        let store = MutationStore::open(root.path()).unwrap();
        let mutation = Mutation {
            drive_id: "drive".into(),
            item_id: "file".into(),
            parent_id: "root".into(),
            name: "name.txt".into(),
            etag: "v1".into(),
            directory: false,
            kind: MutationKind::Delete,
            status: MutationStatus::Pending,
        };
        store.add(&mutation).unwrap();
        assert_eq!(
            store.add(&mutation).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let reopened = MutationStore::open(root.path()).unwrap();
        assert_eq!(reopened.list().unwrap().first(), Some(&mutation));
        reopened
            .update_status(&mutation, MutationStatus::Applied)
            .unwrap();
        assert_eq!(reopened.list().unwrap()[0].status, MutationStatus::Applied);
        reopened.remove(&mutation).unwrap();
        assert!(reopened.list().unwrap().is_empty());
    }

    #[test]
    fn only_conflicted_operation_can_be_cancelled() {
        let root = tempfile::tempdir().unwrap();
        let store = MutationStore::open(root.path()).unwrap();
        let mutation = Mutation {
            drive_id: "drive".into(),
            item_id: "file".into(),
            parent_id: "root".into(),
            name: "name.txt".into(),
            etag: "v1".into(),
            directory: false,
            kind: MutationKind::Delete,
            status: MutationStatus::Pending,
        };
        store.add(&mutation).unwrap();
        assert_eq!(
            store.remove_conflict("drive", "file").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        store
            .update_status(&mutation, MutationStatus::Conflict)
            .unwrap();
        store.remove_conflict("drive", "file").unwrap();
        assert!(
            MutationStore::open(root.path())
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mass_delete_approval_matches_exact_pending_set() {
        let root = tempfile::tempdir().unwrap();
        let store = MutationStore::open(root.path()).unwrap();
        for index in 0..11 {
            store
                .add(&Mutation {
                    drive_id: "drive".into(),
                    item_id: format!("item-{index}"),
                    parent_id: "root".into(),
                    name: format!("name-{index}"),
                    etag: "v1".into(),
                    directory: false,
                    kind: MutationKind::Delete,
                    status: MutationStatus::Pending,
                })
                .unwrap();
        }
        assert!(!store.deletions_approved(&store.list().unwrap()).unwrap());
        assert!(store.approve_deletions(10).is_err());
        store.approve_deletions(11).unwrap();
        assert!(
            MutationStore::open(root.path())
                .unwrap()
                .deletions_approved(&store.list().unwrap())
                .unwrap()
        );
        store
            .add(&Mutation {
                drive_id: "drive".into(),
                item_id: "extra".into(),
                parent_id: "root".into(),
                name: "extra".into(),
                etag: "v1".into(),
                directory: false,
                kind: MutationKind::Delete,
                status: MutationStatus::Pending,
            })
            .unwrap();
        assert!(!store.deletions_approved(&store.list().unwrap()).unwrap());
    }
}
