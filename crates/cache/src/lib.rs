//! Versioned, size-limited file cache for the future read-only virtual drive.
//! One daemon process owns this store. Remote names never become local paths.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    Database(rusqlite::Error),
    InvalidKey,
    InvalidDirectory,
    MissingEntry,
    SizeMismatch,
    LimitExceeded,
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "cache I/O error: {error}"),
            Self::Database(error) => write!(f, "cache database error: {error}"),
            Self::InvalidKey => write!(f, "cache key needs drive ID, item ID and version"),
            Self::InvalidDirectory => write!(f, "cache directory must not be a symlink"),
            Self::MissingEntry => write!(f, "cache entry is missing or has another version"),
            Self::SizeMismatch => write!(f, "downloaded file size differs from catalog"),
            Self::LimitExceeded => write!(f, "cache limit cannot hold pinned files and this item"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<io::Error> for CacheError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for CacheError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheKey {
    drive_id: String,
    item_id: String,
    version: String,
}

impl CacheKey {
    pub fn new(drive_id: &str, item_id: &str, version: &str) -> Result<Self, CacheError> {
        if drive_id.is_empty() || item_id.is_empty() || version.is_empty() {
            return Err(CacheError::InvalidKey);
        }
        Ok(Self {
            drive_id: drive_id.to_owned(),
            item_id: item_id.to_owned(),
            version: version.to_owned(),
        })
    }

    fn filename(&self) -> String {
        let mut hash = Sha256::new();
        for part in [&self.drive_id, &self.item_id, &self.version] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }
}

pub struct CacheStore {
    connection: Connection,
    objects: PathBuf,
    max_bytes: u64,
    _lock: File,
}

fn private_directory(path: &Path) -> Result<(), CacheError> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CacheError::InvalidDirectory);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn private_database(path: &Path) -> Result<(), CacheError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(path)?.is_file() {
                return Err(CacheError::InvalidDirectory);
            }
        }
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

impl CacheStore {
    pub fn open(root: &Path, max_bytes: u64) -> Result<Self, CacheError> {
        private_directory(root)?;
        let objects = root.join("objects");
        private_directory(&objects)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join("lock"))?;
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        if !lock.metadata()?.is_file() {
            return Err(CacheError::InvalidDirectory);
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let database = root.join("index.sqlite3");
        private_database(&database)?;
        let connection = Connection::open(database)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS entry (
                drive_id TEXT NOT NULL,
                item_id TEXT NOT NULL,
                version TEXT NOT NULL,
                filename TEXT NOT NULL,
                size INTEGER NOT NULL CHECK (size >= 0),
                pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
                access_seq INTEGER NOT NULL,
                PRIMARY KEY (drive_id, item_id)
            );
            CREATE TABLE IF NOT EXISTS clock (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                seq INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO clock(singleton, seq) VALUES (1, 0);",
        )?;
        let mut store = Self {
            connection,
            objects,
            max_bytes,
            _lock: lock,
        };
        store.reconcile_orphans()?;
        store.set_limit(max_bytes)?;
        Ok(store)
    }

    pub fn usage_bytes(&self) -> Result<u64, CacheError> {
        let bytes: i64 =
            self.connection
                .query_row("SELECT COALESCE(SUM(size), 0) FROM entry", [], |row| {
                    row.get(0)
                })?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }

    pub fn get(&mut self, key: &CacheKey) -> Result<Option<File>, CacheError> {
        let entry: Option<(String, i64)> = self
            .connection
            .query_row(
                "SELECT filename, size FROM entry
                 WHERE drive_id = ?1 AND item_id = ?2 AND version = ?3",
                params![key.drive_id, key.item_id, key.version],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((filename, size)) = entry else {
            return Ok(None);
        };
        if filename != key.filename() {
            return Err(CacheError::InvalidKey);
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.objects.join(filename))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || i64::try_from(metadata.len()).ok() != Some(size) {
            return Ok(None);
        }
        let transaction = self.connection.transaction()?;
        let sequence: i64 = transaction.query_row(
            "UPDATE clock SET seq = seq + 1 WHERE singleton = 1 RETURNING seq",
            [],
            |row| row.get(0),
        )?;
        transaction.execute(
            "UPDATE entry SET access_seq = ?4
             WHERE drive_id = ?1 AND item_id = ?2 AND version = ?3",
            params![key.drive_id, key.item_id, key.version, sequence],
        )?;
        transaction.commit()?;
        Ok(Some(file))
    }

    pub fn put(
        &mut self,
        key: &CacheKey,
        source: &Path,
        expected_size: u64,
    ) -> Result<(), CacheError> {
        let size = i64::try_from(expected_size).map_err(|_| CacheError::LimitExceeded)?;
        if expected_size > self.max_bytes {
            return Err(CacheError::LimitExceeded);
        }
        let old: Option<(String, String, bool)> = self
            .connection
            .query_row(
                "SELECT version, filename, pinned FROM entry WHERE drive_id = ?1 AND item_id = ?2",
                params![key.drive_id, key.item_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((version, filename, _)) = &old {
            let previous = CacheKey::new(&key.drive_id, &key.item_id, version)?;
            if *filename != previous.filename() {
                return Err(CacheError::InvalidKey);
            }
        }
        let pinned_bytes: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM entry
             WHERE pinned = 1 AND NOT (drive_id = ?1 AND item_id = ?2)",
            params![key.drive_id, key.item_id],
            |row| row.get(0),
        )?;
        if (pinned_bytes as u64).saturating_add(expected_size) > self.max_bytes {
            return Err(CacheError::LimitExceeded);
        }
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)?;
        if !input.metadata()?.is_file() {
            return Err(CacheError::SizeMismatch);
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.objects)?;
        let copied = io::copy(&mut input, &mut temporary)?;
        if copied != expected_size {
            return Err(CacheError::SizeMismatch);
        }
        temporary.as_file().sync_all()?;
        let filename = key.filename();
        temporary
            .persist(self.objects.join(&filename))
            .map_err(|error| CacheError::Io(error.error))?;
        File::open(&self.objects)?.sync_all()?;

        let transaction = self.connection.transaction()?;
        let sequence: i64 = transaction.query_row(
            "UPDATE clock SET seq = seq + 1 WHERE singleton = 1 RETURNING seq",
            [],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO entry(drive_id, item_id, version, filename, size, pinned, access_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(drive_id, item_id) DO UPDATE SET
                version = excluded.version, filename = excluded.filename,
                size = excluded.size, access_seq = excluded.access_seq",
            params![
                key.drive_id,
                key.item_id,
                key.version,
                filename,
                size,
                old.as_ref().is_some_and(|entry| entry.2),
                sequence,
            ],
        )?;
        transaction.commit()?;
        if let Some((_, previous, _)) = old
            && previous != filename
        {
            match fs::remove_file(self.objects.join(previous)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.evict_to_limit()
    }

    pub fn set_pinned(&self, key: &CacheKey, pinned: bool) -> Result<(), CacheError> {
        let changed = self.connection.execute(
            "UPDATE entry SET pinned = ?4
             WHERE drive_id = ?1 AND item_id = ?2 AND version = ?3",
            params![key.drive_id, key.item_id, key.version, pinned],
        )?;
        if changed == 0 {
            return Err(CacheError::MissingEntry);
        }
        Ok(())
    }

    /// Returns None when the requested version has no complete local copy.
    pub fn cached_pin_state(&mut self, key: &CacheKey) -> Result<Option<bool>, CacheError> {
        if self.get(key)?.is_none() {
            return Ok(None);
        }
        self.connection
            .query_row(
                "SELECT pinned FROM entry
                 WHERE drive_id = ?1 AND item_id = ?2 AND version = ?3",
                params![key.drive_id, key.item_id, key.version],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_limit(&mut self, max_bytes: u64) -> Result<(), CacheError> {
        let pinned: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM entry WHERE pinned = 1",
            [],
            |row| row.get(0),
        )?;
        if pinned as u64 > max_bytes {
            return Err(CacheError::LimitExceeded);
        }
        self.max_bytes = max_bytes;
        self.evict_to_limit()
    }

    fn evict_to_limit(&mut self) -> Result<(), CacheError> {
        while self.usage_bytes()? > self.max_bytes {
            let victim: Option<(String, String, String, String)> = self
                .connection
                .query_row(
                    "SELECT drive_id, item_id, version, filename FROM entry
                     WHERE pinned = 0 ORDER BY access_seq, drive_id, item_id LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((drive_id, item_id, version, filename)) = victim else {
                return Err(CacheError::LimitExceeded);
            };
            let key = CacheKey::new(&drive_id, &item_id, &version)?;
            if filename != key.filename() {
                return Err(CacheError::InvalidKey);
            }
            match fs::remove_file(self.objects.join(filename)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            self.connection.execute(
                "DELETE FROM entry WHERE drive_id = ?1 AND item_id = ?2",
                params![drive_id, item_id],
            )?;
        }
        Ok(())
    }

    fn reconcile_orphans(&self) -> Result<(), CacheError> {
        let mut statement = self.connection.prepare("SELECT filename FROM entry")?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()?;
        for entry in fs::read_dir(&self.objects)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_object = name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit());
            if (is_object || name.starts_with(".tmp")) && !names.contains(&name) {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn key(item: &str, version: &str) -> CacheKey {
        CacheKey::new("drive", item, version).unwrap()
    }

    fn content(file: File) -> String {
        let mut value = String::new();
        let mut file = file;
        file.read_to_string(&mut value).unwrap();
        value
    }

    #[test]
    fn cache_uses_identity_and_version_instead_of_remote_names() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"hello").unwrap();
        let root = directory.path().join("cache");
        let original = key("../../name", "v1");
        let updated = key("../../name", "v2");
        {
            let mut cache = CacheStore::open(&root, 10).unwrap();
            cache.put(&original, &source, 5).unwrap();
            assert_eq!(content(cache.get(&original).unwrap().unwrap()), "hello");
            assert!(cache.get(&updated).unwrap().is_none());
            cache.set_pinned(&original, true).unwrap();
            fs::write(&source, b"world").unwrap();
            cache.put(&updated, &source, 5).unwrap();
            assert!(cache.get(&original).unwrap().is_none());
            assert_eq!(content(cache.get(&updated).unwrap().unwrap()), "world");
            assert!(cache.set_limit(4).is_err());
            let orphan = root.join("objects").join("a".repeat(64));
            fs::write(&orphan, b"orphan").unwrap();
            assert!(CacheStore::open(&root, 5).is_err());
        }
        assert!(CacheStore::open(&root, 4).is_err());
        let mut reopened = CacheStore::open(&root, 5).unwrap();
        assert_eq!(content(reopened.get(&updated).unwrap().unwrap()), "world");
        assert_eq!(fs::read_dir(root.join("objects")).unwrap().count(), 1);
    }

    #[test]
    fn evicts_oldest_unpinned_and_preserves_pinned_files() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let mut cache = CacheStore::open(&directory.path().join("cache"), 9).unwrap();
        for item in ["one", "two"] {
            fs::write(&source, item.as_bytes()).unwrap();
            cache
                .put(&key(item, "v1"), &source, item.len() as u64)
                .unwrap();
        }
        assert!(cache.get(&key("one", "v1")).unwrap().is_some());
        fs::write(&source, b"third").unwrap();
        cache.put(&key("three", "v1"), &source, 5).unwrap();
        assert_eq!(cache.usage_bytes().unwrap(), 8);
        assert!(cache.get(&key("one", "v1")).unwrap().is_some());
        assert!(cache.get(&key("two", "v1")).unwrap().is_none());
        assert!(cache.get(&key("three", "v1")).unwrap().is_some());
        cache.set_pinned(&key("one", "v1"), true).unwrap();
        cache.put(&key("four", "v1"), &source, 5).unwrap();
        assert!(cache.get(&key("one", "v1")).unwrap().is_some());
        assert!(cache.get(&key("three", "v1")).unwrap().is_none());
        assert!(matches!(
            cache.put(&key("too-large", "v1"), &source, 7),
            Err(CacheError::LimitExceeded)
        ));
        assert_eq!(
            cache.cached_pin_state(&key("one", "v1")).unwrap(),
            Some(true)
        );
        assert_eq!(cache.cached_pin_state(&key("two", "v1")).unwrap(), None);
        assert_eq!(cache.cached_pin_state(&key("one", "v2")).unwrap(), None);
        cache.set_pinned(&key("one", "v1"), false).unwrap();
        assert_eq!(
            cache.cached_pin_state(&key("one", "v1")).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn rejects_incomplete_content_and_symlinked_cache_root() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(&source, b"short").unwrap();
        let root = directory.path().join("cache");
        let mut cache = CacheStore::open(&root, 10).unwrap();
        assert!(matches!(
            cache.put(&key("item", "v1"), &source, 6),
            Err(CacheError::SizeMismatch)
        ));
        assert!(cache.get(&key("item", "v1")).unwrap().is_none());
        let source_link = directory.path().join("source-link");
        std::os::unix::fs::symlink(&source, &source_link).unwrap();
        assert!(cache.put(&key("item", "v1"), &source_link, 5).is_err());
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert!(matches!(
            CacheStore::open(&link, 10),
            Err(CacheError::InvalidDirectory)
        ));
    }
}
