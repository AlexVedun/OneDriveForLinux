use onedrive_mount::draft::DraftSnapshot;
use onedrive_sync::{
    QueuedOperation,
    upload::{CHUNK_SIZE, UploadError, UploadStep, UploadTransport},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Upload an immutable local copy. The caller must check the live remote eTag
/// before this call and acknowledge the draft only after a verified response.
pub async fn upload_snapshot<T: UploadTransport>(
    transport: &T,
    snapshot: &DraftSnapshot,
) -> Result<UploadStep, UploadError> {
    if snapshot.size == 0 {
        return Err(UploadError::InvalidResponse);
    }
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
    let url = transport.create_session(&operation, snapshot.size).await?;
    let mut file = tokio::fs::File::from_std(snapshot.file.reopen()?);
    let mut offset = 0_u64;
    loop {
        let length = (snapshot.size - offset).min(CHUNK_SIZE) as usize;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).await?;
        match transport
            .send_range(&url, offset, snapshot.size, bytes)
            .await?
        {
            UploadStep::More(next) if next == offset + length as u64 && next < snapshot.size => {
                offset = next;
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

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_mount::draft::{DraftIdentity, DraftStore};
    use std::sync::Mutex;

    struct FakeTransport {
        expected_id: String,
        sent: Mutex<Vec<u8>>,
    }

    impl UploadTransport for FakeTransport {
        async fn create_session(
            &self,
            operation: &QueuedOperation,
            size: u64,
        ) -> Result<String, UploadError> {
            assert_eq!(operation.kind, "UploadChanged");
            assert_eq!(operation.remote_id.as_deref(), Some("item"));
            assert_eq!(operation.expected_etag.as_deref(), Some("old"));
            assert_eq!(size, CHUNK_SIZE + 1);
            Ok("https://upload.example/session".into())
        }

        async fn session_offset(&self, _: &str) -> Result<u64, UploadError> {
            unreachable!()
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
        };
        assert!(matches!(
            upload_snapshot(&transport, &snapshot).await.unwrap(),
            UploadStep::Complete { .. }
        ));
        assert_eq!(transport.sent.lock().unwrap().last(), Some(&b'a'));
        assert!(!store.acknowledge(&snapshot).unwrap());

        let wrong = FakeTransport {
            expected_id: "other".into(),
            sent: Mutex::new(Vec::new()),
        };
        assert!(matches!(
            upload_snapshot(&wrong, &snapshot).await,
            Err(UploadError::InvalidResponse)
        ));
    }
}
