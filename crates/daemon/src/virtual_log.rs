//! Bounded, private diagnostics for virtual-drive uploads.

use onedrive_mount::draft::DraftIdentity;
use onedrive_sync::upload::UploadError;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub enum UploadEvent<'a> {
    Started { size: u64, new_file: bool },
    Failed(&'a UploadError),
    GraphConfirmed,
    Completed { local_changes_remain: bool },
}

fn draft_tag(identity: &DraftIdentity) -> String {
    let mut hash = Sha256::new();
    for part in [&identity.drive_id, &identity.item_id] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())[..16].to_owned()
}

fn safe_phase(phase: &str) -> &'static str {
    match phase {
        "create session" => "create_session",
        "query session" => "query_session",
        "send bytes" => "send_bytes",
        "create empty file" => "create_empty_file",
        "replace with empty file" => "replace_with_empty_file",
        _ => "unknown",
    }
}

fn safe_code(code: &str) -> Option<&str> {
    (code.len() <= 64
        && !code.is_empty()
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    .then_some(code)
}

fn event_text(event: UploadEvent<'_>) -> String {
    match event {
        UploadEvent::Started { size, new_file } => {
            format!(
                "event=start kind={} size={size}",
                if new_file { "new" } else { "update" }
            )
        }
        UploadEvent::GraphConfirmed => "event=graph_confirmed".to_owned(),
        UploadEvent::Completed {
            local_changes_remain,
        } => format!("event=complete local_changes_remain={local_changes_remain}"),
        UploadEvent::Failed(error) => match error {
            UploadError::HttpStatus {
                status,
                code,
                phase,
            } => {
                let mut line = format!("event=failed phase={} http={status}", safe_phase(phase));
                if let Some(code) = code.as_deref().and_then(safe_code) {
                    line.push_str(&format!(" graph_code={code}"));
                }
                line
            }
            UploadError::Conflict => "event=failed reason=conflict".to_owned(),
            UploadError::Expired => "event=failed reason=session_expired".to_owned(),
            UploadError::Transient => "event=failed reason=transient".to_owned(),
            UploadError::InvalidResponse => "event=failed reason=invalid_response".to_owned(),
            UploadError::SourceChanged => "event=failed reason=source_changed".to_owned(),
            UploadError::Uncertain => "event=failed reason=uncertain".to_owned(),
            UploadError::Io(_) | UploadError::Store(_) => {
                "event=failed reason=local_error".to_owned()
            }
        },
    }
}

pub fn append(root: &Path, identity: &DraftIdentity, event: UploadEvent<'_>) -> io::Result<()> {
    let path = root.join("virtual-upload.log");
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe upload log",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!(
        "{timestamp} draft={} {}\n",
        draft_tag(identity),
        event_text(event)
    );
    if file.metadata()?.len().saturating_add(line.len() as u64) > MAX_LOG_BYTES {
        file.set_len(0)?;
    }
    file.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use onedrive_mount::draft::NewFileLocation;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn identity() -> DraftIdentity {
        DraftIdentity {
            drive_id: "private-drive".into(),
            item_id: "private-item".into(),
            base_etag: "local-new".into(),
            new_file: Some(NewFileLocation {
                parent_id: "private-parent".into(),
                name: "secret.txt".into(),
            }),
        }
    }

    #[test]
    fn log_records_safe_upload_diagnostics_without_names_or_urls() {
        let dir = tempfile::tempdir().unwrap();
        let draft = identity();
        append(
            dir.path(),
            &draft,
            UploadEvent::Started {
                size: 7,
                new_file: true,
            },
        )
        .unwrap();
        append(
            dir.path(),
            &draft,
            UploadEvent::Failed(&UploadError::HttpStatus {
                status: 400,
                code: Some("invalidRequest".into()),
                phase: "create session",
            }),
        )
        .unwrap();
        let path = dir.path().join("virtual-upload.log");
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.contains("kind=new size=7"));
        assert!(log.contains("phase=create_session http=400 graph_code=invalidRequest"));
        for private in [
            "secret.txt",
            "private-drive",
            "private-item",
            "private-parent",
        ] {
            assert!(!log.contains(private));
        }
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn log_rejects_symlink_and_drops_oversized_history() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::write(&outside, "keep").unwrap();
        let path = dir.path().join("virtual-upload.log");
        symlink(&outside, &path).unwrap();
        assert!(append(dir.path(), &identity(), UploadEvent::GraphConfirmed).is_err());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize]).unwrap();
        append(dir.path(), &identity(), UploadEvent::GraphConfirmed).unwrap();
        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.contains("event=graph_confirmed"));
        assert!(!log.contains('x'));
    }
}
