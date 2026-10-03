//! Resumable upload state machine. A transport performs network requests; this
//! module snapshots the source and commits progress only after server replies.

use crate::{QueuedOperation, SyncError, SyncStore, UploadCheckpoint};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub const CHUNK_SIZE: u64 = 5 * 1024 * 1024;

#[derive(Debug)]
pub enum UploadError {
    Io(io::Error),
    Store(SyncError),
    SourceChanged,
    InvalidResponse,
    Transient,
    Conflict,
    Expired,
}

impl From<io::Error> for UploadError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<SyncError> for UploadError {
    fn from(value: SyncError) -> Self {
        Self::Store(value)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UploadStep {
    More(u64),
    Complete {
        remote_id: String,
        etag: Option<String>,
    },
}

/// Implementations must not attach bearer tokens to preauthenticated session URLs.
#[allow(async_fn_in_trait)]
pub trait UploadTransport {
    async fn create_session(
        &self,
        operation: &QueuedOperation,
        size: u64,
    ) -> Result<String, UploadError>;
    async fn session_offset(&self, upload_url: &str) -> Result<u64, UploadError>;
    async fn send_range(
        &self,
        upload_url: &str,
        start: u64,
        total: u64,
        bytes: Vec<u8>,
    ) -> Result<UploadStep, UploadError>;
}

pub async fn upload_queued<T: UploadTransport>(
    store: &SyncStore,
    transport: &T,
    root: &Path,
    operation: &QueuedOperation,
) -> Result<UploadStep, UploadError> {
    if !matches!(operation.kind.as_str(), "UploadNew" | "UploadChanged") {
        return Err(UploadError::InvalidResponse);
    }
    let root = root.to_path_buf();
    let relative = operation.path.clone();
    let (snapshot, fingerprint, size) =
        tokio::task::spawn_blocking(move || snapshot_source(&root, &relative))
            .await
            .map_err(|_| UploadError::InvalidResponse)??;
    if size == 0 || operation.local_fingerprint.as_deref() != Some(&fingerprint) {
        return Err(UploadError::SourceChanged);
    }
    let mut checkpoint = if let Some(previous) = store.upload_checkpoint(operation.id)? {
        if previous.fingerprint != fingerprint || previous.total_size != size {
            return Err(UploadError::SourceChanged);
        }
        match transport.session_offset(&previous.upload_url).await {
            Ok(offset) if offset < size => {
                store.set_upload_offset_from_server(operation.id, offset)?;
                UploadCheckpoint {
                    next_offset: offset,
                    ..previous
                }
            }
            Ok(_) => return Err(UploadError::InvalidResponse),
            Err(UploadError::Expired) => {
                store.clear_upload_checkpoint(operation.id)?;
                create_checkpoint(store, transport, operation, size, fingerprint.clone()).await?
            }
            Err(error) => return Err(error),
        }
    } else {
        create_checkpoint(store, transport, operation, size, fingerprint.clone()).await?
    };
    let mut file = tokio::fs::File::from_std(snapshot.reopen()?);
    loop {
        let start = checkpoint.next_offset;
        let amount = usize::try_from((size - start).min(CHUNK_SIZE))
            .map_err(|_| UploadError::InvalidResponse)?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut bytes = vec![0_u8; amount];
        file.read_exact(&mut bytes).await?;
        match transport
            .send_range(&checkpoint.upload_url, start, size, bytes)
            .await?
        {
            UploadStep::More(next) if next > start && next < size => {
                store.update_upload_offset(operation.id, next)?;
                checkpoint.next_offset = next;
            }
            UploadStep::Complete { remote_id, etag }
                if !remote_id.is_empty() && start + amount as u64 == size =>
            {
                return Ok(UploadStep::Complete { remote_id, etag });
            }
            _ => return Err(UploadError::InvalidResponse),
        }
    }
}

async fn create_checkpoint<T: UploadTransport>(
    store: &SyncStore,
    transport: &T,
    operation: &QueuedOperation,
    size: u64,
    fingerprint: String,
) -> Result<UploadCheckpoint, UploadError> {
    let url = transport.create_session(operation, size).await?;
    let checkpoint = UploadCheckpoint {
        operation_id: operation.id,
        upload_url: url,
        total_size: size,
        fingerprint,
        next_offset: 0,
    };
    store.save_upload_checkpoint(&checkpoint)?;
    Ok(checkpoint)
}

fn snapshot_source(root: &Path, relative: &str) -> io::Result<(NamedTempFile, String, u64)> {
    let relative = Path::new(relative);
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid relative path",
        ));
    }
    let root = root.canonicalize()?;
    let mut source_path = PathBuf::from(&root);
    for component in relative.components() {
        source_path.push(component);
        if fs::symlink_metadata(&source_path)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "symbolic link in source path",
            ));
        }
    }
    if !source_path.canonicalize()?.starts_with(&root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source escapes sync root",
        ));
    }
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source_path)?;
    let before = source.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a file",
        ));
    }
    let mut snapshot = NamedTempFile::new()?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        snapshot.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    let after = source.metadata()?;
    if before.len() != after.len() || before.modified()? != after.modified()? || after.len() != size
    {
        return Err(io::Error::other("source changed during snapshot"));
    }
    snapshot.as_file().sync_all()?;
    Ok((snapshot, format!("{:x}", hasher.finalize()), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, PlannedOperation, scan_local};
    use std::sync::Mutex;

    struct FakeTransport {
        state: Mutex<(u64, bool, usize)>,
    }

    impl UploadTransport for FakeTransport {
        async fn create_session(&self, _: &QueuedOperation, _: u64) -> Result<String, UploadError> {
            Ok("https://upload.example/session?secret=hidden".into())
        }
        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            Ok(self.state.lock().unwrap().0)
        }
        async fn send_range(
            &self,
            _: &str,
            start: u64,
            total: u64,
            bytes: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            let mut state = self.state.lock().unwrap();
            assert_eq!(start, state.0);
            state.2 += 1;
            if state.2 == 2 && !state.1 {
                state.1 = true;
                return Err(UploadError::Transient);
            }
            state.0 += bytes.len() as u64;
            if state.0 == total {
                Ok(UploadStep::Complete {
                    remote_id: "item".into(),
                    etag: Some("v2".into()),
                })
            } else {
                Ok(UploadStep::More(state.0))
            }
        }
    }

    #[tokio::test]
    async fn interrupted_upload_resumes_from_server_offset() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("large.bin"), vec![7_u8; CHUNK_SIZE as usize + 3]).unwrap();
        let fingerprint = scan_local(&root).unwrap().0[0].fingerprint.clone();
        let path = directory.path().join("sync.sqlite3");
        let transport = FakeTransport {
            state: Mutex::new((0, false, 0)),
        };
        let id;
        {
            let mut store = SyncStore::open(&path).unwrap();
            id = store
                .enqueue(&PlannedOperation {
                    action: Action::UploadChanged,
                    path: "large.bin".into(),
                    from_path: None,
                    remote_id: Some("item".into()),
                    expected_etag: Some("v1".into()),
                    local_fingerprint: Some(fingerprint),
                    requires_confirmation: false,
                })
                .unwrap();
            let operation = store.claim_next().unwrap().unwrap();
            assert!(matches!(
                upload_queued(&store, &transport, &root, &operation).await,
                Err(UploadError::Transient)
            ));
            assert_eq!(
                store.upload_checkpoint(id).unwrap().unwrap().next_offset,
                CHUNK_SIZE
            );
        }
        let mut store = SyncStore::open(&path).unwrap();
        let operation = store.claim_next().unwrap().unwrap();
        assert_eq!(operation.attempts, 2);
        let result = upload_queued(&store, &transport, &root, &operation)
            .await
            .unwrap();
        assert_eq!(
            result,
            UploadStep::Complete {
                remote_id: "item".into(),
                etag: Some("v2".into())
            }
        );
        assert_eq!(
            store.upload_checkpoint(id).unwrap().unwrap().next_offset,
            CHUNK_SIZE
        );
    }

    #[tokio::test]
    async fn changed_source_does_not_create_session() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file"), b"changed").unwrap();
        let mut store = SyncStore::in_memory().unwrap();
        store
            .enqueue(&PlannedOperation {
                action: Action::UploadNew,
                path: "file".into(),
                from_path: None,
                remote_id: None,
                expected_etag: None,
                local_fingerprint: Some("old".into()),
                requires_confirmation: false,
            })
            .unwrap();
        let operation = store.claim_next().unwrap().unwrap();
        let transport = FakeTransport {
            state: Mutex::new((0, false, 0)),
        };
        assert!(matches!(
            upload_queued(&store, &transport, directory.path(), &operation).await,
            Err(UploadError::SourceChanged)
        ));
        assert!(store.upload_checkpoint(operation.id).unwrap().is_none());
    }
}
