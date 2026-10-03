//! Offline sync planning. It never changes local or remote files.

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
    DownloadNew,
    UploadChanged,
    DownloadChanged,
    DeleteRemote,
    DeleteLocal,
    Conflict,
}

impl Action {
    pub fn requires_confirmation(&self) -> bool {
        matches!(
            self,
            Self::DeleteRemote | Self::DeleteLocal | Self::Conflict
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlannedOperation {
    pub action: Action,
    pub path: String,
    pub remote_id: Option<String>,
    pub expected_etag: Option<String>,
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
    pub remote_id: Option<String>,
    pub expected_etag: Option<String>,
    pub attempts: u32,
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
                remote_id TEXT,
                expected_etag TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT
             );",
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
        let (local, mut skipped) = scan_local(&root)?;
        let (remote, remote_skipped) = remote_files(remote_items, root_id);
        let mut blocked_paths = skipped.clone();
        let mut baseline = self.baselines()?;
        for mapping in &baseline {
            if remote_skipped.contains(&mapping.remote_id) {
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

    /// Save an approved plan before any network work. Repeated submissions do
    /// not duplicate operations or reset attempt counters.
    pub fn enqueue(&mut self, operation: &PlannedOperation) -> Result<(), SyncError> {
        let key = format!(
            "{:?}:{}:{}",
            operation.action,
            operation.remote_id.as_deref().unwrap_or(""),
            operation.path
        );
        self.connection.execute(
            "INSERT OR IGNORE INTO operation
             (idempotency_key, kind, path, remote_id, expected_etag)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key,
                format!("{:?}", operation.action),
                operation.path,
                operation.remote_id,
                operation.expected_etag
            ],
        )?;
        Ok(())
    }

    pub fn pending_count(&self) -> Result<u64, SyncError> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM operation WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn claim_next(&mut self) -> Result<Option<QueuedOperation>, SyncError> {
        let transaction = self.connection.transaction()?;
        let next: Option<QueuedOperation> = transaction
            .query_row(
                "SELECT id, kind, path, remote_id, expected_etag, attempts
             FROM operation WHERE status = 'pending' ORDER BY id LIMIT 1",
                [],
                |row| {
                    Ok(QueuedOperation {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        path: row.get(2)?,
                        remote_id: row.get(3)?,
                        expected_etag: row.get(4)?,
                        attempts: row.get::<_, u32>(5)? + 1,
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
                (true, false, None, _) => Some(Action::DeleteRemote),
                (false, true, _, Some(_)) => Some(Action::DownloadChanged),
                (false, true, _, None) => Some(Action::DeleteLocal),
            }
        };
        if let Some(action) = action {
            let path = remote.map_or_else(|| base.path.clone(), |file| file.path.clone());
            push(
                &mut operations,
                action,
                path,
                Some(base.remote_id.clone()),
                remote.and_then(|file| file.etag.clone()),
            );
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
        push(&mut operations, action, file.path.clone(), None, None);
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
) {
    let requires_confirmation = action.requires_confirmation();
    operations.push(PlannedOperation {
        action,
        path,
        remote_id,
        expected_etag,
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
            remote_id: None,
            expected_etag: None,
            requires_confirmation: false,
        };
        {
            let mut store = SyncStore::open(&path).unwrap();
            store.set_root(&root, &state).unwrap();
            store.enqueue(&operation).unwrap();
            store.enqueue(&operation).unwrap();
            assert_eq!(store.pending_count().unwrap(), 1);
            let claimed = store.claim_next().unwrap().unwrap();
            assert_eq!(claimed.attempts, 1);
            assert_eq!(store.pending_count().unwrap(), 0);
            assert!(store.set_root(&state.join("nested"), &state).is_err());
        }
        let mut store = SyncStore::open(&path).unwrap();
        assert_eq!(store.root().unwrap(), Some(root.canonicalize().unwrap()));
        assert_eq!(store.pending_count().unwrap(), 1);
        let claimed = store.claim_next().unwrap().unwrap();
        assert_eq!(claimed.attempts, 2);
        store.mark_retry(claimed.id, "network unavailable").unwrap();
        assert_eq!(store.pending_count().unwrap(), 1);
        let claimed = store.claim_next().unwrap().unwrap();
        assert_eq!(claimed.attempts, 3);
        store.mark_done(claimed.id).unwrap();
        assert_eq!(store.pending_count().unwrap(), 0);
        let replacement = directory.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        assert!(matches!(
            store.set_root(&replacement, &state),
            Err(SyncError::RootHasState)
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
