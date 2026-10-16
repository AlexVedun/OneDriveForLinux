use onedrive_mount::draft::{DraftSnapshot, DraftStore, DraftUploadCheckpoint};
use onedrive_sync::{
    QueuedOperation,
    upload::{CHUNK_SIZE, UploadError, UploadStep, UploadTransport},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Upload an immutable local copy. The caller must check the live remote eTag
/// before this call and acknowledge the draft only after a verified response.
pub async fn upload_snapshot<T: UploadTransport>(
    transport: &T,
    store: &DraftStore,
    snapshot: &DraftSnapshot,
) -> Result<UploadStep, UploadError> {
    let operation = QueuedOperation {
        id: 0,
        kind: "UploadChanged".to_owned(),
        path: String::new(),
        from_path: None,
        remote_id: Some(snapshot.identity.item_id.clone()),
        expected_etag: Some(snapshot.identity.base_etag.clone()),
        local_fingerprint: Some(snapshot.sha256.clone()),
        attempts: 0,
    };
    if snapshot.size == 0 {
        store.clear_upload_checkpoint(&snapshot.identity)?;
        let result = transport.upload_empty(&operation).await?;
        return match result {
            UploadStep::Complete { remote_id, etag }
                if remote_id == snapshot.identity.item_id
                    && etag.as_ref().is_some_and(|etag| !etag.is_empty()) =>
            {
                Ok(UploadStep::Complete { remote_id, etag })
            }
            _ => Err(UploadError::InvalidResponse),
        };
    }
    let previous = store.upload_checkpoint(&snapshot.identity)?;
    let mut checkpoint = if let Some(mut previous) = previous {
        if previous.sha256 == snapshot.sha256
            && previous.size == snapshot.size
            && previous.identity == snapshot.identity
        {
            match transport.session_offset(&previous.upload_url).await {
                Ok(offset) if offset < snapshot.size => {
                    previous.next_offset = offset;
                    store.save_upload_checkpoint(&previous)?;
                    previous
                }
                Ok(_) => return Err(UploadError::InvalidResponse),
                Err(UploadError::Expired) => {
                    store.clear_upload_checkpoint(&snapshot.identity)?;
                    create_checkpoint(transport, store, &operation, snapshot).await?
                }
                Err(error) => return Err(error),
            }
        } else {
            store.clear_upload_checkpoint(&snapshot.identity)?;
            create_checkpoint(transport, store, &operation, snapshot).await?
        }
    } else {
        create_checkpoint(transport, store, &operation, snapshot).await?
    };
    let mut file = tokio::fs::File::from_std(snapshot.file.reopen()?);
    loop {
        let offset = checkpoint.next_offset;
        let length = (snapshot.size - offset).min(CHUNK_SIZE) as usize;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).await?;
        match transport
            .send_range(&checkpoint.upload_url, offset, snapshot.size, bytes)
            .await?
        {
            UploadStep::More(next) if next == offset + length as u64 && next < snapshot.size => {
                checkpoint.next_offset = next;
                store.save_upload_checkpoint(&checkpoint)?;
            }
            UploadStep::Complete { remote_id, etag }
                if offset + length as u64 == snapshot.size
                    && remote_id == snapshot.identity.item_id
                    && etag.as_ref().is_some_and(|etag| !etag.is_empty()) =>
            {
                return Ok(UploadStep::Complete { remote_id, etag });
            }
            _ => return Err(UploadError::InvalidResponse),
        }
    }
}

async fn create_checkpoint<T: UploadTransport>(
    transport: &T,
    store: &DraftStore,
    operation: &QueuedOperation,
    snapshot: &DraftSnapshot,
) -> Result<DraftUploadCheckpoint, UploadError> {
    let checkpoint = DraftUploadCheckpoint {
        identity: snapshot.identity.clone(),
        sha256: snapshot.sha256.clone(),
        size: snapshot.size,
        upload_url: transport.create_session(operation, snapshot.size).await?,
        next_offset: 0,
    };
    store.save_upload_checkpoint(&checkpoint)?;
    Ok(checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_mount::draft::{DraftIdentity, DraftStore};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct FakeTransport {
        expected_id: String,
        sent: Mutex<Vec<u8>>,
        fail_final_once: Mutex<bool>,
        sessions: AtomicUsize,
    }

    impl UploadTransport for FakeTransport {
        async fn create_session(
            &self,
            operation: &QueuedOperation,
            size: u64,
        ) -> Result<String, UploadError> {
            self.sessions.fetch_add(1, Ordering::SeqCst);
            assert_eq!(operation.kind, "UploadChanged");
            assert_eq!(operation.remote_id.as_deref(), Some("item"));
            assert_eq!(operation.expected_etag.as_deref(), Some("old"));
            assert_eq!(size, CHUNK_SIZE + 1);
            Ok("https://upload.example/session".into())
        }

        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            Ok(self.sent.lock().unwrap().len() as u64)
        }

        async fn send_range(
            &self,
            _: &str,
            start: u64,
            total: u64,
            bytes: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            let mut sent = self.sent.lock().unwrap();
            assert_eq!(start, sent.len() as u64);
            let mut fail_final_once = self.fail_final_once.lock().unwrap();
            if start + bytes.len() as u64 == total && *fail_final_once {
                *fail_final_once = false;
                return Err(UploadError::Transient);
            }
            drop(fail_final_once);
            sent.extend(bytes);
            if sent.len() as u64 == total {
                Ok(UploadStep::Complete {
                    remote_id: self.expected_id.clone(),
                    etag: Some("new".into()),
                })
            } else {
                Ok(UploadStep::More(sent.len() as u64))
            }
        }
    }

    #[tokio::test]
    async fn upload_uses_snapshot_and_rejects_wrong_remote_id() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "item".into(),
            base_etag: "old".into(),
        };
        let draft = store.begin_empty(identity.clone()).unwrap();
        draft.set_len(CHUNK_SIZE + 1).unwrap();
        draft.write_at(CHUNK_SIZE, b"a").unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        draft.write_at(CHUNK_SIZE, b"b").unwrap();

        let transport = FakeTransport {
            expected_id: "item".into(),
            sent: Mutex::new(Vec::new()),
            fail_final_once: Mutex::new(false),
            sessions: AtomicUsize::new(0),
        };
        assert!(matches!(
            upload_snapshot(&transport, &store, &snapshot)
                .await
                .unwrap(),
            UploadStep::Complete { .. }
        ));
        assert_eq!(transport.sent.lock().unwrap().last(), Some(&b'a'));
        assert!(!store.acknowledge(&snapshot).unwrap());
        store.clear_upload_checkpoint(&identity).unwrap();

        let wrong = FakeTransport {
            expected_id: "other".into(),
            sent: Mutex::new(Vec::new()),
            fail_final_once: Mutex::new(false),
            sessions: AtomicUsize::new(0),
        };
        assert!(matches!(
            upload_snapshot(&wrong, &store, &snapshot).await,
            Err(UploadError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn interrupted_transfer_resumes_from_server_offset_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "item".into(),
            base_etag: "old".into(),
        };
        let draft = store.begin_empty(identity.clone()).unwrap();
        draft.set_len(CHUNK_SIZE + 1).unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        let transport = FakeTransport {
            expected_id: "item".into(),
            sent: Mutex::new(Vec::new()),
            fail_final_once: Mutex::new(true),
            sessions: AtomicUsize::new(0),
        };
        assert!(matches!(
            upload_snapshot(&transport, &store, &snapshot).await,
            Err(UploadError::Transient)
        ));
        assert_eq!(
            store
                .upload_checkpoint(&identity)
                .unwrap()
                .unwrap()
                .next_offset,
            CHUNK_SIZE
        );
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        assert!(matches!(
            upload_snapshot(&transport, &reopened, &snapshot).await,
            Ok(UploadStep::Complete { .. })
        ));
        assert_eq!(transport.sessions.load(Ordering::SeqCst), 1);
        assert_eq!(transport.sent.lock().unwrap().len() as u64, CHUNK_SIZE + 1);
    }

    #[tokio::test]
    async fn changed_draft_starts_a_new_session() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "item".into(),
            base_etag: "old".into(),
        };
        let draft = store.begin_empty(identity.clone()).unwrap();
        draft.set_len(CHUNK_SIZE + 1).unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        store
            .save_upload_checkpoint(&DraftUploadCheckpoint {
                identity: identity.clone(),
                sha256: "f".repeat(64),
                size: snapshot.size,
                upload_url: "https://upload.example/old".into(),
                next_offset: CHUNK_SIZE,
            })
            .unwrap();
        let transport = FakeTransport {
            expected_id: "item".into(),
            sent: Mutex::new(Vec::new()),
            fail_final_once: Mutex::new(false),
            sessions: AtomicUsize::new(0),
        };
        assert!(matches!(
            upload_snapshot(&transport, &store, &snapshot).await,
            Ok(UploadStep::Complete { .. })
        ));
        assert_eq!(transport.sessions.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.upload_checkpoint(&identity).unwrap().unwrap().sha256,
            snapshot.sha256
        );
    }

    struct EmptyTransport {
        remote_id: String,
    }

    impl UploadTransport for EmptyTransport {
        async fn create_session(&self, _: &QueuedOperation, _: u64) -> Result<String, UploadError> {
            unreachable!()
        }

        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            unreachable!()
        }

        async fn send_range(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            unreachable!()
        }

        async fn upload_empty(
            &self,
            operation: &QueuedOperation,
        ) -> Result<UploadStep, UploadError> {
            assert_eq!(operation.kind, "UploadChanged");
            assert_eq!(operation.expected_etag.as_deref(), Some("old"));
            Ok(UploadStep::Complete {
                remote_id: self.remote_id.clone(),
                etag: Some("new".into()),
            })
        }
    }

    #[tokio::test]
    async fn empty_draft_uses_direct_upload_and_discards_old_session() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "item".into(),
            base_etag: "old".into(),
        };
        store.begin_empty(identity.clone()).unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        assert_eq!(snapshot.size, 0);
        store
            .save_upload_checkpoint(&DraftUploadCheckpoint {
                identity: identity.clone(),
                sha256: "f".repeat(64),
                size: 1,
                upload_url: "https://upload.example/old".into(),
                next_offset: 0,
            })
            .unwrap();
        assert!(matches!(
            upload_snapshot(
                &EmptyTransport {
                    remote_id: "item".into()
                },
                &store,
                &snapshot,
            )
            .await,
            Ok(UploadStep::Complete { .. })
        ));
        assert!(store.upload_checkpoint(&identity).unwrap().is_none());
        assert!(matches!(
            upload_snapshot(
                &EmptyTransport {
                    remote_id: "wrong".into()
                },
                &store,
                &snapshot,
            )
            .await,
            Err(UploadError::InvalidResponse)
        ));
    }
}
