use onedrive_mount::draft::{DraftSnapshot, DraftStore, DraftUploadCheckpoint};
use onedrive_sync::{
    QueuedOperation,
    upload::{CHUNK_SIZE, UploadError, UploadStep, UploadTransport},
};
use std::io;
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
    upload_operation_snapshot(
        transport,
        store,
        snapshot,
        operation,
        Some(&snapshot.identity.item_id),
    )
    .await
}

pub async fn upload_new_snapshot_to_parent<T: UploadTransport>(
    transport: &T,
    store: &DraftStore,
    snapshot: &DraftSnapshot,
    remote_parent_id: &str,
) -> Result<UploadStep, UploadError> {
    let location = snapshot
        .identity
        .new_file
        .as_ref()
        .ok_or(UploadError::InvalidResponse)?;
    let operation = QueuedOperation {
        id: 0,
        kind: "UploadNewByParent".to_owned(),
        path: location.name.clone(),
        from_path: None,
        remote_id: Some(remote_parent_id.to_owned()),
        expected_etag: None,
        local_fingerprint: Some(snapshot.sha256.clone()),
        attempts: 0,
    };
    upload_operation_snapshot(transport, store, snapshot, operation, None).await
}

async fn upload_operation_snapshot<T: UploadTransport>(
    transport: &T,
    store: &DraftStore,
    snapshot: &DraftSnapshot,
    operation: QueuedOperation,
    expected_id: Option<&str>,
) -> Result<UploadStep, UploadError> {
    ensure_snapshot_current(store, snapshot)?;
    if snapshot.size == 0 {
        if let Some(previous) = store.upload_checkpoint(&snapshot.identity)? {
            if previous.final_chunk_attempted {
                return Err(UploadError::Uncertain);
            }
            store.clear_upload_checkpoint(&snapshot.identity)?;
        }
        save_current_checkpoint(
            store,
            &DraftUploadCheckpoint {
                identity: snapshot.identity.clone(),
                sha256: snapshot.sha256.clone(),
                size: 0,
                upload_url: String::new(),
                next_offset: 0,
                final_chunk_attempted: true,
            },
        )?;
        ensure_snapshot_current(store, snapshot)?;
        let result = transport.upload_empty(&operation).await?;
        return match result {
            UploadStep::Complete { remote_id, etag }
                if expected_id.is_none_or(|expected| remote_id == expected)
                    && !remote_id.is_empty()
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
                    previous.final_chunk_attempted = false;
                    save_current_checkpoint(store, &previous)?;
                    previous
                }
                Ok(_) if previous.final_chunk_attempted => return Err(UploadError::Uncertain),
                Ok(_) => return Err(UploadError::InvalidResponse),
                Err(UploadError::Expired) => {
                    if previous.final_chunk_attempted {
                        return Err(UploadError::Uncertain);
                    }
                    store.clear_upload_checkpoint(&snapshot.identity)?;
                    create_checkpoint(transport, store, &operation, snapshot).await?
                }
                Err(error) => return Err(error),
            }
        } else {
            if previous.final_chunk_attempted {
                return Err(UploadError::Uncertain);
            }
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
        ensure_snapshot_current(store, snapshot)?;
        if offset + length as u64 == snapshot.size {
            checkpoint.final_chunk_attempted = true;
            save_current_checkpoint(store, &checkpoint)?;
        }
        match transport
            .send_range(&checkpoint.upload_url, offset, snapshot.size, bytes)
            .await?
        {
            UploadStep::More(next) if next == offset + length as u64 && next < snapshot.size => {
                checkpoint.next_offset = next;
                save_current_checkpoint(store, &checkpoint)?;
            }
            UploadStep::Complete { remote_id, etag }
                if offset + length as u64 == snapshot.size
                    && !remote_id.is_empty()
                    && expected_id.is_none_or(|expected| remote_id == expected)
                    && etag.as_ref().is_some_and(|etag| !etag.is_empty()) =>
            {
                return Ok(UploadStep::Complete { remote_id, etag });
            }
            _ => return Err(UploadError::InvalidResponse),
        }
    }
}

fn ensure_snapshot_current(
    store: &DraftStore,
    snapshot: &DraftSnapshot,
) -> Result<(), UploadError> {
    if store.current_identity_matches(&snapshot.identity)? {
        Ok(())
    } else {
        Err(UploadError::SourceChanged)
    }
}

fn save_current_checkpoint(
    store: &DraftStore,
    checkpoint: &DraftUploadCheckpoint,
) -> Result<(), UploadError> {
    store.save_upload_checkpoint(checkpoint).map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::WouldBlock
        ) {
            UploadError::SourceChanged
        } else {
            UploadError::Io(error)
        }
    })
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
        final_chunk_attempted: false,
    };
    save_current_checkpoint(store, &checkpoint)?;
    Ok(checkpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_mount::draft::{DraftIdentity, DraftStore, NewFileLocation};
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

    struct NewTransport(&'static str);

    impl UploadTransport for NewTransport {
        async fn create_session(
            &self,
            operation: &QueuedOperation,
            size: u64,
        ) -> Result<String, UploadError> {
            assert_eq!(operation.kind, "UploadNewByParent");
            assert_eq!(operation.remote_id.as_deref(), Some(self.0));
            assert_eq!(operation.path, "new.txt");
            assert!(operation.expected_etag.is_none());
            assert_eq!(size, 3);
            Ok("https://upload.example/new".into())
        }
        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            Ok(0)
        }
        async fn send_range(
            &self,
            _: &str,
            start: u64,
            total: u64,
            bytes: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            assert_eq!((start, total, bytes.as_slice()), (0, 3, b"abc".as_slice()));
            Ok(UploadStep::Complete {
                remote_id: "remote-created".into(),
                etag: Some("v1".into()),
            })
        }
    }

    struct EmptyNewTransport;

    impl UploadTransport for EmptyNewTransport {
        async fn create_session(&self, _: &QueuedOperation, _: u64) -> Result<String, UploadError> {
            panic!("empty file should not create an upload session")
        }
        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            panic!("empty file should not resume an upload session")
        }
        async fn send_range(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            panic!("empty file should not send a byte range")
        }
        async fn upload_empty(
            &self,
            operation: &QueuedOperation,
        ) -> Result<UploadStep, UploadError> {
            assert_eq!(operation.kind, "UploadNewByParent");
            assert_eq!(operation.remote_id.as_deref(), Some("remote-parent"));
            assert_eq!(operation.path, "new.txt");
            Ok(UploadStep::Complete {
                remote_id: "remote-created".into(),
                etag: Some("v1".into()),
            })
        }
    }

    #[tokio::test]
    async fn empty_new_virtual_file_uses_remote_parent_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = DraftStore::open(dir.path()).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:child".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "local-dir:parent".into(),
                name: "new.txt".into(),
            }),
        };
        store.begin_empty(identity.clone()).unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        assert_eq!(snapshot.size, 0);
        let result =
            upload_new_snapshot_to_parent(&EmptyNewTransport, &store, &snapshot, "remote-parent")
                .await
                .unwrap();
        assert!(
            matches!(result, UploadStep::Complete { remote_id, etag: Some(etag) }
            if remote_id == "remote-created" && etag == "v1")
        );
        let checkpoint = store.upload_checkpoint(&identity).unwrap().unwrap();
        assert_eq!(checkpoint.size, 0);
        assert!(checkpoint.final_chunk_attempted);
        assert_eq!(snapshot.identity, identity);
    }

    struct LostEmptyTransport(AtomicUsize);

    impl UploadTransport for LostEmptyTransport {
        async fn create_session(&self, _: &QueuedOperation, _: u64) -> Result<String, UploadError> {
            panic!("uncertain empty file must not create a session")
        }

        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            panic!("uncertain empty file must not resume a session")
        }

        async fn send_range(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            panic!("uncertain empty file must not send a range")
        }

        async fn upload_empty(&self, _: &QueuedOperation) -> Result<UploadStep, UploadError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(UploadError::Transient)
        }
    }

    #[tokio::test]
    async fn lost_empty_file_reply_does_not_repeat_create_after_restart() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:empty".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "new.txt".into(),
            }),
        };
        let draft = store.begin_empty(identity.clone()).unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        let transport = LostEmptyTransport(AtomicUsize::new(0));
        assert!(matches!(
            upload_new_snapshot_to_parent(&transport, &store, &snapshot, "root").await,
            Err(UploadError::Transient)
        ));
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        let checkpoint = reopened.upload_checkpoint(&identity).unwrap().unwrap();
        assert_eq!(checkpoint.size, 0);
        assert_eq!(checkpoint.sha256, snapshot.sha256);
        assert!(checkpoint.final_chunk_attempted);
        assert!(matches!(
            upload_new_snapshot_to_parent(&transport, &reopened, &snapshot, "root").await,
            Err(UploadError::Uncertain)
        ));
        draft.write_at(0, b"later").unwrap();
        let changed = reopened.snapshot(&identity).unwrap().unwrap();
        assert!(matches!(
            upload_new_snapshot_to_parent(&transport, &reopened, &changed, "root").await,
            Err(UploadError::Uncertain)
        ));
        assert_eq!(transport.0.load(Ordering::SeqCst), 1);
        assert!(reopened.recover(&identity).unwrap().is_some());
    }

    #[tokio::test]
    async fn new_virtual_file_upload_uses_parent_and_returns_remote_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&dir.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:one".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "parent".into(),
                name: "new.txt".into(),
            }),
        };
        store
            .begin_empty(identity.clone())
            .unwrap()
            .write_at(0, b"abc")
            .unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        let result =
            upload_new_snapshot_to_parent(&NewTransport("parent"), &store, &snapshot, "parent")
                .await
                .unwrap();
        assert!(
            matches!(result, UploadStep::Complete { remote_id, etag: Some(etag) }
            if remote_id == "remote-created" && etag == "v1")
        );
    }

    #[tokio::test]
    async fn moved_new_file_is_not_uploaded_from_stale_snapshot() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:one".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "parent".into(),
                name: "new.txt".into(),
            }),
        };
        store
            .begin_empty(identity.clone())
            .unwrap()
            .write_at(0, b"abc")
            .unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        store
            .relocate_new(
                &identity.drive_id,
                &identity.item_id,
                NewFileLocation {
                    parent_id: "other".into(),
                    name: "moved.txt".into(),
                },
            )
            .unwrap();

        assert!(matches!(
            upload_new_snapshot_to_parent(&NewTransport("parent"), &store, &snapshot, "parent")
                .await,
            Err(UploadError::SourceChanged)
        ));
    }

    struct RelocatingTransport<'a>(&'a DraftStore);

    impl UploadTransport for RelocatingTransport<'_> {
        async fn create_session(&self, _: &QueuedOperation, _: u64) -> Result<String, UploadError> {
            self.0.relocate_new(
                "drive",
                "local:one",
                NewFileLocation {
                    parent_id: "other".into(),
                    name: "moved.txt".into(),
                },
            )?;
            Ok("https://upload.example/session".into())
        }

        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            panic!("there is no previous session")
        }

        async fn send_range(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            panic!("moved file must not be sent")
        }
    }

    #[tokio::test]
    async fn move_during_session_creation_prevents_final_upload() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:one".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "parent".into(),
                name: "new.txt".into(),
            }),
        };
        store
            .begin_empty(identity.clone())
            .unwrap()
            .write_at(0, b"abc")
            .unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();

        assert!(matches!(
            upload_new_snapshot_to_parent(
                &RelocatingTransport(&store),
                &store,
                &snapshot,
                "parent"
            )
            .await,
            Err(UploadError::SourceChanged)
        ));
        assert!(store.upload_checkpoint(&identity).unwrap().is_none());
    }

    #[tokio::test]
    async fn nested_new_file_uses_remote_parent_without_changing_draft_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = DraftStore::open(dir.path()).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:child".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "local-dir:parent".into(),
                name: "new.txt".into(),
            }),
        };
        store
            .begin_empty(identity.clone())
            .unwrap()
            .write_at(0, b"abc")
            .unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        upload_new_snapshot_to_parent(&NewTransport("parent"), &store, &snapshot, "parent")
            .await
            .unwrap();
        assert_eq!(snapshot.identity, identity);
        assert!(store.acknowledge(&snapshot).unwrap());
        assert!(store.recover(&identity).unwrap().is_none());
    }

    #[tokio::test]
    async fn upload_uses_snapshot_and_rejects_wrong_remote_id() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "item".into(),
            base_etag: "old".into(),
            new_file: None,
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
            new_file: None,
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
        assert!(
            store
                .upload_checkpoint(&identity)
                .unwrap()
                .unwrap()
                .final_chunk_attempted
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

    struct LostFinalTransport {
        sessions: AtomicUsize,
        ranges: AtomicUsize,
    }

    impl UploadTransport for LostFinalTransport {
        async fn create_session(&self, _: &QueuedOperation, _: u64) -> Result<String, UploadError> {
            self.sessions.fetch_add(1, Ordering::SeqCst);
            Ok("https://upload.example/uncertain".into())
        }

        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            Err(UploadError::Expired)
        }

        async fn send_range(
            &self,
            _: &str,
            _: u64,
            _: u64,
            _: Vec<u8>,
        ) -> Result<UploadStep, UploadError> {
            self.ranges.fetch_add(1, Ordering::SeqCst);
            Err(UploadError::Transient)
        }
    }

    #[tokio::test]
    async fn lost_final_reply_does_not_create_a_second_new_file_session() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("drafts");
        let store = DraftStore::open(&root).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "local:new".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "root".into(),
                name: "new.txt".into(),
            }),
        };
        let draft = store.begin_empty(identity.clone()).unwrap();
        draft.write_at(0, b"first").unwrap();
        let snapshot = store.snapshot(&identity).unwrap().unwrap();
        let transport = LostFinalTransport {
            sessions: AtomicUsize::new(0),
            ranges: AtomicUsize::new(0),
        };
        assert!(matches!(
            upload_new_snapshot_to_parent(&transport, &store, &snapshot, "root").await,
            Err(UploadError::Transient)
        ));
        assert!(
            store
                .upload_checkpoint(&identity)
                .unwrap()
                .unwrap()
                .final_chunk_attempted
        );
        drop(store);

        let reopened = DraftStore::open(&root).unwrap();
        assert!(matches!(
            upload_new_snapshot_to_parent(&transport, &reopened, &snapshot, "root").await,
            Err(UploadError::Uncertain)
        ));
        draft.write_at(0, b"later").unwrap();
        let changed = reopened.snapshot(&identity).unwrap().unwrap();
        assert!(matches!(
            upload_new_snapshot_to_parent(&transport, &reopened, &changed, "root").await,
            Err(UploadError::Uncertain)
        ));
        assert_eq!(transport.sessions.load(Ordering::SeqCst), 1);
        assert_eq!(transport.ranges.load(Ordering::SeqCst), 1);
        assert!(reopened.recover(&identity).unwrap().is_some());
    }

    #[tokio::test]
    async fn changed_draft_starts_a_new_session() {
        let temporary = tempfile::tempdir().unwrap();
        let store = DraftStore::open(&temporary.path().join("drafts")).unwrap();
        let identity = DraftIdentity {
            drive_id: "drive".into(),
            item_id: "item".into(),
            base_etag: "old".into(),
            new_file: None,
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
                final_chunk_attempted: false,
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
            new_file: None,
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
                final_chunk_attempted: false,
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
        let checkpoint = store.upload_checkpoint(&identity).unwrap().unwrap();
        assert_eq!(checkpoint.size, 0);
        assert!(checkpoint.final_chunk_attempted);
        store.clear_upload_checkpoint(&identity).unwrap();
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
