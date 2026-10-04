//! Offline sync planning. It never changes local or remote files.

pub mod folders;
pub mod monitor;
pub mod upload;

use onedrive_catalog::CatalogItem;
use onedrive_core::{PathError, resolve_path, validate_distinct_paths};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum SyncError {
    Io(io::Error),
    Database(rusqlite::Error),
    Path(PathError),
    InvalidRoot,
    RootHasState,
    MissingRootId,
    InvalidCheckpoint,
    InvalidApproval,
    DriveMismatch,
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "local scan failed: {error}"),
            Self::Database(error) => write!(f, "sync database error: {error}"),
            Self::Path(error) => write!(f, "invalid sync path: {error}"),
            Self::InvalidRoot => write!(f, "sync root must be an existing directory"),
            Self::RootHasState => write!(
                f,
                "sync root cannot change while mappings or queued operations exist"
            ),
            Self::MissingRootId => write!(f, "OneDrive root ID is missing; refresh the catalog"),
            Self::InvalidCheckpoint => write!(f, "invalid upload checkpoint"),
            Self::InvalidApproval => write!(f, "operation cannot be approved"),
            Self::DriveMismatch => write!(f, "sync state belongs to a different or unknown drive"),
        }
    }
}

impl std::error::Error for SyncError {}
impl From<io::Error> for SyncError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<rusqlite::Error> for SyncError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}
impl From<PathError> for SyncError {
    fn from(value: PathError) -> Self {
        Self::Path(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalFile {
    pub path: String,
    pub fingerprint: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFile {
    pub id: String,
    pub path: String,
    pub etag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Baseline {
    pub remote_id: String,
    pub path: String,
    pub etag: Option<String>,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    UploadNew,
    CreateRemoteFolder,
    DownloadNew,
    UploadChanged,
    DownloadChanged,
    RenameRemote,
    RenameLocal,
    DeleteRemote,
    DeleteLocal,
    Conflict,
}

impl Action {
    pub fn requires_confirmation(&self) -> bool {
        matches!(
            self,
            Self::DeleteRemote | Self::DeleteLocal | Self::RenameRemote | Self::Conflict
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlannedOperation {
    pub action: Action,
    pub path: String,
    pub from_path: Option<String>,
    pub remote_id: Option<String>,
    pub expected_etag: Option<String>,
    pub local_fingerprint: Option<String>,
    pub requires_confirmation: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Preview {
    pub operations: Vec<PlannedOperation>,
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedOperation {
    pub id: i64,
    pub kind: String,
    pub path: String,
    pub from_path: Option<String>,
    pub remote_id: Option<String>,
    pub expected_etag: Option<String>,
    pub local_fingerprint: Option<String>,
    pub attempts: u32,
}

/// The upload URL is a preauthenticated secret. Never include it in logs.
pub struct UploadCheckpoint {
    pub operation_id: i64,
    pub upload_url: String,
    pub total_size: u64,
    pub fingerprint: String,
    pub next_offset: u64,
}

pub struct SyncStore {
    connection: Connection,
}

impl SyncStore {
    pub fn open(path: &Path) -> Result<Self, SyncError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::initialize(connection)
    }

    pub fn in_memory() -> Result<Self, SyncError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(connection: Connection) -> Result<Self, SyncError> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS sync_settings (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                root TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sync_drive (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                drive_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sync_control (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                paused INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1))
             );
             CREATE TABLE IF NOT EXISTS mapping (
                remote_id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                etag TEXT,
                fingerprint TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS operation (
                id INTEGER PRIMARY KEY,
                idempotency_key TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL,
                path TEXT NOT NULL,
                from_path TEXT,
                remote_id TEXT,
                expected_etag TEXT,
                local_fingerprint TEXT,
                approved INTEGER NOT NULL DEFAULT 0 CHECK (approved IN (0, 1)),
                status TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT
             );
             CREATE TABLE IF NOT EXISTS transfer (
                operation_id INTEGER PRIMARY KEY REFERENCES operation(id) ON DELETE CASCADE,
                upload_url TEXT NOT NULL,
                total_size INTEGER NOT NULL,
                fingerprint TEXT NOT NULL,
                next_offset INTEGER NOT NULL
             );",
        )?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let columns = {
            let mut statement = connection.prepare("PRAGMA table_info(operation)")?;
            let names = statement.query_map([], |row| row.get::<_, String>(1))?;
            names.collect::<Result<Vec<_>, _>>()?
        };
        if !columns.iter().any(|name| name == "local_fingerprint") {
            connection.execute(
                "ALTER TABLE operation ADD COLUMN local_fingerprint TEXT",
                [],
            )?;
        }
        if !columns.iter().any(|name| name == "from_path") {
            connection.execute("ALTER TABLE operation ADD COLUMN from_path TEXT", [])?;
        }
        if !columns.iter().any(|name| name == "approved") {
            connection.execute(
                "ALTER TABLE operation ADD COLUMN approved INTEGER NOT NULL DEFAULT 0 CHECK (approved IN (0, 1))",
                [],
            )?;
        }
        // Existing automatic work may continue after migration. Destructive
        // operations and unresolved conflicts remain unapproved.
        connection.execute(
            "UPDATE operation SET approved = 1
             WHERE kind IN ('UploadNew', 'CreateRemoteFolder', 'DownloadNew', 'UploadChanged',
                            'DownloadChanged', 'RenameLocal')",
            [],
        )?;
        // A process crash may interrupt an operation before its result is
        // recorded. The future executor must recheck both sides on retry.
        connection.execute(
            "UPDATE operation SET status = 'pending' WHERE status = 'running'",
            [],
        )?;
        Ok(Self { connection })
    }

    pub fn set_root(&self, root: &Path, state_directory: &Path) -> Result<PathBuf, SyncError> {
        if !root.is_dir() {
            return Err(SyncError::InvalidRoot);
        }
        validate_distinct_paths(&[root, state_directory])?;
        let resolved = resolve_path(root)?;
        if self.root()?.is_some_and(|current| current != resolved) {
            let mapping_count: i64 =
                self.connection
                    .query_row("SELECT count(*) FROM mapping", [], |row| row.get(0))?;
            let operation_count: i64 =
                self.connection
                    .query_row("SELECT count(*) FROM operation", [], |row| row.get(0))?;
            if mapping_count != 0 || operation_count != 0 {
                return Err(SyncError::RootHasState);
            }
        }
        self.connection.execute(
            "INSERT INTO sync_settings(singleton, root) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET root = excluded.root",
            [resolved.to_string_lossy().as_ref()],
        )?;
        Ok(resolved)
    }

    pub fn root(&self) -> Result<Option<PathBuf>, SyncError> {
        let root: Option<String> = self
            .connection
            .query_row(
                "SELECT root FROM sync_settings WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(root.map(PathBuf::from))
    }

    pub fn is_paused(&self) -> Result<bool, SyncError> {
        Ok(self
            .connection
            .query_row(
                "SELECT paused FROM sync_control WHERE singleton = 1",
                [],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// A pause stops new claims; work already in progress finishes normally.
    pub fn set_paused(&self, paused: bool) -> Result<(), SyncError> {
        self.connection.execute(
            "INSERT INTO sync_control(singleton, paused) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET paused = excluded.paused",
            [paused],
        )?;
        Ok(())
    }

    pub fn ensure_drive(&self, drive_id: &str) -> Result<(), SyncError> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT drive_id FROM sync_drive WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if stored.as_deref() == Some(drive_id) {
            return Ok(());
        }
        let mappings: i64 =
            self.connection
                .query_row("SELECT count(*) FROM mapping", [], |row| row.get(0))?;
        let operations: i64 =
            self.connection
                .query_row("SELECT count(*) FROM operation", [], |row| row.get(0))?;
        if mappings != 0 || operations != 0 || stored.is_some() {
            return Err(SyncError::DriveMismatch);
        }
        Ok(())
    }

    pub fn bind_drive(&self, drive_id: &str) -> Result<(), SyncError> {
        self.ensure_drive(drive_id)?;
        self.connection.execute(
            "INSERT OR IGNORE INTO sync_drive(singleton, drive_id) VALUES (1, ?1)",
            [drive_id],
        )?;
        Ok(())
    }

    pub fn baselines(&self) -> Result<Vec<Baseline>, SyncError> {
        let mut statement = self
            .connection
            .prepare("SELECT remote_id, path, etag, fingerprint FROM mapping ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            Ok(Baseline {
                remote_id: row.get(0)?,
                path: row.get(1)?,
                etag: row.get(2)?,
                fingerprint: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Called only after a transfer was confirmed on both sides.
    pub fn confirm_baseline(&self, baseline: &Baseline) -> Result<(), SyncError> {
        self.connection.execute(
            "INSERT INTO mapping(remote_id, path, etag, fingerprint) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(remote_id) DO UPDATE SET path = excluded.path,
                 etag = excluded.etag, fingerprint = excluded.fingerprint",
            params![
                baseline.remote_id,
                baseline.path,
                baseline.etag,
                baseline.fingerprint
            ],
        )?;
        Ok(())
    }

    pub fn preview(
        &self,
        remote_items: &[CatalogItem],
        root_id: Option<&str>,
    ) -> Result<Preview, SyncError> {
        let root_id = root_id.ok_or(SyncError::MissingRootId)?;
        let root = self.root()?.ok_or(SyncError::InvalidRoot)?;
        if !root.is_dir() {
            return Err(SyncError::InvalidRoot);
        }
        let (mut local, mut skipped) = scan_local(&root)?;
        // The resumable transport cannot commit a zero-byte range. Keep such
        // paths out of both upload and deletion planning until a safe empty
        // file create operation is available.
        for file in &local {
            if file.size == 0 {
                skipped.push(file.path.clone());
            }
        }
        local.retain(|file| file.size != 0);
        let (remote, remote_skipped) = remote_files(remote_items, root_id);
        let mut blocked_paths = skipped.clone();
        let mut baseline = self.baselines()?;
        for mapping in &baseline {
            if remote_skipped.contains(&mapping.remote_id)
                || remote.iter().any(|item| {
                    item.id == mapping.remote_id
                        && skipped.iter().any(|path| {
                            item.path == *path || item.path.starts_with(&format!("{path}/"))
                        })
                })
            {
                blocked_paths.push(mapping.path.clone());
            }
        }
        let blocked = |path: &str| {
            blocked_paths
                .iter()
                .any(|item| path == item || path.starts_with(&format!("{item}/")))
        };
        baseline.retain(|item| !blocked(&item.path));
        let local: Vec<_> = local
            .into_iter()
            .filter(|item| !blocked(&item.path))
            .collect();
        let remote: Vec<_> = remote
            .into_iter()
            .filter(|item| !blocked(&item.path))
            .collect();
        skipped.extend(remote_skipped);
        Ok(plan(&local, &remote, &baseline, skipped))
    }

    /// Queue only upload actions that the current complete catalog can
    /// execute without replacing an unknown remote item. No network work is
    /// started here; the caller explicitly requests execution later.
    pub fn enqueue_safe_uploads(
        &mut self,
        remote_items: &[CatalogItem],
        root_id: &str,
    ) -> Result<u64, SyncError> {
        let preview = self.preview(remote_items, Some(root_id))?;
        let before = self.pending_count()?;
        for operation in preview.operations {
            let safe = match operation.action {
                Action::UploadChanged => {
                    operation.remote_id.is_some() && operation.expected_etag.is_some()
                }
                Action::UploadNew => remote_parent_exists(remote_items, root_id, &operation.path),
                _ => false,
            };
            if safe {
                self.enqueue(&operation)?;
            }
        }
        Ok(self.pending_count()?.saturating_sub(before))
    }

    /// Queue downloads only when the current local parent is an ordinary
    /// directory and the remote version can be checked before writing.
    /// Execution still requires a separate request.
    pub fn enqueue_safe_downloads(
        &mut self,
        remote_items: &[CatalogItem],
        root_id: &str,
    ) -> Result<u64, SyncError> {
        let preview = self.preview(remote_items, Some(root_id))?;
        let root = self.root()?.ok_or(SyncError::InvalidRoot)?;
        let before = self.pending_count()?;
        for operation in preview.operations {
            let safe = matches!(
                operation.action,
                Action::DownloadNew | Action::DownloadChanged
            ) && operation.remote_id.is_some()
                && operation.expected_etag.is_some()
                && (operation.action != Action::DownloadChanged
                    || operation.local_fingerprint.is_some())
                && safe_download_destination(&root, &operation.path, &operation.action);
            if safe {
                self.enqueue(&operation)?;
            }
        }
        Ok(self.pending_count()?.saturating_sub(before))
    }

    pub fn prepare_local_folders(
        &self,
        remote_items: &[CatalogItem],
        root_id: &str,
    ) -> Result<folders::FolderPreparation, SyncError> {
        let root = self.root()?.ok_or(SyncError::InvalidRoot)?;
        folders::prepare(&root, remote_items, root_id)
    }

    /// Queue only local directories whose exact parent is already identified
    /// in the complete remote catalog. Missing ancestors are queued after a
    /// later catalog refresh confirms their new IDs.
    pub fn enqueue_safe_remote_folders(
        &mut self,
        remote_items: &[CatalogItem],
        root_id: &str,
    ) -> Result<u64, SyncError> {
        let root = self.root()?.ok_or(SyncError::InvalidRoot)?;
        let local_directories = folders::scan_local_directories(&root)?;
        let before = self.pending_count()?;
        for path in local_directories {
            let Some(parent_id) = remote_parent_id(remote_items, root_id, &path) else {
                continue;
            };
            let name = path.rsplit('/').next().unwrap_or_default();
            let folded = name.to_lowercase();
            if remote_items.iter().any(|item| {
                !item.deleted
                    && item.parent_id.as_deref() == Some(parent_id)
                    && item
                        .name
                        .as_deref()
                        .is_some_and(|other| other.to_lowercase() == folded)
            }) {
                continue;
            }
            self.enqueue(&PlannedOperation {
                action: Action::CreateRemoteFolder,
                path,
                from_path: None,
                remote_id: Some(parent_id.to_owned()),
                expected_etag: None,
                local_fingerprint: None,
                requires_confirmation: false,
            })?;
        }
        Ok(self.pending_count()?.saturating_sub(before))
    }

    /// Save a plan before any network work. Actions that need review stay
    /// unapproved regardless of the caller-supplied preview flag.
    pub fn enqueue(&mut self, operation: &PlannedOperation) -> Result<i64, SyncError> {
        let mut hasher = Sha256::new();
        let kind = format!("{:?}", operation.action);
        for component in [
            kind.as_str(),
            operation.remote_id.as_deref().unwrap_or(""),
            &operation.path,
            operation.expected_etag.as_deref().unwrap_or(""),
            operation.local_fingerprint.as_deref().unwrap_or(""),
        ] {
            hasher.update((component.len() as u64).to_le_bytes());
            hasher.update(component.as_bytes());
        }
        // Keep the original key for operations queued before the from_path
        // migration. Renames have a distinct source component.
        if let Some(from_path) = &operation.from_path {
            hasher.update((from_path.len() as u64).to_le_bytes());
            hasher.update(from_path.as_bytes());
        }
        let key = format!("{:x}", hasher.finalize());
        self.connection.execute(
            "INSERT OR IGNORE INTO operation
             (idempotency_key, kind, path, from_path, remote_id, expected_etag, local_fingerprint, approved)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                key,
                kind,
                operation.path,
                operation.from_path,
                operation.remote_id,
                operation.expected_etag,
                operation.local_fingerprint,
                !operation.action.requires_confirmation(),
            ],
        )?;
        self.connection
            .query_row(
                "SELECT id FROM operation WHERE idempotency_key = ?1",
                [key],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn pending_count(&self) -> Result<u64, SyncError> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM operation WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn review_count(&self) -> Result<u64, SyncError> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM operation WHERE status = 'pending' AND approved = 0",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Only a separately reviewed deletion or remote rename can be approved.
    /// A conflict needs its own resolution operation instead.
    pub fn approve_operation(&self, id: i64) -> Result<(), SyncError> {
        let changed = self.connection.execute(
            "UPDATE operation SET approved = 1 WHERE id = ?1 AND status = 'pending'
             AND approved = 0 AND kind IN ('DeleteRemote', 'DeleteLocal', 'RenameRemote')",
            [id],
        )?;
        if changed != 1 {
            return Err(SyncError::InvalidApproval);
        }
        Ok(())
    }

    pub fn claim_next(&mut self) -> Result<Option<QueuedOperation>, SyncError> {
        let transaction = self.connection.transaction()?;
        let next: Option<QueuedOperation> = transaction
            .query_row(
                "SELECT id, kind, path, remote_id, expected_etag, attempts, local_fingerprint, from_path
             FROM operation WHERE status = 'pending' AND approved = 1
               AND kind <> 'Conflict'
               AND NOT EXISTS (SELECT 1 FROM sync_control WHERE singleton = 1 AND paused = 1)
             ORDER BY id LIMIT 1",
                [],
                |row| {
                    Ok(QueuedOperation {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        path: row.get(2)?,
                        from_path: row.get(7)?,
                        remote_id: row.get(3)?,
                        expected_etag: row.get(4)?,
                        attempts: row.get::<_, u32>(5)? + 1,
                        local_fingerprint: row.get(6)?,
                    })
                },
            )
            .optional()?;
        if let Some(operation) = &next {
            transaction.execute(
                "UPDATE operation SET status = 'running', attempts = attempts + 1 WHERE id = ?1",
                [operation.id],
            )?;
        }
        transaction.commit()?;
        Ok(next)
    }

    pub fn mark_retry(&self, id: i64, reason: &str) -> Result<(), SyncError> {
        self.connection.execute(
            "UPDATE operation SET status = 'pending', last_error = ?2 WHERE id = ?1 AND status = 'running'",
            params![id, reason],
        )?;
        Ok(())
    }

    pub fn mark_done(&self, id: i64) -> Result<(), SyncError> {
        self.connection.execute(
            "UPDATE operation SET status = 'done', last_error = NULL WHERE id = ?1 AND status = 'running'",
            [id],
        )?;
        Ok(())
    }

    pub fn claim_upload(&mut self) -> Result<Option<QueuedOperation>, SyncError> {
        let transaction = self.connection.transaction()?;
        let next: Option<QueuedOperation> = transaction
            .query_row(
                "SELECT id, kind, path, remote_id, expected_etag, attempts, local_fingerprint, from_path
             FROM operation WHERE status = 'pending' AND approved = 1
               AND kind IN ('UploadNew', 'UploadChanged')
               AND NOT EXISTS (SELECT 1 FROM sync_control WHERE singleton = 1 AND paused = 1)
             ORDER BY id LIMIT 1",
                [],
                |row| {
                    Ok(QueuedOperation {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        path: row.get(2)?,
                        from_path: row.get(7)?,
                        remote_id: row.get(3)?,
                        expected_etag: row.get(4)?,
                        attempts: row.get::<_, u32>(5)? + 1,
                        local_fingerprint: row.get(6)?,
                    })
                },
            )
            .optional()?;
        if let Some(operation) = &next {
            transaction.execute(
                "UPDATE operation SET status = 'running', attempts = attempts + 1 WHERE id = ?1",
                [operation.id],
            )?;
        }
        transaction.commit()?;
        Ok(next)
    }

    pub fn claim_download(&mut self) -> Result<Option<QueuedOperation>, SyncError> {
        let transaction = self.connection.transaction()?;
        let next: Option<QueuedOperation> = transaction
            .query_row(
                "SELECT id, kind, path, remote_id, expected_etag, attempts, local_fingerprint, from_path
                 FROM operation WHERE status = 'pending' AND approved = 1
                   AND kind IN ('DownloadNew', 'DownloadChanged')
                   AND NOT EXISTS (SELECT 1 FROM sync_control WHERE singleton = 1 AND paused = 1)
                 ORDER BY id LIMIT 1",
                [],
                |row| {
                    Ok(QueuedOperation {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        path: row.get(2)?,
                        remote_id: row.get(3)?,
                        expected_etag: row.get(4)?,
                        attempts: row.get::<_, u32>(5)? + 1,
                        local_fingerprint: row.get(6)?,
                        from_path: row.get(7)?,
                    })
                },
            )
            .optional()?;
        if let Some(operation) = &next {
            transaction.execute(
                "UPDATE operation SET status = 'running', attempts = attempts + 1 WHERE id = ?1",
                [operation.id],
            )?;
        }
        transaction.commit()?;
        Ok(next)
    }

    pub fn mark_blocked(&self, id: i64, reason: &str) -> Result<(), SyncError> {
        self.connection.execute(
            "UPDATE operation SET status = 'blocked', last_error = ?2 WHERE id = ?1 AND status = 'running'",
            params![id, reason],
        )?;
        Ok(())
    }

    pub fn finish_uploaded(
        &mut self,
        operation: &QueuedOperation,
        remote_id: &str,
        etag: &str,
    ) -> Result<(), SyncError> {
        let identity_matches = match operation.kind.as_str() {
            "UploadChanged" => operation.remote_id.as_deref() == Some(remote_id),
            "UploadNew" => operation.remote_id.is_none(),
            _ => false,
        };
        if !identity_matches || remote_id.is_empty() || etag.is_empty() {
            return Err(SyncError::InvalidCheckpoint);
        }
        let fingerprint = operation
            .local_fingerprint
            .as_deref()
            .ok_or(SyncError::InvalidCheckpoint)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE operation SET status = 'done', last_error = NULL WHERE id = ?1 AND status = 'running'",
            [operation.id],
        )?;
        if changed != 1 {
            return Err(SyncError::InvalidCheckpoint);
        }
        transaction.execute(
            "INSERT INTO mapping(remote_id, path, etag, fingerprint) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(remote_id) DO UPDATE SET path = excluded.path,
                 etag = excluded.etag, fingerprint = excluded.fingerprint",
            params![remote_id, operation.path, etag, fingerprint],
        )?;
        transaction.execute(
            "DELETE FROM transfer WHERE operation_id = ?1",
            [operation.id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_downloaded(
        &mut self,
        operation: &QueuedOperation,
        fingerprint: &str,
    ) -> Result<(), SyncError> {
        if !matches!(operation.kind.as_str(), "DownloadNew" | "DownloadChanged")
            || operation.remote_id.as_deref().is_none_or(str::is_empty)
            || operation.expected_etag.as_deref().is_none_or(str::is_empty)
            || fingerprint.is_empty()
        {
            return Err(SyncError::InvalidCheckpoint);
        }
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE operation SET status = 'done', last_error = NULL WHERE id = ?1 AND status = 'running'",
            [operation.id],
        )?;
        if changed != 1 {
            return Err(SyncError::InvalidCheckpoint);
        }
        transaction.execute(
            "INSERT INTO mapping(remote_id, path, etag, fingerprint) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(remote_id) DO UPDATE SET path = excluded.path,
                 etag = excluded.etag, fingerprint = excluded.fingerprint",
            params![
                operation.remote_id,
                operation.path,
                operation.expected_etag,
                fingerprint
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn upload_checkpoint(
        &self,
        operation_id: i64,
    ) -> Result<Option<UploadCheckpoint>, SyncError> {
        self.connection
            .query_row(
                "SELECT upload_url, total_size, fingerprint, next_offset
             FROM transfer WHERE operation_id = ?1",
                [operation_id],
                |row| {
                    let size: i64 = row.get(1)?;
                    let offset: i64 = row.get(3)?;
                    Ok(UploadCheckpoint {
                        operation_id,
                        upload_url: row.get(0)?,
                        total_size: u64::try_from(size)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(1, size))?,
                        fingerprint: row.get(2)?,
                        next_offset: u64::try_from(offset)
                            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, offset))?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn save_upload_checkpoint(&self, checkpoint: &UploadCheckpoint) -> Result<(), SyncError> {
        let url =
            url::Url::parse(&checkpoint.upload_url).map_err(|_| SyncError::InvalidCheckpoint)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || checkpoint.next_offset > checkpoint.total_size
        {
            return Err(SyncError::InvalidCheckpoint);
        }
        let size =
            i64::try_from(checkpoint.total_size).map_err(|_| SyncError::InvalidCheckpoint)?;
        let offset =
            i64::try_from(checkpoint.next_offset).map_err(|_| SyncError::InvalidCheckpoint)?;
        self.connection.execute(
            "INSERT INTO transfer(operation_id, upload_url, total_size, fingerprint, next_offset)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(operation_id) DO UPDATE SET
                 upload_url = excluded.upload_url, total_size = excluded.total_size,
                 fingerprint = excluded.fingerprint, next_offset = excluded.next_offset",
            params![
                checkpoint.operation_id,
                checkpoint.upload_url,
                size,
                checkpoint.fingerprint,
                offset
            ],
        )?;
        Ok(())
    }

    pub fn update_upload_offset(&self, operation_id: i64, offset: u64) -> Result<(), SyncError> {
        let offset = i64::try_from(offset).map_err(|_| SyncError::InvalidCheckpoint)?;
        let changed = self.connection.execute(
            "UPDATE transfer SET next_offset = ?2
             WHERE operation_id = ?1 AND next_offset <= ?2 AND total_size >= ?2",
            params![operation_id, offset],
        )?;
        if changed != 1 {
            return Err(SyncError::InvalidCheckpoint);
        }
        Ok(())
    }

    pub fn set_upload_offset_from_server(
        &self,
        operation_id: i64,
        offset: u64,
    ) -> Result<(), SyncError> {
        let offset = i64::try_from(offset).map_err(|_| SyncError::InvalidCheckpoint)?;
        let changed = self.connection.execute(
            "UPDATE transfer SET next_offset = ?2 WHERE operation_id = ?1 AND total_size >= ?2",
            params![operation_id, offset],
        )?;
        if changed != 1 {
            return Err(SyncError::InvalidCheckpoint);
        }
        Ok(())
    }

    pub fn clear_upload_checkpoint(&self, operation_id: i64) -> Result<(), SyncError> {
        self.connection.execute(
            "DELETE FROM transfer WHERE operation_id = ?1",
            [operation_id],
        )?;
        Ok(())
    }
}

/// Scan without following symlinks. Any unreadable file aborts the preview so
/// it cannot be mistaken for a local deletion.
pub fn scan_local(root: &Path) -> Result<(Vec<LocalFile>, Vec<String>), SyncError> {
    if !root.is_dir() {
        return Err(SyncError::InvalidRoot);
    }
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| SyncError::InvalidRoot)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                skipped.push(relative.to_string_lossy().into_owned());
                continue;
            };
            if !valid_name(&name) {
                skipped.push(relative.to_string_lossy().into_owned());
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                skipped.push(relative.to_string_lossy().into_owned());
            } else if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let mut file = File::open(&path)?;
                let mut hasher = Sha256::new();
                let mut buffer = [0_u8; 64 * 1024];
                loop {
                    let size = file.read(&mut buffer)?;
                    if size == 0 {
                        break;
                    }
                    hasher.update(&buffer[..size]);
                }
                let after = file.metadata()?;
                if metadata.len() != after.len() || metadata.modified()? != after.modified()? {
                    return Err(SyncError::Io(io::Error::other(
                        "local file changed during scan",
                    )));
                }
                files.push(LocalFile {
                    path: relative.to_string_lossy().replace('\\', "/"),
                    fingerprint: format!("{:x}", hasher.finalize()),
                    size: metadata.len(),
                });
            } else {
                skipped.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort();
    Ok((files, skipped))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|character| "\"*:<>?/\\|".contains(character) || character.is_control())
}

fn safe_download_destination(root: &Path, relative: &str, action: &Action) -> bool {
    let parts: Vec<_> = relative.split('/').collect();
    if parts.is_empty() || parts.iter().any(|part| !valid_name(part)) {
        return false;
    }
    let mut parent = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        parent.push(part);
        if !fs::symlink_metadata(&parent).is_ok_and(|metadata| metadata.is_dir()) {
            return false;
        }
    }
    let target = parent.join(parts[parts.len() - 1]);
    match (action, fs::symlink_metadata(target)) {
        (Action::DownloadNew, Err(error)) => error.kind() == io::ErrorKind::NotFound,
        (Action::DownloadChanged, Ok(metadata)) => metadata.is_file(),
        _ => false,
    }
}

/// Accept an upload path only when every parent is a uniquely identified
/// folder in the complete remote catalog. Case variants are ambiguous.
pub fn remote_parent_exists(items: &[CatalogItem], root_id: &str, path: &str) -> bool {
    remote_parent_id(items, root_id, path).is_some()
}

pub fn remote_parent_id<'a>(
    items: &'a [CatalogItem],
    root_id: &'a str,
    path: &str,
) -> Option<&'a str> {
    let parts: Vec<_> = path.split('/').collect();
    if root_id.is_empty() || parts.is_empty() || parts.iter().any(|part| !valid_name(part)) {
        return None;
    }
    let mut parent_id = root_id;
    for part in &parts[..parts.len() - 1] {
        let folded = part.to_lowercase();
        let mut matching = items.iter().filter(|item| {
            !item.deleted
                && item.parent_id.as_deref() == Some(parent_id)
                && item
                    .name
                    .as_ref()
                    .is_some_and(|name| name.to_lowercase() == folded)
        });
        let folder = matching.next()?;
        if matching.next().is_some()
            || folder.kind != "folder"
            || folder.name.as_deref() != Some(part)
        {
            return None;
        }
        parent_id = &folder.item_id;
    }
    Some(parent_id)
}

pub fn remote_files(items: &[CatalogItem], root_id: &str) -> (Vec<RemoteFile>, Vec<String>) {
    let by_id: HashMap<&str, &CatalogItem> = items
        .iter()
        .map(|item| (item.item_id.as_str(), item))
        .collect();
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for item in items
        .iter()
        .filter(|item| !item.deleted && item.kind == "file")
    {
        let mut parts = Vec::new();
        let mut current = item;
        let mut seen = HashSet::new();
        let valid = loop {
            if current.item_id == root_id {
                break true;
            }
            if !seen.insert(current.item_id.as_str()) {
                break false;
            }
            let Some(name) = current.name.as_deref() else {
                break false;
            };
            if !valid_name(name) {
                break false;
            }
            parts.push(name);
            let Some(parent_id) = current.parent_id.as_deref() else {
                break false;
            };
            if parent_id == root_id {
                break true;
            }
            let Some(parent) = by_id.get(parent_id) else {
                break false;
            };
            if parent.deleted || parent.kind != "folder" {
                break false;
            }
            current = parent;
        };
        if valid {
            parts.reverse();
            files.push(RemoteFile {
                id: item.item_id.clone(),
                path: parts.join("/"),
                etag: item.etag.clone(),
            });
        } else {
            skipped.push(item.item_id.clone());
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    (files, skipped)
}

pub fn plan(
    local: &[LocalFile],
    remote: &[RemoteFile],
    baseline: &[Baseline],
    mut skipped: Vec<String>,
) -> Preview {
    let local_by_path: HashMap<&str, &LocalFile> = local
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let remote_by_id: HashMap<&str, &RemoteFile> =
        remote.iter().map(|file| (file.id.as_str(), file)).collect();
    let mut local_by_fingerprint: HashMap<&str, Vec<&LocalFile>> = HashMap::new();
    for file in local {
        local_by_fingerprint
            .entry(&file.fingerprint)
            .or_default()
            .push(file);
    }
    let mut baseline_fingerprint_count: HashMap<&str, usize> = HashMap::new();
    for base in baseline {
        *baseline_fingerprint_count
            .entry(&base.fingerprint)
            .or_default() += 1;
    }
    let mut remote_by_path: HashMap<&str, &RemoteFile> = HashMap::new();
    let mut ambiguous = HashSet::new();
    let mut by_case: HashMap<String, &str> = HashMap::new();
    for file in remote {
        if remote_by_path.insert(&file.path, file).is_some() {
            ambiguous.insert(file.path.clone());
        }
        let folded = file.path.to_lowercase();
        if let Some(prior) = by_case.get(&folded) {
            if *prior != file.path {
                ambiguous.insert(file.path.clone());
                ambiguous.insert((*prior).to_owned());
            }
        } else {
            by_case.insert(folded, &file.path);
        }
    }
    for file in local {
        let folded = file.path.to_lowercase();
        if let Some(prior) = by_case.get(&folded) {
            if *prior != file.path {
                ambiguous.insert(file.path.clone());
                ambiguous.insert((*prior).to_owned());
            }
        } else {
            by_case.insert(folded, &file.path);
        }
    }
    let mut operations = Vec::new();
    let mut used_local = HashSet::new();
    let mut used_remote = HashSet::new();
    for base in baseline {
        let local = local_by_path.get(base.path.as_str()).copied();
        let remote = remote_by_id.get(base.remote_id.as_str()).copied();
        if let Some(file) = local {
            used_local.insert(file.path.as_str());
        }
        if let Some(file) = remote {
            used_remote.insert(file.id.as_str());
        }
        let local_changed = local.is_none_or(|file| file.fingerprint != base.fingerprint);
        let remote_changed =
            remote.is_none_or(|file| file.etag != base.etag || file.path != base.path);
        let rename_collision = remote.is_some_and(|file| {
            file.path != base.path && local_by_path.contains_key(file.path.as_str())
        });
        let local_move = if local.is_none()
            && remote.is_some_and(|file| file.path == base.path && file.etag == base.etag)
            && baseline_fingerprint_count.get(base.fingerprint.as_str()) == Some(&1)
        {
            local_by_fingerprint
                .get(base.fingerprint.as_str())
                .filter(|matches| matches.len() == 1)
                .and_then(|matches| matches.first().copied())
                .filter(|candidate| {
                    candidate.path != base.path
                        && !remote_by_path.contains_key(candidate.path.as_str())
                        && !ambiguous.contains(&candidate.path)
                        && !baseline.iter().any(|item| item.path == candidate.path)
                })
        } else {
            None
        };
        let action = if rename_collision
            || ambiguous.contains(&base.path)
            || remote.is_some_and(|file| ambiguous.contains(&file.path))
        {
            Some(Action::Conflict)
        } else {
            match (local_changed, remote_changed, local, remote) {
                (false, false, _, _) => None,
                (true, true, _, _) => Some(Action::Conflict),
                (true, false, Some(_), _) => Some(Action::UploadChanged),
                (true, false, None, _) if local_move.is_some() => Some(Action::RenameRemote),
                (true, false, None, _) => Some(Action::DeleteRemote),
                (false, true, Some(_), Some(file)) if file.path != base.path => {
                    if file.etag == base.etag {
                        Some(Action::RenameLocal)
                    } else {
                        Some(Action::Conflict)
                    }
                }
                (false, true, _, Some(_)) => Some(Action::DownloadChanged),
                (false, true, _, None) => Some(Action::DeleteLocal),
            }
        };
        if let Some(action) = action {
            let moved = action == Action::RenameRemote;
            if let Some(candidate) = local_move.filter(|_| moved) {
                used_local.insert(candidate.path.as_str());
            }
            let path = if moved {
                local_move.expect("rename has a destination").path.clone()
            } else {
                remote.map_or_else(|| base.path.clone(), |file| file.path.clone())
            };
            let from_path = matches!(action, Action::RenameRemote | Action::RenameLocal)
                .then(|| base.path.clone());
            operations.push(PlannedOperation {
                requires_confirmation: action.requires_confirmation(),
                action,
                path,
                from_path,
                remote_id: Some(base.remote_id.clone()),
                expected_etag: remote.and_then(|file| file.etag.clone()),
                local_fingerprint: local.or(local_move).map(|file| file.fingerprint.clone()),
            });
        }
    }
    for file in remote {
        if used_remote.contains(file.id.as_str()) {
            continue;
        }
        let local = local_by_path.get(file.path.as_str()).copied();
        if let Some(local) = local {
            used_local.insert(local.path.as_str());
        }
        let action = if local.is_some() || ambiguous.contains(&file.path) {
            Action::Conflict
        } else {
            Action::DownloadNew
        };
        push(
            &mut operations,
            action,
            file.path.clone(),
            Some(file.id.clone()),
            file.etag.clone(),
            local.map(|file| file.fingerprint.clone()),
        );
    }
    for file in local {
        if used_local.contains(file.path.as_str()) {
            continue;
        }
        let action = if ambiguous.contains(&file.path) {
            Action::Conflict
        } else {
            Action::UploadNew
        };
        push(
            &mut operations,
            action,
            file.path.clone(),
            None,
            None,
            Some(file.fingerprint.clone()),
        );
    }
    operations.sort_by(|a, b| {
        (&a.path, format!("{:?}", a.action)).cmp(&(&b.path, format!("{:?}", b.action)))
    });
    skipped.sort();
    Preview {
        operations,
        skipped,
    }
}

fn push(
    operations: &mut Vec<PlannedOperation>,
    action: Action,
    path: String,
    remote_id: Option<String>,
    expected_etag: Option<String>,
    local_fingerprint: Option<String>,
) {
    let requires_confirmation = action.requires_confirmation();
    operations.push(PlannedOperation {
        action,
        path,
        from_path: None,
        remote_id,
        expected_etag,
        local_fingerprint,
        requires_confirmation,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(path: &str, fingerprint: &str) -> LocalFile {
        LocalFile {
            path: path.into(),
            fingerprint: fingerprint.into(),
            size: 1,
        }
    }

    fn remote(id: &str, path: &str, etag: &str) -> RemoteFile {
        RemoteFile {
            id: id.into(),
            path: path.into(),
            etag: Some(etag.into()),
        }
    }

    fn base(id: &str, path: &str, etag: &str, fingerprint: &str) -> Baseline {
        Baseline {
            remote_id: id.into(),
            path: path.into(),
            etag: Some(etag.into()),
            fingerprint: fingerprint.into(),
        }
    }

    fn folder(id: &str, parent_id: &str, name: &str) -> CatalogItem {
        CatalogItem {
            drive_id: "drive".into(),
            item_id: id.into(),
            parent_id: Some(parent_id.into()),
            name: Some(name.into()),
            kind: "folder".into(),
            size: None,
            etag: None,
            ctag: None,
            modified: None,
            deleted: false,
        }
    }

    #[test]
    fn nested_upload_requires_exact_unambiguous_remote_parent() {
        let items = [folder("a", "root", "Docs"), folder("b", "a", "Drafts")];
        assert!(remote_parent_exists(
            &items,
            "root",
            "Docs/Drafts/report.txt"
        ));
        assert!(remote_parent_exists(&items, "root", "root.txt"));
        assert!(!remote_parent_exists(
            &items,
            "root",
            "Docs/Missing/report.txt"
        ));
        assert!(!remote_parent_exists(
            &items,
            "root",
            "docs/Drafts/report.txt"
        ));
        assert!(!remote_parent_exists(&items, "root", "Docs//report.txt"));
        let ambiguous = [folder("a", "root", "Docs"), folder("b", "root", "docs")];
        assert!(!remote_parent_exists(&ambiguous, "root", "Docs/report.txt"));
    }

    #[test]
    fn initial_preview_preserves_same_path_as_conflict() {
        let result = plan(
            &[local("same.txt", "local")],
            &[remote("id", "same.txt", "v1")],
            &[],
            vec![],
        );
        assert_eq!(result.operations.len(), 1);
        assert_eq!(result.operations[0].action, Action::Conflict);
        assert!(result.operations[0].requires_confirmation);
    }

    #[test]
    fn three_way_changes_never_silently_overwrite_or_delete() {
        let baseline = [base("id", "report.txt", "v1", "old")];
        let both = plan(
            &[local("report.txt", "new")],
            &[remote("id", "report.txt", "v2")],
            &baseline,
            vec![],
        );
        assert_eq!(both.operations[0].action, Action::Conflict);
        let local_deleted = plan(&[], &[remote("id", "report.txt", "v1")], &baseline, vec![]);
        assert_eq!(local_deleted.operations[0].action, Action::DeleteRemote);
        assert!(local_deleted.operations[0].requires_confirmation);
        let remote_deleted = plan(&[local("report.txt", "old")], &[], &baseline, vec![]);
        assert_eq!(remote_deleted.operations[0].action, Action::DeleteLocal);
        assert!(remote_deleted.operations[0].requires_confirmation);
    }

    #[test]
    fn unique_local_move_is_one_reviewable_rename() {
        let result = plan(
            &[local("moved/report.txt", "hash")],
            &[remote("id", "report.txt", "v1")],
            &[base("id", "report.txt", "v1", "hash")],
            vec![],
        );
        assert_eq!(result.operations.len(), 1);
        let rename = &result.operations[0];
        assert_eq!(rename.action, Action::RenameRemote);
        assert_eq!(rename.from_path.as_deref(), Some("report.txt"));
        assert_eq!(rename.path, "moved/report.txt");
        assert!(rename.requires_confirmation);
    }

    #[test]
    fn repeated_content_cannot_identify_a_local_move() {
        let result = plan(
            &[local("moved.txt", "same")],
            &[
                remote("first", "first.txt", "v1"),
                remote("second", "second.txt", "v1"),
            ],
            &[
                base("first", "first.txt", "v1", "same"),
                base("second", "second.txt", "v1", "same"),
            ],
            vec![],
        );
        assert!(
            !result
                .operations
                .iter()
                .any(|item| item.action == Action::RenameRemote)
        );
        assert_eq!(
            result
                .operations
                .iter()
                .filter(|item| item.action == Action::DeleteRemote)
                .count(),
            2
        );
    }

    #[test]
    fn remote_move_preserves_id_and_needs_unchanged_version() {
        let baseline = [base("id", "old.txt", "v1", "hash")];
        let moved = plan(
            &[local("old.txt", "hash")],
            &[remote("id", "new.txt", "v1")],
            &baseline,
            vec![],
        );
        assert_eq!(moved.operations.len(), 1);
        assert_eq!(moved.operations[0].action, Action::RenameLocal);
        assert_eq!(moved.operations[0].from_path.as_deref(), Some("old.txt"));
        let changed = plan(
            &[local("old.txt", "hash")],
            &[remote("id", "new.txt", "v2")],
            &baseline,
            vec![],
        );
        assert_eq!(changed.operations[0].action, Action::Conflict);
    }

    #[test]
    fn scan_skips_symlink_and_never_reads_outside_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("good.txt"), b"content").unwrap();
        fs::write(directory.path().join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(directory.path().join("outside.txt"), root.join("link.txt"))
            .unwrap();
        let (files, skipped) = scan_local(&root).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "good.txt");
        assert_eq!(skipped, ["link.txt"]);
    }

    #[test]
    fn queue_and_root_survive_restart_without_duplicate_operations() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        let state = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        let path = state.join("sync.sqlite3");
        let operation = PlannedOperation {
            action: Action::UploadNew,
            path: "one.txt".into(),
            from_path: None,
            remote_id: None,
            expected_etag: None,
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        {
            let mut store = SyncStore::open(&path).unwrap();
            store.set_root(&root, &state).unwrap();
            store.enqueue(&operation).unwrap();
            store.enqueue(&operation).unwrap();
            assert_eq!(store.pending_count().unwrap(), 1);
            let mut changed = operation.clone();
            changed.local_fingerprint = Some("changed".into());
            store.enqueue(&changed).unwrap();
            assert_eq!(store.pending_count().unwrap(), 2);
            let claimed = store.claim_next().unwrap().unwrap();
            assert_eq!(claimed.attempts, 1);
            assert_eq!(store.pending_count().unwrap(), 1);
            assert!(store.set_root(&state.join("nested"), &state).is_err());
        }
        let mut store = SyncStore::open(&path).unwrap();
        assert_eq!(store.root().unwrap(), Some(root.canonicalize().unwrap()));
        assert_eq!(store.pending_count().unwrap(), 2);
        let claimed = store.claim_next().unwrap().unwrap();
        assert_eq!(claimed.attempts, 2);
        store.mark_retry(claimed.id, "network unavailable").unwrap();
        assert_eq!(store.pending_count().unwrap(), 2);
        let claimed = store.claim_next().unwrap().unwrap();
        assert_eq!(claimed.attempts, 3);
        store.mark_done(claimed.id).unwrap();
        assert_eq!(store.pending_count().unwrap(), 1);
        let replacement = directory.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        assert!(matches!(
            store.set_root(&replacement, &state),
            Err(SyncError::RootHasState)
        ));
    }

    #[test]
    fn existing_operation_database_gets_fingerprint_column() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("old.sqlite3");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE operation (
                id INTEGER PRIMARY KEY, idempotency_key TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL, path TEXT NOT NULL, remote_id TEXT,
                expected_etag TEXT, status TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT
             );",
            )
            .unwrap();
        let mut store = SyncStore::open(&path).unwrap();
        let operation = PlannedOperation {
            action: Action::UploadNew,
            path: "file.txt".into(),
            from_path: None,
            remote_id: None,
            expected_etag: None,
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        store
            .connection
            .execute(
                "INSERT INTO operation(idempotency_key, kind, path, local_fingerprint, approved)
             VALUES (?1, 'UploadNew', 'file.txt', 'hash', 1)",
                ["81f7011b8f12139db6715dc832fc3f5c96ac83786383d005c841f67ee884a08a"],
            )
            .unwrap();
        assert_eq!(store.enqueue(&operation).unwrap(), 1);
        assert_eq!(store.pending_count().unwrap(), 1);
        assert_eq!(
            store
                .claim_next()
                .unwrap()
                .unwrap()
                .local_fingerprint
                .as_deref(),
            Some("hash")
        );
    }

    #[test]
    fn queued_rename_keeps_source_path_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sync.sqlite3");
        let operation = PlannedOperation {
            action: Action::RenameLocal,
            path: "new.txt".into(),
            from_path: Some("old.txt".into()),
            remote_id: Some("id".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        SyncStore::open(&path).unwrap().enqueue(&operation).unwrap();
        let mut reopened = SyncStore::open(&path).unwrap();
        let queued = reopened.claim_next().unwrap().unwrap();
        assert_eq!(queued.kind, "RenameLocal");
        assert_eq!(queued.path, "new.txt");
        assert_eq!(queued.from_path.as_deref(), Some("old.txt"));
    }

    #[test]
    fn review_gate_survives_restart_and_ignores_preview_flag() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sync.sqlite3");
        let deletion = PlannedOperation {
            action: Action::DeleteRemote,
            path: "gone.txt".into(),
            from_path: None,
            remote_id: Some("id".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: None,
            requires_confirmation: false,
        };
        let conflict = PlannedOperation {
            action: Action::Conflict,
            path: "changed.txt".into(),
            from_path: None,
            remote_id: Some("other".into()),
            expected_etag: Some("v2".into()),
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        let automatic = PlannedOperation {
            action: Action::UploadNew,
            path: "new.txt".into(),
            from_path: None,
            remote_id: None,
            expected_etag: None,
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        let (deletion_id, conflict_id) = {
            let mut store = SyncStore::open(&path).unwrap();
            let deletion_id = store.enqueue(&deletion).unwrap();
            let conflict_id = store.enqueue(&conflict).unwrap();
            store.enqueue(&automatic).unwrap();
            assert_eq!(store.review_count().unwrap(), 2);
            let claimed = store.claim_next().unwrap().unwrap();
            assert_eq!(claimed.kind, "UploadNew");
            store.mark_done(claimed.id).unwrap();
            assert!(store.claim_next().unwrap().is_none());
            (deletion_id, conflict_id)
        };
        let mut store = SyncStore::open(&path).unwrap();
        assert_eq!(store.review_count().unwrap(), 2);
        assert!(matches!(
            store.approve_operation(conflict_id),
            Err(SyncError::InvalidApproval)
        ));
        store.approve_operation(deletion_id).unwrap();
        assert!(matches!(
            store.approve_operation(deletion_id),
            Err(SyncError::InvalidApproval)
        ));
        assert_eq!(store.review_count().unwrap(), 1);
        assert_eq!(store.claim_next().unwrap().unwrap().kind, "DeleteRemote");
        assert!(store.claim_next().unwrap().is_none());
    }

    #[test]
    fn migration_keeps_legacy_deletion_awaiting_review() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("old.sqlite3");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE operation (
                    id INTEGER PRIMARY KEY, idempotency_key TEXT NOT NULL UNIQUE,
                    kind TEXT NOT NULL, path TEXT NOT NULL, remote_id TEXT,
                    expected_etag TEXT, status TEXT NOT NULL DEFAULT 'pending',
                    attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT
                 );
                 INSERT INTO operation(idempotency_key, kind, path)
                 VALUES ('old-delete', 'DeleteRemote', 'old.txt'),
                        ('old-upload', 'UploadNew', 'new.txt');",
            )
            .unwrap();
        let mut store = SyncStore::open(&path).unwrap();
        assert_eq!(store.review_count().unwrap(), 1);
        assert_eq!(store.claim_next().unwrap().unwrap().kind, "UploadNew");
        assert!(store.claim_next().unwrap().is_none());
    }

    #[test]
    fn local_deletion_and_remote_rename_also_need_approval() {
        let mut store = SyncStore::in_memory().unwrap();
        for action in [Action::DeleteLocal, Action::RenameRemote] {
            let operation = PlannedOperation {
                action,
                path: "new.txt".into(),
                from_path: Some("old.txt".into()),
                remote_id: Some("id".into()),
                expected_etag: Some("v1".into()),
                local_fingerprint: Some("hash".into()),
                requires_confirmation: false,
            };
            let id = store.enqueue(&operation).unwrap();
            assert!(store.claim_next().unwrap().is_none());
            store.approve_operation(id).unwrap();
            let claimed = store.claim_next().unwrap().unwrap();
            assert_eq!(claimed.id, id);
            store.mark_done(id).unwrap();
        }
    }

    #[test]
    fn upload_checkpoint_persists_and_rejects_regression() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sync.sqlite3");
        let operation = PlannedOperation {
            action: Action::UploadChanged,
            path: "file.txt".into(),
            from_path: None,
            remote_id: Some("item".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        let id = {
            let mut store = SyncStore::open(&path).unwrap();
            let id = store.enqueue(&operation).unwrap();
            let checkpoint = UploadCheckpoint {
                operation_id: id,
                upload_url: "https://upload.example/session?secret=1".into(),
                total_size: 1_000_000,
                fingerprint: "hash".into(),
                next_offset: 0,
            };
            store.save_upload_checkpoint(&checkpoint).unwrap();
            store.update_upload_offset(id, 327_680).unwrap();
            assert!(store.update_upload_offset(id, 1).is_err());
            id
        };
        let store = SyncStore::open(&path).unwrap();
        let checkpoint = store.upload_checkpoint(id).unwrap().unwrap();
        assert_eq!(checkpoint.next_offset, 327_680);
        assert_eq!(checkpoint.total_size, 1_000_000);
        store.clear_upload_checkpoint(id).unwrap();
        assert!(store.upload_checkpoint(id).unwrap().is_none());
    }

    #[test]
    fn completed_upload_updates_mapping_and_queue_atomically() {
        let mut store = SyncStore::in_memory().unwrap();
        let operation = PlannedOperation {
            action: Action::UploadChanged,
            path: "file.txt".into(),
            from_path: None,
            remote_id: Some("item".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some("new-hash".into()),
            requires_confirmation: false,
        };
        let id = store.enqueue(&operation).unwrap();
        let claimed = store.claim_upload().unwrap().unwrap();
        let checkpoint = UploadCheckpoint {
            operation_id: id,
            upload_url: "https://upload.example/secret".into(),
            total_size: 1,
            fingerprint: "new-hash".into(),
            next_offset: 0,
        };
        store.save_upload_checkpoint(&checkpoint).unwrap();
        assert!(store.finish_uploaded(&claimed, "wrong-id", "v2").is_err());
        assert!(store.upload_checkpoint(id).unwrap().is_some());
        store.finish_uploaded(&claimed, "item", "v2").unwrap();
        assert!(store.upload_checkpoint(id).unwrap().is_none());
        let mapping = store.baselines().unwrap();
        assert_eq!(mapping.len(), 1);
        assert_eq!(mapping[0].etag.as_deref(), Some("v2"));
        assert_eq!(mapping[0].fingerprint, "new-hash");
        assert_eq!(store.pending_count().unwrap(), 0);
    }

    #[test]
    fn new_upload_confirms_server_identity_and_baseline() {
        let mut store = SyncStore::in_memory().unwrap();
        let operation = PlannedOperation {
            action: Action::UploadNew,
            path: "new.txt".into(),
            from_path: None,
            remote_id: None,
            expected_etag: None,
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        let id = store.enqueue(&operation).unwrap();
        let claimed = store.claim_upload().unwrap().unwrap();
        assert_eq!(claimed.id, id);
        assert!(store.finish_uploaded(&claimed, "", "v1").is_err());
        assert!(store.baselines().unwrap().is_empty());
        store.finish_uploaded(&claimed, "created-id", "v1").unwrap();
        assert_eq!(store.pending_count().unwrap(), 0);
        assert_eq!(
            store.baselines().unwrap(),
            [Baseline {
                remote_id: "created-id".into(),
                path: "new.txt".into(),
                etag: Some("v1".into()),
                fingerprint: "hash".into(),
            }]
        );
    }

    #[test]
    fn pause_persists_and_prevents_new_claims() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sync.sqlite3");
        let operation = PlannedOperation {
            action: Action::UploadChanged,
            path: "file.txt".into(),
            from_path: None,
            remote_id: Some("item".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        {
            let mut store = SyncStore::open(&path).unwrap();
            store.enqueue(&operation).unwrap();
            store.set_paused(true).unwrap();
            assert!(store.is_paused().unwrap());
            assert!(store.claim_next().unwrap().is_none());
            assert_eq!(store.pending_count().unwrap(), 1);
        }
        let mut store = SyncStore::open(&path).unwrap();
        assert!(store.is_paused().unwrap());
        assert!(store.claim_upload().unwrap().is_none());
        store.set_paused(false).unwrap();
        assert!(!store.is_paused().unwrap());
        assert!(store.claim_upload().unwrap().is_some());
    }

    #[test]
    fn queued_state_is_bound_to_one_drive() {
        let mut store = SyncStore::in_memory().unwrap();
        store.bind_drive("drive-one").unwrap();
        let operation = PlannedOperation {
            action: Action::UploadNew,
            path: "file".into(),
            from_path: None,
            remote_id: None,
            expected_etag: None,
            local_fingerprint: Some("hash".into()),
            requires_confirmation: false,
        };
        store.enqueue(&operation).unwrap();
        assert!(store.ensure_drive("drive-one").is_ok());
        assert!(matches!(
            store.bind_drive("drive-two"),
            Err(SyncError::DriveMismatch)
        ));
    }

    #[test]
    fn remote_paths_follow_parent_ids_after_folder_rename() {
        let items = vec![
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "folder".into(),
                parent_id: Some("root".into()),
                name: Some("Renamed".into()),
                kind: "folder".into(),
                size: None,
                etag: None,
                ctag: None,
                modified: None,
                deleted: false,
            },
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "file".into(),
                parent_id: Some("folder".into()),
                name: Some("report.txt".into()),
                kind: "file".into(),
                size: Some(1),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
        ];
        let (files, skipped) = remote_files(&items, "root");
        assert!(skipped.is_empty());
        assert_eq!(files[0].path, "Renamed/report.txt");
    }

    #[test]
    fn root_name_is_not_part_of_relative_path() {
        let items = vec![
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "root".into(),
                parent_id: None,
                name: Some("OneDrive".into()),
                kind: "folder".into(),
                size: None,
                etag: None,
                ctag: None,
                modified: None,
                deleted: false,
            },
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "file".into(),
                parent_id: Some("root".into()),
                name: Some("report.txt".into()),
                kind: "file".into(),
                size: Some(1),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
        ];
        let (files, skipped) = remote_files(&items, "root");
        assert!(skipped.is_empty());
        assert_eq!(files[0].path, "report.txt");
        let mut orphan = items[1].clone();
        orphan.parent_id = Some("missing".into());
        let (files, skipped) = remote_files(&[orphan], "root");
        assert!(files.is_empty());
        assert_eq!(skipped, ["file"]);
    }

    #[test]
    fn skipped_symlink_never_becomes_a_delete_or_download() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        let state = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        std::os::unix::fs::symlink(directory.path(), root.join("file.txt")).unwrap();
        let store = SyncStore::in_memory().unwrap();
        store.set_root(&root, &state).unwrap();
        store
            .confirm_baseline(&base("id", "file.txt", "v1", "hash"))
            .unwrap();
        let item = CatalogItem {
            drive_id: "drive".into(),
            item_id: "id".into(),
            parent_id: Some("root".into()),
            name: Some("file.txt".into()),
            kind: "file".into(),
            size: Some(1),
            etag: Some("v1".into()),
            ctag: None,
            modified: None,
            deleted: false,
        };
        let preview = store.preview(&[item], Some("root")).unwrap();
        assert!(preview.operations.is_empty());
        assert_eq!(preview.skipped, ["file.txt"]);
    }

    #[test]
    fn empty_local_file_is_skipped_without_upload_or_delete() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        let state = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(root.join("empty.txt"), b"").unwrap();
        let store = SyncStore::in_memory().unwrap();
        store.set_root(&root, &state).unwrap();
        store
            .confirm_baseline(&base("id", "empty.txt", "v1", "old-hash"))
            .unwrap();
        let item = CatalogItem {
            drive_id: "drive".into(),
            item_id: "id".into(),
            parent_id: Some("root".into()),
            name: Some("empty.txt".into()),
            kind: "file".into(),
            size: Some(0),
            etag: Some("v1".into()),
            ctag: None,
            modified: None,
            deleted: false,
        };
        let preview = store.preview(&[item], Some("root")).unwrap();
        assert!(preview.operations.is_empty());
        assert_eq!(preview.skipped, ["empty.txt"]);
    }

    #[test]
    fn safe_upload_batch_skips_empty_missing_parent_and_repeats() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        let state = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(root.join("Docs")).unwrap();
        fs::create_dir(root.join("Missing")).unwrap();
        fs::write(root.join("new.txt"), b"new").unwrap();
        fs::write(root.join("Docs/nested.txt"), b"nested").unwrap();
        fs::write(root.join("Missing/unavailable.txt"), b"missing").unwrap();
        fs::write(root.join("empty.txt"), b"").unwrap();
        fs::write(root.join("matched.txt"), b"changed").unwrap();
        let items = [
            folder("docs", "root", "Docs"),
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "existing".into(),
                parent_id: Some("root".into()),
                name: Some("matched.txt".into()),
                kind: "file".into(),
                size: Some(7),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
        ];
        let mut store = SyncStore::in_memory().unwrap();
        store.set_root(&root, &state).unwrap();
        store
            .confirm_baseline(&base("existing", "matched.txt", "v1", "old-hash"))
            .unwrap();
        assert_eq!(store.enqueue_safe_uploads(&items, "root").unwrap(), 3);
        assert_eq!(store.enqueue_safe_uploads(&items, "root").unwrap(), 0);
        let mut paths = Vec::new();
        while let Some(operation) = store.claim_upload().unwrap() {
            paths.push(operation.path);
        }
        paths.sort();
        assert_eq!(paths, ["Docs/nested.txt", "matched.txt", "new.txt"]);
    }

    #[test]
    fn safe_download_batch_requires_version_and_existing_local_parent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        let state = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(root.join("changed.txt"), b"old").unwrap();
        fs::create_dir(root.join("occupied.txt")).unwrap();
        let items = [
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "changed".into(),
                parent_id: Some("root".into()),
                name: Some("changed.txt".into()),
                kind: "file".into(),
                size: Some(3),
                etag: Some("v2".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "new".into(),
                parent_id: Some("root".into()),
                name: Some("new.txt".into()),
                kind: "file".into(),
                size: Some(3),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "unversioned".into(),
                parent_id: Some("root".into()),
                name: Some("unversioned.txt".into()),
                kind: "file".into(),
                size: Some(3),
                etag: None,
                ctag: None,
                modified: None,
                deleted: false,
            },
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "occupied".into(),
                parent_id: Some("root".into()),
                name: Some("occupied.txt".into()),
                kind: "file".into(),
                size: Some(3),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
            folder("remote-folder", "root", "Missing"),
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "nested".into(),
                parent_id: Some("remote-folder".into()),
                name: Some("nested.txt".into()),
                kind: "file".into(),
                size: Some(3),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
        ];
        let mut store = SyncStore::in_memory().unwrap();
        store.set_root(&root, &state).unwrap();
        let fingerprint = scan_local(&root)
            .unwrap()
            .0
            .into_iter()
            .find(|file| file.path == "changed.txt")
            .unwrap()
            .fingerprint;
        store
            .confirm_baseline(&base("changed", "changed.txt", "v1", &fingerprint))
            .unwrap();
        assert_eq!(store.enqueue_safe_downloads(&items, "root").unwrap(), 2);
        assert_eq!(store.enqueue_safe_downloads(&items, "root").unwrap(), 0);
        assert_eq!(store.pending_count().unwrap(), 2);
        let first = store.claim_download().unwrap().unwrap();
        assert_eq!(first.kind, "DownloadChanged");
        assert_eq!(first.expected_etag.as_deref(), Some("v2"));
        store.finish_downloaded(&first, "new-hash").unwrap();
        assert_eq!(
            store
                .baselines()
                .unwrap()
                .iter()
                .find(|item| item.remote_id == "changed")
                .unwrap()
                .fingerprint,
            "new-hash"
        );
        let second = store.claim_download().unwrap().unwrap();
        assert_eq!(second.kind, "DownloadNew");
        store.mark_retry(second.id, "offline").unwrap();
        store.set_paused(true).unwrap();
        assert!(store.claim_download().unwrap().is_none());
        store.set_paused(false).unwrap();
        assert_eq!(store.claim_download().unwrap().unwrap().id, second.id);
    }

    #[cfg(unix)]
    #[test]
    fn download_destination_rejects_symlink_parent_and_target() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/file.txt"), b"old").unwrap();
        symlink(root.join("real"), root.join("linked")).unwrap();
        symlink(root.join("real/file.txt"), root.join("target.txt")).unwrap();
        assert!(!safe_download_destination(
            &root,
            "linked/new.txt",
            &Action::DownloadNew
        ));
        assert!(!safe_download_destination(
            &root,
            "target.txt",
            &Action::DownloadChanged
        ));
    }

    #[test]
    fn remote_move_under_skipped_symlink_does_not_delete_old_copy() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("sync");
        let state = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(root.join("old.txt"), b"content").unwrap();
        std::os::unix::fs::symlink(directory.path(), root.join("linked")).unwrap();
        let store = SyncStore::in_memory().unwrap();
        store.set_root(&root, &state).unwrap();
        let fingerprint = format!("{:x}", Sha256::digest(b"content"));
        store
            .confirm_baseline(&base("id", "old.txt", "v1", &fingerprint))
            .unwrap();
        let items = vec![
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "folder".into(),
                parent_id: Some("root".into()),
                name: Some("linked".into()),
                kind: "folder".into(),
                size: None,
                etag: None,
                ctag: None,
                modified: None,
                deleted: false,
            },
            CatalogItem {
                drive_id: "drive".into(),
                item_id: "id".into(),
                parent_id: Some("folder".into()),
                name: Some("new.txt".into()),
                kind: "file".into(),
                size: Some(7),
                etag: Some("v1".into()),
                ctag: None,
                modified: None,
                deleted: false,
            },
        ];
        let preview = store.preview(&items, Some("root")).unwrap();
        assert!(preview.operations.is_empty());
        assert_eq!(preview.skipped, ["linked"]);
    }

    #[test]
    fn case_collisions_require_review() {
        let result = plan(
            &[local("report.txt", "a")],
            &[remote("id", "Report.txt", "v1")],
            &[],
            vec![],
        );
        assert!(
            result
                .operations
                .iter()
                .all(|item| item.action == Action::Conflict)
        );
    }

    #[test]
    fn remote_rename_into_existing_local_path_is_conflict() {
        let result = plan(
            &[local("old.txt", "hash"), local("taken.txt", "other")],
            &[remote("id", "taken.txt", "v2")],
            &[base("id", "old.txt", "v1", "hash")],
            vec![],
        );
        assert!(
            result
                .operations
                .iter()
                .any(|item| item.remote_id.as_deref() == Some("id")
                    && item.action == Action::Conflict)
        );
    }
}
