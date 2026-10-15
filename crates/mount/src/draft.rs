//! Durable local copies for future virtual-drive writes. No cloud operation is implied.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftIdentity {
    pub drive_id: String,
    pub item_id: String,
    /// Remote version that the local edit started from.
    pub base_etag: String,
}

impl DraftIdentity {
    fn validate(&self) -> io::Result<()> {
        if self.drive_id.is_empty() || self.item_id.is_empty() || self.base_etag.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty draft identity",
            ));
        }
        Ok(())
    }

    fn key(&self) -> String {
        let mut hash = Sha256::new();
        for part in [&self.drive_id, &self.item_id] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }
}

pub struct DraftStore {
    root: PathBuf,
}

pub struct DraftFile {
    identity: DraftIdentity,
    file: File,
}

impl DraftStore {
    pub fn open(root: &Path) -> io::Result<Self> {
        match fs::create_dir(root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(root)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid draft directory",
            ));
        }
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        Ok(Self { root: root.into() })
    }

    fn paths(&self, identity: &DraftIdentity) -> (PathBuf, PathBuf) {
        let key = identity.key();
        (
            self.root.join(format!("{key}.json")),
            self.root.join(format!("{key}.data")),
        )
    }

    /// A new draft starts from a complete local snapshot. The cache source is never modified.
    pub fn begin_from(&self, identity: DraftIdentity, source: &Path) -> io::Result<DraftFile> {
        identity.validate()?;
        if let Some(existing) = self.recover(&identity)? {
            return Ok(existing);
        }
        let (metadata_path, data_path) = self.paths(&identity);
        if fs::symlink_metadata(&data_path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "orphan draft data",
            ));
        }
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        if !input.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "draft source is not a file",
            ));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        io::copy(&mut input, &mut temporary)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(&data_path)
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()?;

        let mut metadata = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut metadata, &identity)?;
        metadata.flush()?;
        metadata.as_file().sync_all()?;
        metadata
            .persist_noclobber(metadata_path)
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()?;
        self.recover(&identity)?
            .ok_or_else(|| io::Error::other("draft disappeared"))
    }

    /// Return a local draft even if the remote version has since changed.
    pub fn recover(&self, identity: &DraftIdentity) -> io::Result<Option<DraftFile>> {
        identity.validate()?;
        let (metadata_path, data_path) = self.paths(identity);
        let metadata = match read_metadata(&metadata_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if metadata.drive_id != identity.drive_id || metadata.item_id != identity.item_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "draft identity mismatch",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(data_path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "draft data is not a file",
            ));
        }
        Ok(Some(DraftFile {
            identity: metadata,
            file,
        }))
    }

    pub fn list(&self) -> io::Result<Vec<DraftIdentity>> {
        let mut drafts = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
            {
                continue;
            }
            let identity = read_metadata(&entry.path())?;
            identity.validate()?;
            if self.paths(&identity).0 != entry.path() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "draft filename mismatch",
                ));
            }
            self.recover(&identity)?
                .ok_or_else(|| io::Error::other("draft disappeared"))?;
            drafts.push(identity);
        }
        drafts.sort_by(|left, right| {
            (&left.drive_id, &left.item_id).cmp(&(&right.drive_id, &right.item_id))
        });
        Ok(drafts)
    }
}

fn read_metadata(path: &Path) -> io::Result<DraftIdentity> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid draft metadata",
        ));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

impl DraftFile {
    pub fn identity(&self) -> &DraftIdentity {
        &self.identity
    }

    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read_at(buffer, offset)
    }

    /// A successful write is durably stored locally; upload is a separate step.
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<usize> {
        let length =
            u64::try_from(bytes.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        if offset
            .checked_add(length)
            .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut written = 0;
        while written < bytes.len() {
            let count = self
                .file
                .write_at(&bytes[written..], offset + written as u64)?;
            if count == 0 {
                return Err(io::Error::from(io::ErrorKind::WriteZero));
            }
            written += count;
        }
        if written != 0 {
            self.file.sync_all()?;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> DraftIdentity {
        DraftIdentity {
            drive_id: "drive".into(),
            item_id: "file".into(),
            base_etag: "v1".into(),
        }
    }

    #[test]
    fn writes_survive_reopen_without_changing_cached_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("cached");
        fs::write(&source, b"hello").unwrap();
        let root = directory.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let draft = store.begin_from(identity(), &source).unwrap();
        assert_eq!(draft.write_at(1, b"a").unwrap(), 1);
        assert_eq!(draft.identity(), &identity());
        drop(draft);
        drop(store);
        let reopened = DraftStore::open(&root).unwrap();
        assert_eq!(reopened.list().unwrap(), vec![identity()]);
        let mut bytes = [0; 5];
        reopened
            .recover(&identity())
            .unwrap()
            .unwrap()
            .read_at(0, &mut bytes)
            .unwrap();
        assert_eq!(&bytes, b"hallo");
        assert_eq!(fs::read(source).unwrap(), b"hello");
    }

    #[test]
    fn existing_draft_keeps_original_base_version() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"old").unwrap();
        let store = DraftStore::open(&directory.path().join("drafts")).unwrap();
        store
            .begin_from(identity(), &source)
            .unwrap()
            .write_at(0, b"new")
            .unwrap();
        fs::write(&source, b"remote").unwrap();
        let mut changed = identity();
        changed.base_etag = "v2".into();
        let draft = store.begin_from(changed, &source).unwrap();
        assert_eq!(draft.identity().base_etag, "v1");
        let mut bytes = [0; 3];
        draft.read_at(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"new");
    }

    #[test]
    fn refuses_symlinked_source_and_draft_directory() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"data").unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&source, &alias).unwrap();
        let store = DraftStore::open(&directory.path().join("drafts")).unwrap();
        assert!(store.begin_from(identity(), &alias).is_err());
        let other_alias = directory.path().join("draft-alias");
        std::os::unix::fs::symlink(directory.path().join("drafts"), &other_alias).unwrap();
        assert!(DraftStore::open(&other_alias).is_err());
    }

    #[test]
    fn incomplete_draft_is_reported_without_overwriting_it() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"original").unwrap();
        let store = DraftStore::open(&directory.path().join("drafts")).unwrap();
        let (_, data) = store.paths(&identity());
        fs::write(&data, b"unindexed edit").unwrap();
        let error = store.begin_from(identity(), &source).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&data).unwrap(), b"unindexed edit");
    }
}
