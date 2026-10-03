//! Atomic replacement for read-only downloads. A partial transfer never
//! replaces the last good local file.

use std::io;
use std::path::Path;
use tempfile::NamedTempFile;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub struct AtomicDownload {
    temporary: NamedTempFile,
    file: File,
    written: u64,
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
        })
    }

    pub async fn write_chunk(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.file.write_all(chunk).await?;
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
}
