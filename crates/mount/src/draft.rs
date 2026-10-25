//! Durable local copies for future virtual-drive writes. No cloud operation is implied.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftIdentity {
    pub drive_id: String,
    pub item_id: String,
    /// Remote version that the local edit started from.
    pub base_etag: String,
    /// Location of a file created in the virtual drive, before Graph assigns an ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_file: Option<NewFileLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewFileLocation {
    pub parent_id: String,
    pub name: String,
}

impl NewFileLocation {
    pub fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.ends_with([' ', '.'])
            && !name
                .chars()
                .any(|character| "\"*:<>?/\\|".contains(character) || character.is_control())
    }
}

impl DraftIdentity {
    fn validate(&self) -> io::Result<()> {
        if self.drive_id.is_empty() || self.item_id.is_empty() || self.base_etag.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty draft identity",
            ));
        }
        if let Some(location) = &self.new_file
            && (!self.item_id.starts_with("local:")
                || self.base_etag != "local-new"
                || location.parent_id.is_empty()
                || !NewFileLocation::valid_name(&location.name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid new file",
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
    detached: AtomicBool,
}

pub struct DraftSnapshot {
    pub identity: DraftIdentity,
    pub file: tempfile::NamedTempFile,
    pub size: u64,
    pub sha256: String,
}

/// The URL authorizes writes without a bearer token and must stay private.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftUploadCheckpoint {
    pub identity: DraftIdentity,
    pub sha256: String,
    pub size: u64,
    pub upload_url: String,
    /// Equal to size only when the final result is uncertain and the session is gone.
    pub next_offset: u64,
    /// The final range or an empty upload may have committed remotely even if its reply was lost.
    #[serde(default)]
    pub final_chunk_attempted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DraftUploadIssue {
    LoginRequired,
    RemoteChanged,
    TransferFailed,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftUploadFailure {
    pub identity: DraftIdentity,
    pub issue: DraftUploadIssue,
    pub attempted_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

impl std::fmt::Debug for DraftUploadCheckpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DraftUploadCheckpoint")
            .field("identity", &self.identity)
            .field("sha256", &self.sha256)
            .field("size", &self.size)
            .field("upload_url", &"[redacted]")
            .field("next_offset", &self.next_offset)
            .field("final_chunk_attempted", &self.final_chunk_attempted)
            .finish()
    }
}

impl DraftUploadCheckpoint {
    fn validate(&self) -> io::Result<()> {
        self.identity.validate()?;
        let valid_transfer = if self.size == 0 {
            self.final_chunk_attempted && self.next_offset == 0 && self.upload_url.is_empty()
        } else {
            (self.next_offset < self.size
                || (self.next_offset == self.size && self.final_chunk_attempted))
                && self.upload_url.starts_with("https://")
                && self.upload_url.len() <= 16 * 1024
        };
        if !valid_transfer
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid draft upload checkpoint",
            ));
        }
        Ok(())
    }
}

fn lock_exclusive(file: &File) -> io::Result<FileLock<'_>> {
    let result = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(file), libc::LOCK_EX) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileLock(file))
}

struct FileLock<'a>(&'a File);

impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(self.0), libc::LOCK_UN) };
    }
}

struct StoreLock(File);

impl Drop for StoreLock {
    fn drop(&mut self) {
        unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN) };
    }
}

fn lock_store(root: &Path) -> io::Result<StoreLock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join(".store-lock"))?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid store lock",
        ));
    }
    let result = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(StoreLock(file))
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
        let _lock = lock_store(root)?;
        let store = Self { root: root.into() };
        store.cleanup_removed()?;
        Ok(store)
    }

    /// Finish cleanup after a confirmed upload or a local-only file removal.
    fn cleanup_removed(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_none_or(|ext| ext != "uploaded" && ext != "removed")
            {
                continue;
            }
            let identity = read_metadata(&entry.path())?;
            identity.validate()?;
            let (metadata_path, data_path) = self.paths(&identity);
            if metadata_path.with_extension(entry.path().extension().unwrap()) != entry.path() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "removed marker filename mismatch",
                ));
            }
            if !metadata_path.exists() {
                match fs::remove_file(data_path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                File::open(&self.root)?.sync_all()?;
                match fs::remove_file(self.checkpoint_path(&identity)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                match fs::remove_file(self.failure_path(&identity)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            fs::remove_file(entry.path())?;
            File::open(&self.root)?.sync_all()?;
        }
        Ok(())
    }

    fn paths(&self, identity: &DraftIdentity) -> (PathBuf, PathBuf) {
        let key = identity.key();
        (
            self.root.join(format!("{key}.json")),
            self.root.join(format!("{key}.data")),
        )
    }

    fn checkpoint_path(&self, identity: &DraftIdentity) -> PathBuf {
        self.root.join(format!("{}.session", identity.key()))
    }

    fn failure_path(&self, identity: &DraftIdentity) -> PathBuf {
        self.root.join(format!("{}.failure", identity.key()))
    }

    pub fn upload_failure(
        &self,
        identity: &DraftIdentity,
    ) -> io::Result<Option<DraftUploadFailure>> {
        identity.validate()?;
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.failure_path(identity))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > 4096 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid upload failure",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let failure: DraftUploadFailure = serde_json::from_slice(&bytes)?;
        if failure.identity.drive_id != identity.drive_id
            || failure.identity.item_id != identity.item_id
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "upload failure identity mismatch",
            ));
        }
        if failure.identity != *identity {
            return Ok(None);
        }
        Ok(Some(failure))
    }

    pub fn save_upload_failure(&self, failure: &DraftUploadFailure) -> io::Result<()> {
        failure.identity.validate()?;
        let _lock = lock_store(&self.root)?;
        self.require_current_identity(&failure.identity)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temporary, failure)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.failure_path(&failure.identity))
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()
    }

    pub fn clear_upload_failure(&self, identity: &DraftIdentity) -> io::Result<()> {
        identity.validate()?;
        let _lock = lock_store(&self.root)?;
        match fs::remove_file(self.failure_path(identity)) {
            Ok(()) => File::open(&self.root)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn upload_checkpoint(
        &self,
        identity: &DraftIdentity,
    ) -> io::Result<Option<DraftUploadCheckpoint>> {
        identity.validate()?;
        let _lock = lock_store(&self.root)?;
        self.upload_checkpoint_locked(identity)
    }

    fn upload_checkpoint_locked(
        &self,
        identity: &DraftIdentity,
    ) -> io::Result<Option<DraftUploadCheckpoint>> {
        let path = self.checkpoint_path(identity);
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > 32 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid checkpoint file",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let checkpoint: DraftUploadCheckpoint = serde_json::from_slice(&bytes)?;
        checkpoint.validate()?;
        if checkpoint.identity.drive_id != identity.drive_id
            || checkpoint.identity.item_id != identity.item_id
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "draft checkpoint identity mismatch",
            ));
        }
        if checkpoint.identity != *identity {
            fs::remove_file(path)?;
            File::open(&self.root)?.sync_all()?;
            return Ok(None);
        }
        Ok(Some(checkpoint))
    }

    pub fn save_upload_checkpoint(&self, checkpoint: &DraftUploadCheckpoint) -> io::Result<()> {
        checkpoint.validate()?;
        let _lock = lock_store(&self.root)?;
        self.require_current_identity(&checkpoint.identity)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temporary, checkpoint)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.checkpoint_path(&checkpoint.identity))
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()
    }

    fn require_current_identity(&self, identity: &DraftIdentity) -> io::Result<()> {
        let draft = self
            .recover(identity)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if draft.identity() != identity {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        Ok(())
    }

    /// Check the exact draft version before a network upload request.
    pub fn current_identity_matches(&self, identity: &DraftIdentity) -> io::Result<bool> {
        identity.validate()?;
        let _lock = lock_store(&self.root)?;
        Ok(self
            .recover(identity)?
            .is_some_and(|draft| draft.identity() == identity))
    }

    pub fn clear_upload_checkpoint(&self, identity: &DraftIdentity) -> io::Result<()> {
        identity.validate()?;
        let _lock = lock_store(&self.root)?;
        match fs::remove_file(self.checkpoint_path(identity)) {
            Ok(()) => File::open(&self.root)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// A new draft starts from a complete local snapshot. The cache source is never modified.
    pub fn begin_from(&self, identity: DraftIdentity, source: &Path) -> io::Result<DraftFile> {
        identity.validate()?;
        if let Some(existing) = self.recover(&identity)? {
            return Ok(existing);
        }
        let input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        self.begin_from_file(identity, &input)
    }

    pub fn begin_from_file(&self, identity: DraftIdentity, input: &File) -> io::Result<DraftFile> {
        self.begin_with_source(identity, Some(input))
    }

    /// Atomically replace an existing draft's bytes after a successful editor rename.
    pub fn replace_from_file(
        &self,
        identity: &DraftIdentity,
        input: &File,
    ) -> io::Result<DraftFile> {
        identity.validate()?;
        let _lock = lock_store(&self.root)?;
        if self.recover(identity)?.is_none() || !input.metadata()?.is_file() {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        let mut offset = 0;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = input.read_at(&mut buffer, offset)?;
            if count == 0 {
                break;
            }
            temporary.write_all(&buffer[..count])?;
            offset += count as u64;
        }
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.paths(identity).1)
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()?;
        self.recover(identity)?
            .ok_or_else(|| io::Error::other("draft disappeared"))
    }

    pub fn begin_empty(&self, identity: DraftIdentity) -> io::Result<DraftFile> {
        self.begin_with_source(identity, None)
    }

    /// Change the visible location of a file that has never existed in Graph.
    pub fn relocate_new(
        &self,
        drive_id: &str,
        item_id: &str,
        location: NewFileLocation,
    ) -> io::Result<()> {
        let _lock = lock_store(&self.root)?;
        let draft = self
            .recover_item(drive_id, item_id)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let mut identity = draft.identity().clone();
        if identity.new_file.is_none() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if self
            .upload_checkpoint_locked(&identity)?
            .is_some_and(|checkpoint| checkpoint.final_chunk_attempted)
        {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        identity.new_file = Some(location);
        identity.validate()?;
        self.clear_stale_upload_state(&identity)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temporary, &identity)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.paths(&identity).0)
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()
    }

    /// Discard a local-only file; a marker completes data cleanup after a crash.
    pub fn remove_new(&self, drive_id: &str, item_id: &str) -> io::Result<()> {
        let _lock = lock_store(&self.root)?;
        let draft = self
            .recover_item(drive_id, item_id)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        if draft.identity().new_file.is_none() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        if self
            .upload_checkpoint_locked(draft.identity())?
            .is_some_and(|checkpoint| checkpoint.final_chunk_attempted)
        {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let metadata = self.paths(draft.identity()).0;
        fs::rename(&metadata, metadata.with_extension("removed"))?;
        File::open(&self.root)?.sync_all()?;
        self.cleanup_removed()
    }

    fn clear_stale_upload_state(&self, identity: &DraftIdentity) -> io::Result<()> {
        for path in [self.checkpoint_path(identity), self.failure_path(identity)] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        File::open(&self.root)?.sync_all()
    }

    fn begin_with_source(
        &self,
        identity: DraftIdentity,
        input: Option<&File>,
    ) -> io::Result<DraftFile> {
        identity.validate()?;
        let _store_lock = lock_store(&self.root)?;
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
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        if let Some(input) = input {
            if !input.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "draft source is not a file",
                ));
            }
            let mut position = 0;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = input.read_at(&mut buffer, position)?;
                if count == 0 {
                    break;
                }
                temporary.write_all(&buffer[..count])?;
                position += count as u64;
            }
        }
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
            detached: AtomicBool::new(false),
        }))
    }

    pub fn recover_item(&self, drive_id: &str, item_id: &str) -> io::Result<Option<DraftFile>> {
        self.recover(&DraftIdentity {
            drive_id: drive_id.to_owned(),
            item_id: item_id.to_owned(),
            base_etag: "lookup".to_owned(),
            new_file: None,
        })
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

    /// Capture a stable copy while FUSE writes to this draft are serialized.
    pub fn snapshot(&self, identity: &DraftIdentity) -> io::Result<Option<DraftSnapshot>> {
        let Some(draft) = self.recover(identity)? else {
            return Ok(None);
        };
        if draft.identity() != identity {
            return Ok(None);
        }
        let _lock = lock_exclusive(&draft.file)?;
        if draft.file.metadata()?.nlink() == 0
            || self
                .recover(identity)?
                .is_none_or(|current| current.identity() != identity)
        {
            return Ok(None);
        }
        let mut snapshot = tempfile::NamedTempFile::new_in(&self.root)?;
        let mut hasher = Sha256::new();
        let mut offset = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = draft.file.read_at(&mut buffer, offset)?;
            if read == 0 {
                break;
            }
            snapshot.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            offset += read as u64;
        }
        snapshot.as_file().sync_all()?;
        if self
            .recover(identity)?
            .is_none_or(|current| current.identity() != identity)
        {
            return Ok(None);
        }
        Ok(Some(DraftSnapshot {
            identity: draft.identity,
            file: snapshot,
            size: offset,
            sha256: format!("{:x}", hasher.finalize()),
        }))
    }

    /// Save a stable copy for manual conflict recovery without changing the draft.
    pub fn export_item(&self, drive_id: &str, item_id: &str, destination: &Path) -> io::Result<()> {
        if !destination.is_absolute() || destination.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "export destination must be an absolute file path",
            ));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing export directory"))?
            .canonicalize()?;
        if parent.starts_with(self.root.canonicalize()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot export into draft storage",
            ));
        }
        let destination = parent.join(destination.file_name().unwrap());
        let draft = self
            .recover_item(drive_id, item_id)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "draft is missing"))?;
        let snapshot = self
            .snapshot(draft.identity())?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "draft is missing"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
        io::copy(&mut snapshot.file.reopen()?, &mut temporary)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(destination)
            .map_err(|error| error.error)?;
        File::open(parent)?.sync_all()
    }

    /// Forget only the bytes and location that match the uploaded snapshot.
    pub fn acknowledge(&self, snapshot: &DraftSnapshot) -> io::Result<bool> {
        let _store_lock = lock_store(&self.root)?;
        let Some(draft) = self.recover(&snapshot.identity)? else {
            return Ok(false);
        };
        if draft.identity() != &snapshot.identity {
            return Ok(false);
        }
        let _lock = lock_exclusive(&draft.file)?;
        if draft.file.metadata()?.nlink() == 0 || self.recover(&snapshot.identity)?.is_none() {
            return Ok(false);
        }
        let mut hasher = Sha256::new();
        let mut offset = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = draft.file.read_at(&mut buffer, offset)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            offset += read as u64;
        }
        if offset != snapshot.size || format!("{:x}", hasher.finalize()) != snapshot.sha256 {
            return Ok(false);
        }
        let (metadata_path, data_path) = self.paths(&snapshot.identity);
        let uploaded_path = metadata_path.with_extension("uploaded");
        fs::rename(&metadata_path, &uploaded_path)?;
        File::open(&self.root)?.sync_all()?;
        fs::remove_file(data_path)?;
        File::open(&self.root)?.sync_all()?;
        match fs::remove_file(self.checkpoint_path(&snapshot.identity)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match fs::remove_file(self.failure_path(&snapshot.identity)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::remove_file(uploaded_path)?;
        File::open(&self.root)?.sync_all()?;
        Ok(true)
    }

    /// Keep later local edits based on the version that this upload just published.
    pub fn rebase_existing_after_upload(
        &self,
        snapshot: &DraftSnapshot,
        uploaded_etag: &str,
    ) -> io::Result<bool> {
        if snapshot.identity.new_file.is_some() || uploaded_etag.is_empty() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let _store_lock = lock_store(&self.root)?;
        let Some(draft) = self.recover(&snapshot.identity)? else {
            return Ok(false);
        };
        if draft.identity() != &snapshot.identity {
            return Ok(false);
        }
        let _lock = lock_exclusive(&draft.file)?;
        if draft.file.metadata()?.nlink() == 0 {
            return Ok(false);
        }
        let mut hasher = Sha256::new();
        let mut offset = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = draft.file.read_at(&mut buffer, offset)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            offset += read as u64;
        }
        if offset == snapshot.size && format!("{:x}", hasher.finalize()) == snapshot.sha256 {
            return Ok(false);
        }
        let mut identity = snapshot.identity.clone();
        identity.base_etag = uploaded_etag.to_owned();
        self.clear_stale_upload_state(&identity)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut temporary, &identity)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.paths(&identity).0)
            .map_err(|error| error.error)?;
        File::open(&self.root)?.sync_all()?;
        Ok(true)
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
    /// An unlinked local file remains usable through its existing FUSE handles.
    pub(crate) fn mark_detached(&self) {
        self.detached.store(true, Ordering::Release);
    }

    fn ensure_live_file(&self) -> io::Result<()> {
        if self.file.metadata()?.nlink() == 0 && !self.detached.load(Ordering::Acquire) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(())
    }

    pub fn identity(&self) -> &DraftIdentity {
        &self.identity
    }

    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    pub fn attributes(&self) -> io::Result<(u64, SystemTime, SystemTime)> {
        let metadata = self.file.metadata()?;
        Ok((metadata.len(), metadata.accessed()?, metadata.modified()?))
    }

    pub fn set_times(
        &self,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> io::Result<()> {
        let _lock = lock_exclusive(&self.file)?;
        self.ensure_live_file()?;
        let mut times = fs::FileTimes::new();
        if let Some(atime) = atime {
            times = times.set_accessed(atime);
        }
        if let Some(mtime) = mtime {
            times = times.set_modified(mtime);
        }
        self.file.set_times(times)?;
        self.file.sync_all()
    }

    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    pub fn set_len(&self, size: u64) -> io::Result<()> {
        if size > i64::MAX as u64 {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let _lock = lock_exclusive(&self.file)?;
        self.ensure_live_file()?;
        self.file.set_len(size)?;
        self.file.sync_all()
    }

    /// Reserve local space before acknowledging FUSE; cloud upload remains separate.
    pub fn allocate(&self, offset: u64, length: u64, keep_size: bool) -> io::Result<()> {
        if length == 0
            || offset
                .checked_add(length)
                .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let _lock = lock_exclusive(&self.file)?;
        self.ensure_live_file()?;
        let mode = if keep_size {
            libc::FALLOC_FL_KEEP_SIZE
        } else {
            0
        };
        let result =
            unsafe { libc::fallocate(self.file.as_raw_fd(), mode, offset as i64, length as i64) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        self.file.sync_all()
    }

    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        read_at_until_eof(offset, buffer, |buffer, offset| {
            self.file.read_at(buffer, offset)
        })
    }

    pub fn seek_data_hole(&self, offset: i64, whence: i32) -> io::Result<i64> {
        if offset < 0 || !matches!(whence, libc::SEEK_DATA | libc::SEEK_HOLE) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // Draft reads and writes use positioned I/O, so moving this fd's cursor is safe.
        let result = unsafe { libc::lseek(self.file.as_raw_fd(), offset, whence) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    /// A successful write is durably stored locally; upload is a separate step.
    pub fn write_at(&self, offset: u64, bytes: &[u8]) -> io::Result<usize> {
        let _lock = lock_exclusive(&self.file)?;
        self.write_at_locked(offset, bytes)
    }

    /// Append while holding the same file lock used by positioned writes.
    pub fn append(&self, bytes: &[u8]) -> io::Result<usize> {
        let _lock = lock_exclusive(&self.file)?;
        let offset = self.file.metadata()?.len();
        self.write_at_locked(offset, bytes)
    }

    fn write_at_locked(&self, offset: u64, bytes: &[u8]) -> io::Result<usize> {
        self.ensure_live_file()?;
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

fn read_at_until_eof(
    offset: u64,
    buffer: &mut [u8],
    mut read: impl FnMut(&mut [u8], u64) -> io::Result<usize>,
) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let Some(position) = offset.checked_add(filled as u64) else {
            break;
        };
        if position > i64::MAX as u64 {
            break;
        }
        let count = read(&mut buffer[filled..], position)?;
        if count == 0 {
            break;
        }
        filled += count;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positioned_read_continues_after_short_reads_until_eof() {
        let source = b"abcdef";
        let mut calls = Vec::new();
        let mut output = [0; 8];
        let count = read_at_until_eof(4, &mut output, |buffer, offset| {
            calls.push(offset);
            let position = (offset - 4) as usize;
            let count = buffer.len().min(2).min(source.len() - position);
            buffer[..count].copy_from_slice(&source[position..position + count]);
            Ok(count)
        })
        .unwrap();

        assert_eq!(count, source.len());
        assert_eq!(&output[..count], source);
        assert_eq!(calls, [4, 6, 8, 10]);
    }

    #[test]
    fn set_len_rejects_size_beyond_positioned_io_range_without_changing_draft() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"keep").unwrap();

        assert_eq!(
            draft.set_len(i64::MAX as u64 + 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(draft.len().unwrap(), 4);
        let mut bytes = [0; 4];
        assert_eq!(draft.read_at(0, &mut bytes).unwrap(), 4);
        assert_eq!(&bytes, b"keep");
    }

    #[test]
    fn read_at_beyond_supported_file_range_returns_eof() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"keep").unwrap();

        let mut bytes = [0; 4];
        assert_eq!(draft.read_at(u64::MAX, &mut bytes).unwrap(), 0);
        assert_eq!(draft.len().unwrap(), 4);
    }

    #[test]
    fn append_serializes_offset_selection_across_draft_handles() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        let first = store.begin_empty(identity()).unwrap();
        first.write_at(0, b"start").unwrap();
        let second = store.recover_item("drive", "file").unwrap().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..20 {
                    first.append(b"A").unwrap();
                }
            });
            scope.spawn(|| {
                for _ in 0..20 {
                    second.append(b"B").unwrap();
                }
            });
        });
        let mut bytes = vec![0; 45];
        assert_eq!(first.read_at(0, &mut bytes).unwrap(), 45);
        assert_eq!(&bytes[..5], b"start");
        assert_eq!(bytes[5..].iter().filter(|byte| **byte == b'A').count(), 20);
        assert_eq!(bytes[5..].iter().filter(|byte| **byte == b'B').count(), 20);
    }

    #[test]
    fn allocate_preserves_bytes_and_respects_keep_size() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"abc").unwrap();
        draft.allocate(16, 16, true).unwrap();
        assert_eq!(draft.len().unwrap(), 3);
        draft.allocate(8, 8, false).unwrap();
        assert_eq!(draft.len().unwrap(), 16);
        let mut bytes = [0_u8; 16];
        draft.read_at(0, &mut bytes).unwrap();
        assert_eq!(&bytes[..3], b"abc");
        assert_eq!(&bytes[3..], &[0; 13]);
        assert_eq!(
            draft.allocate(0, 0, false).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    fn identity() -> DraftIdentity {
        DraftIdentity {
            drive_id: "drive".into(),
            item_id: "file".into(),
            base_etag: "v1".into(),
            new_file: None,
        }
    }

    #[test]
    fn new_file_move_and_removal_survive_reopening_store() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        let new_file = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:one".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "first.txt".into(),
            }),
        };
        let draft = store.begin_empty(new_file).unwrap();
        draft.write_at(0, b"keep me").unwrap();
        store
            .relocate_new(
                "drive",
                "local:one",
                NewFileLocation {
                    parent_id: "other".into(),
                    name: "second.txt".into(),
                },
            )
            .unwrap();
        let reopened = DraftStore::open(root.path()).unwrap();
        let moved = reopened
            .recover_item("drive", "local:one")
            .unwrap()
            .unwrap();
        assert_eq!(
            moved.identity().new_file.as_ref().unwrap().parent_id,
            "other"
        );
        assert_eq!(
            moved.identity().new_file.as_ref().unwrap().name,
            "second.txt"
        );
        let mut content = [0; 7];
        assert_eq!(moved.read_at(0, &mut content).unwrap(), 7);
        assert_eq!(&content, b"keep me");
        reopened.remove_new("drive", "local:one").unwrap();
        assert!(
            DraftStore::open(root.path())
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remove_new_finishes_interrupted_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        let new_file = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:two".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "draft.txt".into(),
            }),
        };
        store.begin_empty(new_file.clone()).unwrap();
        let metadata = store.paths(&new_file).0;
        fs::rename(&metadata, metadata.with_extension("removed")).unwrap();
        assert!(store.paths(&new_file).1.exists());
        let reopened = DraftStore::open(root.path()).unwrap();
        assert!(reopened.list().unwrap().is_empty());
        assert!(!store.paths(&new_file).1.exists());
    }

    #[test]
    fn removed_draft_requires_explicit_detachment_for_open_handle_writes() {
        let root = tempfile::tempdir().unwrap();
        let store = DraftStore::open(root.path()).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:detached".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "deleted.txt".into(),
            }),
        };
        let draft = store.begin_empty(identity).unwrap();
        store.remove_new("drive", "local:detached").unwrap();
        assert_eq!(
            draft.write_at(0, b"hidden").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        draft.mark_detached();
        draft.write_at(0, b"hidden").unwrap();
        assert_eq!(draft.len().unwrap(), 6);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn new_file_names_must_be_accepted_by_onedrive() {
        assert!(NewFileLocation::valid_name("новый файл.txt"));
        for name in [
            "",
            ".",
            "..",
            "trailing.",
            "trailing ",
            "bad:name",
            "a/b",
            "a\\b",
            "line\n",
        ] {
            assert!(!NewFileLocation::valid_name(name), "{name:?}");
        }
    }

    #[test]
    fn upload_failure_survives_restart_and_clears_after_acknowledgement() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"local").unwrap();
        let failure = DraftUploadFailure {
            identity: identity(),
            issue: DraftUploadIssue::TransferFailed,
            attempted_at_unix: 123,
            http_status: Some(400),
        };
        store.save_upload_failure(&failure).unwrap();
        let path = store.failure_path(&identity());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let reopened = DraftStore::open(&root).unwrap();
        assert_eq!(
            reopened.upload_failure(&identity()).unwrap(),
            Some(failure.clone())
        );
        let mut old_record = serde_json::to_value(&failure).unwrap();
        old_record.as_object_mut().unwrap().remove("http_status");
        let decoded: DraftUploadFailure = serde_json::from_value(old_record).unwrap();
        assert_eq!(decoded.http_status, None);
        reopened.clear_upload_failure(&identity()).unwrap();
        assert!(reopened.upload_failure(&identity()).unwrap().is_none());
        assert_eq!(reopened.list().unwrap(), vec![identity()]);
        reopened.save_upload_failure(&failure).unwrap();
        let snapshot = reopened.snapshot(&identity()).unwrap().unwrap();
        assert!(reopened.acknowledge(&snapshot).unwrap());
        assert!(reopened.upload_failure(&identity()).unwrap().is_none());
    }

    #[test]
    fn export_preserves_draft_and_never_overwrites_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"local changes").unwrap();
        let destination = temporary.path().join("recovered.txt");
        store.export_item("drive", "file", &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"local changes");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(store.list().unwrap(), vec![identity()]);
        assert_eq!(
            store
                .export_item("drive", "file", &destination)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&destination).unwrap(), b"local changes");
        assert!(
            store
                .export_item("drive", "file", &root.join("unsafe.txt"))
                .is_err()
        );
        assert!(
            store
                .export_item("drive", "missing", &temporary.path().join("missing"))
                .is_err()
        );
    }

    #[test]
    fn uploaded_snapshot_only_removes_unchanged_draft() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"first").unwrap();
        let snapshot = store.snapshot(&identity()).unwrap().unwrap();
        assert_eq!(snapshot.size, 5);
        assert_eq!(fs::read(snapshot.file.path()).unwrap(), b"first");

        draft.write_at(0, b"later").unwrap();
        assert!(!store.acknowledge(&snapshot).unwrap());
        assert_eq!(store.list().unwrap(), vec![identity()]);
        assert_eq!(fs::read(snapshot.file.path()).unwrap(), b"first");

        let latest = store.snapshot(&identity()).unwrap().unwrap();
        assert!(store.acknowledge(&latest).unwrap());
        assert!(store.list().unwrap().is_empty());
        assert_eq!(
            draft.write_at(0, b"late").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn later_local_edits_rebase_on_uploaded_existing_version() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"first").unwrap();
        let snapshot = store.snapshot(&identity()).unwrap().unwrap();
        assert!(!store.rebase_existing_after_upload(&snapshot, "v2").unwrap());

        draft.write_at(0, b"later").unwrap();
        assert!(!store.acknowledge(&snapshot).unwrap());
        let failure = DraftUploadFailure {
            identity: identity(),
            issue: DraftUploadIssue::TransferFailed,
            attempted_at_unix: 123,
            http_status: Some(503),
        };
        store.save_upload_failure(&failure).unwrap();
        assert!(store.rebase_existing_after_upload(&snapshot, "v2").unwrap());

        let reopened = DraftStore::open(&root).unwrap();
        let current = reopened.recover_item("drive", "file").unwrap().unwrap();
        assert_eq!(current.identity().base_etag, "v2");
        let mut bytes = [0; 5];
        current.read_at(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"later");
        assert!(
            reopened
                .upload_failure(current.identity())
                .unwrap()
                .is_none()
        );
        assert!(
            !reopened
                .rebase_existing_after_upload(&snapshot, "v3")
                .unwrap()
        );
        assert_eq!(reopened.list().unwrap()[0].base_etag, "v2");
    }

    #[test]
    fn upload_acknowledgement_keeps_renamed_new_file_with_same_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "before.txt".into(),
            }),
        };
        let draft = store.begin_empty(identity.clone()).unwrap();
        draft.write_at(0, b"unchanged").unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        store
            .relocate_new(
                "drive",
                "local:new",
                NewFileLocation {
                    parent_id: "other-folder".into(),
                    name: "after.txt".into(),
                },
            )
            .unwrap();

        assert!(!store.acknowledge(&snapshot).unwrap());
        let reopened = DraftStore::open(&root).unwrap();
        let current = reopened
            .recover_item("drive", "local:new")
            .unwrap()
            .unwrap();
        assert_eq!(
            current.identity().new_file,
            Some(NewFileLocation {
                parent_id: "other-folder".into(),
                name: "after.txt".into(),
            })
        );
        let mut bytes = [0; 9];
        assert_eq!(current.read_at(0, &mut bytes).unwrap(), 9);
        assert_eq!(&bytes, b"unchanged");
    }

    #[test]
    fn snapshot_rejects_old_location_after_new_file_moves() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let original = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "before.txt".into(),
            }),
        };
        let draft = store.begin_empty(original.clone()).unwrap();
        draft.write_at(0, b"content").unwrap();
        store
            .relocate_new(
                &original.drive_id,
                &original.item_id,
                NewFileLocation {
                    parent_id: "other".into(),
                    name: "moved.txt".into(),
                },
            )
            .unwrap();

        assert!(store.snapshot(&original).unwrap().is_none());
        let current = store
            .recover_item(&original.drive_id, &original.item_id)
            .unwrap()
            .unwrap();
        let snapshot = store.snapshot(current.identity()).unwrap().unwrap();
        assert_eq!(snapshot.identity, *current.identity());
        assert_eq!(fs::read(snapshot.file.path()).unwrap(), b"content");
    }

    #[test]
    fn reopening_finishes_interrupted_uploaded_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let draft = store.begin_empty(identity()).unwrap();
        draft.write_at(0, b"uploaded").unwrap();
        let (metadata, data) = store.paths(&identity());
        let marker = metadata.with_extension("uploaded");
        store
            .save_upload_checkpoint(&DraftUploadCheckpoint {
                identity: identity(),
                sha256: "a".repeat(64),
                size: 8,
                upload_url: "https://upload.example/secret".into(),
                next_offset: 0,
                final_chunk_attempted: false,
            })
            .unwrap();
        fs::rename(&metadata, &marker).unwrap();
        drop(draft);
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        assert!(reopened.list().unwrap().is_empty());
        assert!(!data.exists());
        assert!(!marker.exists());
        assert!(reopened.upload_checkpoint(&identity()).unwrap().is_none());
        let fresh = reopened.begin_empty(identity()).unwrap();
        fresh.write_at(0, b"new").unwrap();
        assert_eq!(reopened.list().unwrap(), vec![identity()]);
    }

    #[test]
    fn reopening_finishes_cleanup_when_uploaded_data_is_already_gone() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        store.begin_empty(identity()).unwrap();
        let (metadata, data) = store.paths(&identity());
        let marker = metadata.with_extension("uploaded");
        fs::rename(&metadata, &marker).unwrap();
        fs::remove_file(data).unwrap();
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        assert!(reopened.list().unwrap().is_empty());
        assert!(!marker.exists());
        assert!(reopened.begin_empty(identity()).is_ok());
    }

    #[test]
    fn upload_checkpoint_survives_restart_and_remains_private() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        store.begin_empty(identity()).unwrap();
        let checkpoint = DraftUploadCheckpoint {
            identity: identity(),
            sha256: "a".repeat(64),
            size: 100,
            upload_url: "https://upload.example/secret".into(),
            next_offset: 50,
            final_chunk_attempted: false,
        };
        let mut legacy = serde_json::to_value(&checkpoint).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("final_chunk_attempted");
        let decoded: DraftUploadCheckpoint = serde_json::from_value(legacy).unwrap();
        assert!(!decoded.final_chunk_attempted);
        store.save_upload_checkpoint(&checkpoint).unwrap();
        assert!(!format!("{checkpoint:?}").contains("secret"));
        let path = store.checkpoint_path(&identity());
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        assert_eq!(
            reopened.upload_checkpoint(&identity()).unwrap(),
            Some(checkpoint)
        );
        reopened.clear_upload_checkpoint(&identity()).unwrap();
        assert!(reopened.upload_checkpoint(&identity()).unwrap().is_none());
    }

    #[test]
    fn unavailable_final_session_checkpoint_survives_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        store.begin_empty(identity()).unwrap();
        let mut checkpoint = DraftUploadCheckpoint {
            identity: identity(),
            sha256: "a".repeat(64),
            size: 3,
            upload_url: "https://upload.example/secret".into(),
            next_offset: 3,
            final_chunk_attempted: true,
        };
        store.save_upload_checkpoint(&checkpoint).unwrap();
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        assert_eq!(
            reopened.upload_checkpoint(&identity()).unwrap(),
            Some(checkpoint.clone())
        );
        checkpoint.final_chunk_attempted = false;
        assert_eq!(
            reopened
                .save_upload_checkpoint(&checkpoint)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        checkpoint.final_chunk_attempted = true;
        checkpoint.next_offset = 4;
        assert_eq!(
            reopened
                .save_upload_checkpoint(&checkpoint)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn empty_upload_checkpoint_requires_attempt_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        store.begin_empty(identity()).unwrap();
        let mut checkpoint = DraftUploadCheckpoint {
            identity: identity(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            size: 0,
            upload_url: String::new(),
            next_offset: 0,
            final_chunk_attempted: true,
        };
        store.save_upload_checkpoint(&checkpoint).unwrap();
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        assert_eq!(
            reopened.upload_checkpoint(&identity()).unwrap(),
            Some(checkpoint.clone())
        );
        checkpoint.final_chunk_attempted = false;
        assert_eq!(
            reopened
                .save_upload_checkpoint(&checkpoint)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        checkpoint.final_chunk_attempted = true;
        checkpoint.upload_url = "https://upload.example/unexpected".into();
        assert_eq!(
            reopened
                .save_upload_checkpoint(&checkpoint)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn renamed_new_file_rejects_stale_upload_progress_and_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let original = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "before.txt".into(),
            }),
        };
        store.begin_empty(original.clone()).unwrap();
        store
            .relocate_new(
                "drive",
                "local:new",
                NewFileLocation {
                    parent_id: "root".into(),
                    name: "after.txt".into(),
                },
            )
            .unwrap();
        let checkpoint = DraftUploadCheckpoint {
            identity: original.clone(),
            sha256: "a".repeat(64),
            size: 100,
            upload_url: "https://upload.example/old".into(),
            next_offset: 50,
            final_chunk_attempted: false,
        };
        let failure = DraftUploadFailure {
            identity: original.clone(),
            issue: DraftUploadIssue::TransferFailed,
            attempted_at_unix: 123,
            http_status: None,
        };
        assert_eq!(
            store
                .save_upload_checkpoint(&checkpoint)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            store.save_upload_failure(&failure).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        let reopened = DraftStore::open(&root).unwrap();
        let current = reopened
            .recover_item("drive", "local:new")
            .unwrap()
            .unwrap();
        assert_eq!(
            current.identity().new_file.as_ref().unwrap().name,
            "after.txt"
        );
        assert!(
            reopened
                .upload_checkpoint(current.identity())
                .unwrap()
                .is_none()
        );
        assert!(
            reopened
                .upload_failure(current.identity())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn new_file_with_uncertain_final_upload_cannot_be_moved_or_duplicated() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let original = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "first.txt".into(),
            }),
        };
        store
            .begin_empty(original.clone())
            .unwrap()
            .write_at(0, b"x")
            .unwrap();
        let mut checkpoint = DraftUploadCheckpoint {
            identity: original.clone(),
            sha256: "a".repeat(64),
            size: 1,
            upload_url: "https://upload.example/uncertain".into(),
            next_offset: 0,
            final_chunk_attempted: true,
        };
        store.save_upload_checkpoint(&checkpoint).unwrap();
        let moved = NewFileLocation {
            parent_id: "other".into(),
            name: "second.txt".into(),
        };

        assert_eq!(
            store
                .relocate_new("drive", "local:new", moved.clone())
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            store
                .recover_item("drive", "local:new")
                .unwrap()
                .unwrap()
                .identity(),
            &original
        );
        assert_eq!(
            store.upload_checkpoint(&original).unwrap(),
            Some(checkpoint.clone())
        );
        let reopened = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        assert_eq!(
            reopened
                .remove_new("drive", "local:new")
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            reopened.upload_checkpoint(&original).unwrap(),
            Some(checkpoint.clone())
        );
        assert!(
            reopened
                .recover_item("drive", "local:new")
                .unwrap()
                .is_some()
        );

        checkpoint.final_chunk_attempted = false;
        store.save_upload_checkpoint(&checkpoint).unwrap();
        store
            .relocate_new("drive", "local:new", moved.clone())
            .unwrap();
        let current = store.recover_item("drive", "local:new").unwrap().unwrap();
        assert_eq!(current.identity().new_file.as_ref(), Some(&moved));
        assert!(
            store
                .upload_checkpoint(current.identity())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn stale_upload_failure_does_not_hide_renamed_draft() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let original = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "before.txt".into(),
            }),
        };
        store.begin_empty(original.clone()).unwrap();
        let old_failure = DraftUploadFailure {
            identity: original.clone(),
            issue: DraftUploadIssue::TransferFailed,
            attempted_at_unix: 123,
            http_status: None,
        };
        store.save_upload_failure(&old_failure).unwrap();
        let failure_path = store.failure_path(&original);
        let previous_record = fs::read(&failure_path).unwrap();
        store
            .relocate_new(
                "drive",
                "local:new",
                NewFileLocation {
                    parent_id: "root".into(),
                    name: "after.txt".into(),
                },
            )
            .unwrap();
        // Simulate a failure record written by an older process after relocation.
        fs::write(&failure_path, previous_record).unwrap();

        let reopened = DraftStore::open(&root).unwrap();
        let current = reopened.list().unwrap().remove(0);
        assert_eq!(current.new_file.as_ref().unwrap().name, "after.txt");
        assert!(reopened.upload_failure(&current).unwrap().is_none());
        let latest_failure = DraftUploadFailure {
            identity: current.clone(),
            attempted_at_unix: 124,
            ..old_failure
        };
        reopened.save_upload_failure(&latest_failure).unwrap();
        assert_eq!(
            reopened.upload_failure(&current).unwrap(),
            Some(latest_failure)
        );
    }

    #[test]
    fn stale_upload_checkpoint_is_removed_after_renamed_draft_reopens() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let original = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "before.txt".into(),
            }),
        };
        store.begin_empty(original.clone()).unwrap();
        let old_checkpoint = DraftUploadCheckpoint {
            identity: original.clone(),
            sha256: "a".repeat(64),
            size: 100,
            upload_url: "https://upload.example/old-session".into(),
            next_offset: 50,
            final_chunk_attempted: false,
        };
        store.save_upload_checkpoint(&old_checkpoint).unwrap();
        let checkpoint_path = store.checkpoint_path(&original);
        let previous_record = fs::read(&checkpoint_path).unwrap();
        store
            .relocate_new(
                "drive",
                "local:new",
                NewFileLocation {
                    parent_id: "root".into(),
                    name: "after.txt".into(),
                },
            )
            .unwrap();
        // Recreate a checkpoint left by an older uploader after relocation.
        fs::write(&checkpoint_path, previous_record).unwrap();

        let reopened = DraftStore::open(&root).unwrap();
        let current = reopened.list().unwrap().remove(0);
        assert!(reopened.upload_checkpoint(&current).unwrap().is_none());
        assert!(!checkpoint_path.exists());
        let latest_checkpoint = DraftUploadCheckpoint {
            identity: current.clone(),
            upload_url: "https://upload.example/new-session".into(),
            ..old_checkpoint
        };
        reopened.save_upload_checkpoint(&latest_checkpoint).unwrap();
        assert_eq!(
            reopened.upload_checkpoint(&current).unwrap(),
            Some(latest_checkpoint)
        );
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
