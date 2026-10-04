//! Atomic replacement for read-only downloads. A partial transfer never
//! replaces the last good local file.

use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};
use tempfile::NamedTempFile;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub struct AtomicDownload {
    temporary: NamedTempFile,
    file: File,
    written: u64,
    hasher: Sha256,
}

impl AtomicDownload {
    pub fn new(target: &Path) -> io::Result<Self> {
        let directory = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "target has no parent directory",
            )
        })?;
        match target.symlink_metadata() {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "target must be a regular file",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let temporary = NamedTempFile::new_in(directory)?;
        let file = File::from_std(temporary.reopen()?);
        Ok(Self {
            temporary,
            file,
            written: 0,
            hasher: Sha256::new(),
        })
    }

    pub async fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.file.write_all(chunk).await?;
        self.hasher.update(chunk);
        self.written = self
            .written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "download is too large"))?;
        Ok(())
    }

    pub async fn finish(mut self, target: &Path, expected_size: Option<u64>) -> io::Result<()> {
        if expected_size.is_some_and(|size| size != self.written) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "download size does not match catalog",
            ));
        }
        self.file.flush().await?;
        self.file.sync_all().await?;
        drop(self.file);
        self.temporary
            .persist(target)
            .map_err(|error| error.error)?;
        Ok(())
    }

    pub fn new_queued(root: &Path, target: &Path, changed: bool) -> io::Result<Self> {
        validate_queued_target(root, target, changed)?;
        Self::new(target)
    }

    pub async fn finish_queued(
        mut self,
        root: &Path,
        target: &Path,
        expected_size: u64,
        expected_local: Option<&str>,
    ) -> io::Result<String> {
        if self.written != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "download size changed",
            ));
        }
        self.file.flush().await?;
        self.file.sync_all().await?;
        drop(self.file);
        validate_queued_target(root, target, expected_local.is_some())?;
        if let Some(expected) = expected_local {
            if fingerprint_file(target)? != expected {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "local file changed",
                ));
            }
            self.temporary
                .persist(target)
                .map_err(|error| error.error)?;
        } else {
            self.temporary
                .persist_noclobber(target)
                .map_err(|error| error.error)?;
        }
        Ok(format!("{:x}", self.hasher.finalize()))
    }
}

fn validate_queued_target(root: &Path, target: &Path, changed: bool) -> io::Result<()> {
    let relative = target.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "download path escapes sync root",
        )
    })?;
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid download path",
        ));
    }
    let mut parent = root.to_path_buf();
    for part in relative.parent().into_iter().flat_map(Path::components) {
        parent.push(part);
        if !fs::symlink_metadata(&parent)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "unsafe download parent",
            ));
        }
    }
    match fs::symlink_metadata(target) {
        Ok(metadata) if changed && metadata.is_file() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "download target changed",
        )),
        Err(error) if !changed && error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn fingerprint_file(path: &Path) -> io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "target is not a file",
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let after = file.metadata()?;
    if before.len() != after.len() || before.modified()? != after.modified()? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "local file changed",
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn partial_download_preserves_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("report.txt");
        std::fs::write(&target, b"previous good copy").unwrap();
        let mut transfer = AtomicDownload::new(&target).unwrap();
        transfer.write_chunk(b"new").await.unwrap();
        assert!(transfer.finish(&target, Some(10)).await.is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"previous good copy");
    }

    #[tokio::test]
    async fn complete_download_replaces_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("report.txt");
        std::fs::write(&target, b"old").unwrap();
        let mut transfer = AtomicDownload::new(&target).unwrap();
        transfer.write_chunk(b"new ").await.unwrap();
        transfer.write_chunk(b"copy").await.unwrap();
        transfer.finish(&target, Some(8)).await.unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new copy");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_rejects_symlink_destination() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.txt");
        let link = directory.path().join("link.txt");
        std::fs::write(&original, b"private").unwrap();
        symlink(&original, &link).unwrap();
        assert!(AtomicDownload::new(&link).is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"private");
    }

    #[tokio::test]
    async fn queued_download_keeps_new_destination_when_created_mid_transfer() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let target = root.join("new.txt");
        let mut transfer = AtomicDownload::new_queued(root, &target, false).unwrap();
        transfer.write_chunk(b"remote").await.unwrap();
        std::fs::write(&target, b"local").unwrap();
        assert!(
            transfer
                .finish_queued(root, &target, 6, None)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"local");
    }

    #[tokio::test]
    async fn queued_download_publishes_complete_file_with_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let target = root.join("new.txt");
        let mut transfer = AtomicDownload::new_queued(root, &target, false).unwrap();
        transfer.write_chunk(b"remote").await.unwrap();
        let fingerprint = transfer
            .finish_queued(root, &target, 6, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"remote");
        assert_eq!(fingerprint, fingerprint_file(&target).unwrap());
    }

    #[tokio::test]
    async fn queued_download_keeps_changed_local_file() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let target = root.join("changed.txt");
        std::fs::write(&target, b"old").unwrap();
        let old_hash = fingerprint_file(&target).unwrap();
        let mut transfer = AtomicDownload::new_queued(root, &target, true).unwrap();
        transfer.write_chunk(b"remote").await.unwrap();
        std::fs::write(&target, b"local").unwrap();
        assert!(
            transfer
                .finish_queued(root, &target, 6, Some(&old_hash))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(target).unwrap(), b"local");
    }
}
