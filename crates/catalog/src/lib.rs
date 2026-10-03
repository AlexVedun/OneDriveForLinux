//! Persistent OneDrive metadata catalog. Only the daemon may write this database.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::Path;
use url::Url;

pub const INITIAL_DELTA_URL: &str = "https://graph.microsoft.com/v1.0/me/drive/root/delta";

#[derive(Debug)]
pub enum CatalogError {
    Database(rusqlite::Error),
    InvalidPage,
    InvalidLink,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(error) => write!(f, "catalog database error: {error}"),
            Self::InvalidPage => write!(f, "delta page must contain one continuation link"),
            Self::InvalidLink => write!(f, "delta continuation URL is not a Graph v1.0 URL"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<rusqlite::Error> for CatalogError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Debug, Deserialize)]
pub struct DeltaPage {
    pub value: Vec<DeltaItem>,
    #[serde(rename = "@odata.nextLink")]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub delta_link: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeltaItem {
    pub id: String,
    pub name: Option<String>,
    pub parent_reference: Option<ParentReference>,
    pub size: Option<i64>,
    #[serde(rename = "eTag")]
    pub etag: Option<String>,
    #[serde(rename = "cTag")]
    pub ctag: Option<String>,
    pub last_modified_date_time: Option<String>,
    pub folder: Option<serde_json::Value>,
    pub file: Option<serde_json::Value>,
    pub deleted: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct ParentReference {
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogItem {
    pub drive_id: String,
    pub item_id: String,
    pub parent_id: Option<String>,
    pub name: Option<String>,
    pub kind: String,
    pub size: Option<i64>,
    pub etag: Option<String>,
    pub ctag: Option<String>,
    pub modified: Option<String>,
    pub deleted: bool,
}

pub struct Catalog {
    connection: Connection,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FetchError {
    Gone,
    Failed,
}

#[derive(Debug)]
pub enum ScanError {
    Catalog(CatalogError),
    Fetch(FetchError),
}

impl From<CatalogError> for ScanError {
    fn from(error: CatalogError) -> Self {
        Self::Catalog(error)
    }
}

/// Walk a complete delta cycle. Each page and its continuation link are
/// committed together, so a failed fetch can resume at the saved nextLink.
pub async fn scan_with<F, Fut>(
    catalog: &mut Catalog,
    drive_id: &str,
    mut fetch: F,
) -> Result<(), ScanError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<DeltaPage, FetchError>>,
{
    let mut handled_gone = false;
    loop {
        let url = catalog.request_url(drive_id)?;
        let page = match fetch(url).await {
            Ok(page) => page,
            Err(FetchError::Gone) if !handled_gone => {
                catalog.reset_expired_cursor(drive_id)?;
                handled_gone = true;
                continue;
            }
            Err(error) => return Err(ScanError::Fetch(error)),
        };
        let complete = page.delta_link.is_some();
        catalog.apply_page(drive_id, &page)?;
        if complete {
            return Ok(());
        }
    }
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::initialize(connection)
    }

    pub fn in_memory() -> Result<Self, CatalogError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    fn initialize(connection: Connection) -> Result<Self, CatalogError> {
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version(version)
                 SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_version);
             CREATE TABLE IF NOT EXISTS remote_item (
                 drive_id TEXT NOT NULL,
                 item_id TEXT NOT NULL,
                 parent_id TEXT,
                 name TEXT,
                 kind TEXT NOT NULL,
                 size INTEGER,
                 etag TEXT,
                 ctag TEXT,
                 modified TEXT,
                 deleted INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (drive_id, item_id)
             );
             CREATE INDEX IF NOT EXISTS remote_item_parent
                 ON remote_item(drive_id, parent_id);
             CREATE TABLE IF NOT EXISTS delta_cursor (
                 drive_id TEXT PRIMARY KEY,
                 delta_link TEXT,
                 next_link TEXT,
                 needs_reconcile INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS active_drive (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 drive_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS drive_root (
                 drive_id TEXT PRIMARY KEY,
                 item_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS reconcile_seen (
                 drive_id TEXT NOT NULL,
                 item_id TEXT NOT NULL,
                 PRIMARY KEY (drive_id, item_id)
             );",
        )?;
        Ok(Self { connection })
    }

    pub fn request_url(&self, drive_id: &str) -> Result<String, CatalogError> {
        let stored: Option<(Option<String>, Option<String>)> = self
            .connection
            .query_row(
                "SELECT next_link, delta_link FROM delta_cursor WHERE drive_id = ?1",
                [drive_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(stored
            .and_then(|(next, delta)| next.or(delta))
            .unwrap_or_else(|| INITIAL_DELTA_URL.to_owned()))
    }

    pub fn active_drive_id(&self) -> Result<Option<String>, CatalogError> {
        self.connection
            .query_row(
                "SELECT drive_id FROM active_drive WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_active_drive_id(&self, drive_id: &str) -> Result<(), CatalogError> {
        self.connection.execute(
            "INSERT INTO active_drive(singleton, drive_id) VALUES (1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET drive_id = excluded.drive_id",
            [drive_id],
        )?;
        Ok(())
    }

    pub fn root_id(&self, drive_id: &str) -> Result<Option<String>, CatalogError> {
        self.connection
            .query_row(
                "SELECT item_id FROM drive_root WHERE drive_id = ?1",
                [drive_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_root_id(&self, drive_id: &str, item_id: &str) -> Result<(), CatalogError> {
        self.connection.execute(
            "INSERT INTO drive_root(drive_id, item_id) VALUES (?1, ?2)
             ON CONFLICT(drive_id) DO UPDATE SET item_id = excluded.item_id",
            params![drive_id, item_id],
        )?;
        Ok(())
    }

    pub fn apply_page(&mut self, drive_id: &str, page: &DeltaPage) -> Result<(), CatalogError> {
        let continuation = match (&page.next_link, &page.delta_link) {
            (Some(next), None) => next,
            (None, Some(delta)) => delta,
            _ => return Err(CatalogError::InvalidPage),
        };
        validate_graph_url(continuation)?;
        let transaction = self.connection.transaction()?;
        let reconciling: bool = transaction
            .query_row(
                "SELECT needs_reconcile FROM delta_cursor WHERE drive_id = ?1",
                [drive_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false);
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO remote_item
                 (drive_id, item_id, parent_id, name, kind, size, etag, ctag, modified, deleted)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(drive_id, item_id) DO UPDATE SET
                     parent_id = COALESCE(excluded.parent_id, remote_item.parent_id),
                     name = COALESCE(excluded.name, remote_item.name),
                     kind = CASE WHEN excluded.kind = 'unknown' THEN remote_item.kind ELSE excluded.kind END,
                     size = COALESCE(excluded.size, remote_item.size),
                     etag = COALESCE(excluded.etag, remote_item.etag),
                     ctag = COALESCE(excluded.ctag, remote_item.ctag),
                     modified = COALESCE(excluded.modified, remote_item.modified),
                     deleted = excluded.deleted",
            )?;
            for item in &page.value {
                let kind = if item.folder.is_some() {
                    "folder"
                } else if item.file.is_some() {
                    "file"
                } else {
                    "unknown"
                };
                statement.execute(params![
                    drive_id,
                    item.id,
                    item.parent_reference
                        .as_ref()
                        .and_then(|parent| parent.id.as_deref()),
                    item.name,
                    kind,
                    item.size,
                    item.etag,
                    item.ctag,
                    item.last_modified_date_time,
                    item.deleted.is_some(),
                ])?;
                if reconciling {
                    transaction.execute(
                        "INSERT OR IGNORE INTO reconcile_seen(drive_id, item_id) VALUES (?1, ?2)",
                        params![drive_id, item.id],
                    )?;
                }
            }
        }
        if reconciling && page.delta_link.is_some() {
            transaction.execute(
                "UPDATE remote_item SET deleted = 1
                 WHERE drive_id = ?1 AND item_id NOT IN
                     (SELECT item_id FROM reconcile_seen WHERE drive_id = ?1)",
                [drive_id],
            )?;
            transaction.execute("DELETE FROM reconcile_seen WHERE drive_id = ?1", [drive_id])?;
        }
        transaction.execute(
            "INSERT INTO delta_cursor(drive_id, delta_link, next_link, needs_reconcile)
             VALUES (?1, ?2, ?3, 0)
             ON CONFLICT(drive_id) DO UPDATE SET
                 delta_link = CASE WHEN excluded.delta_link IS NULL
                     THEN delta_cursor.delta_link ELSE excluded.delta_link END,
                 next_link = excluded.next_link,
                 needs_reconcile = CASE WHEN ?4 THEN 0 ELSE delta_cursor.needs_reconcile END",
            params![
                drive_id,
                page.delta_link,
                page.next_link,
                page.delta_link.is_some()
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A 410 response invalidates the cursor, but must not erase local metadata.
    pub fn reset_expired_cursor(&mut self, drive_id: &str) -> Result<(), CatalogError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM reconcile_seen WHERE drive_id = ?1", [drive_id])?;
        transaction.execute(
            "INSERT INTO delta_cursor(drive_id, delta_link, next_link, needs_reconcile)
             VALUES (?1, NULL, NULL, 1)
             ON CONFLICT(drive_id) DO UPDATE SET
                 delta_link = NULL, next_link = NULL, needs_reconcile = 1",
            [drive_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn needs_reconcile(&self, drive_id: &str) -> Result<bool, CatalogError> {
        Ok(self
            .connection
            .query_row(
                "SELECT needs_reconcile FROM delta_cursor WHERE drive_id = ?1",
                [drive_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn is_complete(&self, drive_id: &str) -> Result<bool, CatalogError> {
        Ok(self
            .connection
            .query_row(
                "SELECT delta_link IS NOT NULL AND next_link IS NULL AND needs_reconcile = 0
             FROM delta_cursor WHERE drive_id = ?1",
                [drive_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn items(&self, drive_id: &str) -> Result<Vec<CatalogItem>, CatalogError> {
        let mut statement = self.connection.prepare(
            "SELECT drive_id, item_id, parent_id, name, kind, size, etag, ctag, modified, deleted
             FROM remote_item WHERE drive_id = ?1 ORDER BY parent_id, name, item_id",
        )?;
        let rows = statement.query_map([drive_id], CatalogItem::from_row)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn item(&self, drive_id: &str, item_id: &str) -> Result<Option<CatalogItem>, CatalogError> {
        self.connection
            .query_row(
                "SELECT drive_id, item_id, parent_id, name, kind, size, etag, ctag, modified, deleted
                 FROM remote_item WHERE drive_id = ?1 AND item_id = ?2",
                params![drive_id, item_id],
                CatalogItem::from_row,
            )
            .optional()
            .map_err(Into::into)
    }
}

impl CatalogItem {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            drive_id: row.get(0)?,
            item_id: row.get(1)?,
            parent_id: row.get(2)?,
            name: row.get(3)?,
            kind: row.get(4)?,
            size: row.get(5)?,
            etag: row.get(6)?,
            ctag: row.get(7)?,
            modified: row.get(8)?,
            deleted: row.get(9)?,
        })
    }
}

pub fn validate_graph_url(link: &str) -> Result<(), CatalogError> {
    let url = Url::parse(link).map_err(|_| CatalogError::InvalidLink)?;
    if url.scheme() != "https"
        || url.host_str() != Some("graph.microsoft.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !url.path().starts_with("/v1.0/")
    {
        return Err(CatalogError::InvalidLink);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::fs;
    use std::rc::Rc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const NEXT: &str = "https://graph.microsoft.com/v1.0/me/drive/root/delta?token=next";
    const CURSOR: &str = "https://graph.microsoft.com/v1.0/me/drive/root/delta?token=done";

    fn page(json: &str) -> DeltaPage {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn incomplete_enumeration_resumes_after_restart() {
        let directory = std::env::temp_dir().join(format!(
            "onedrive-catalog-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("catalog.db");
        {
            let mut catalog = Catalog::open(&path).unwrap();
            assert_eq!(catalog.request_url("drive").unwrap(), INITIAL_DELTA_URL);
            assert!(!catalog.is_complete("drive").unwrap());
            catalog.set_root_id("drive", "root").unwrap();
            catalog
                .apply_page(
                    "drive",
                    &page(&format!(
                        r#"{{"value":[{{"id":"folder","name":"Old","folder":{{}}}}],"@odata.nextLink":"{NEXT}"}}"#
                    )),
                )
                .unwrap();
            assert_eq!(catalog.request_url("drive").unwrap(), NEXT);
            assert!(!catalog.is_complete("drive").unwrap());
        }
        {
            let mut catalog = Catalog::open(&path).unwrap();
            assert_eq!(catalog.root_id("drive").unwrap().as_deref(), Some("root"));
            assert_eq!(catalog.request_url("drive").unwrap(), NEXT);
            catalog
                .apply_page(
                    "drive",
                    &page(&format!(
                        r#"{{"value":[{{"id":"folder","name":"Renamed","folder":{{}}}},{{"id":"file","name":"report.txt","parentReference":{{"id":"folder"}},"file":{{}},"size":42}}],"@odata.deltaLink":"{CURSOR}"}}"#
                    )),
                )
                .unwrap();
            assert_eq!(catalog.request_url("drive").unwrap(), CURSOR);
            assert!(catalog.is_complete("drive").unwrap());
            let items = catalog.items("drive").unwrap();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0].name.as_deref(), Some("Renamed"));
            assert_eq!(items[1].parent_id.as_deref(), Some("folder"));
            assert_eq!(items[1].size, Some(42));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn deleted_item_preserves_identity_and_last_metadata() {
        let mut catalog = Catalog::in_memory().unwrap();
        catalog
            .apply_page(
                "drive",
                &page(&format!(
                    r#"{{"value":[{{"id":"item","name":"before.txt","file":{{}},"eTag":"v1"}}],"@odata.deltaLink":"{CURSOR}"}}"#
                )),
            )
            .unwrap();
        catalog
            .apply_page(
                "drive",
                &page(&format!(
                    r#"{{"value":[{{"id":"item","deleted":{{}}}}],"@odata.deltaLink":"{CURSOR}"}}"#
                )),
            )
            .unwrap();
        let items = catalog.items("drive").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item_id, "item");
        assert_eq!(items[0].name.as_deref(), Some("before.txt"));
        assert_eq!(items[0].etag.as_deref(), Some("v1"));
        assert!(items[0].deleted);
    }

    #[test]
    fn invalid_continuation_does_not_apply_items_or_advance_cursor() {
        let mut catalog = Catalog::in_memory().unwrap();
        let invalid = page(
            r#"{"value":[{"id":"item","name":"secret","file":{}}],"@odata.nextLink":"https://evil.example/collect"}"#,
        );
        assert!(matches!(
            catalog.apply_page("drive", &invalid),
            Err(CatalogError::InvalidLink)
        ));
        assert!(catalog.items("drive").unwrap().is_empty());
        assert_eq!(catalog.request_url("drive").unwrap(), INITIAL_DELTA_URL);
        assert!(validate_graph_url("https://graph.microsoft.com.evil/v1.0/delta").is_err());
        assert!(validate_graph_url("http://graph.microsoft.com/v1.0/delta").is_err());
    }

    #[test]
    fn expired_cursor_keeps_items_for_safe_reconciliation() {
        let mut catalog = Catalog::in_memory().unwrap();
        catalog
            .apply_page(
                "drive",
                &page(&format!(
                    r#"{{"value":[{{"id":"item","name":"keep.txt","file":{{}}}}],"@odata.deltaLink":"{CURSOR}"}}"#
                )),
            )
            .unwrap();
        catalog.reset_expired_cursor("drive").unwrap();
        assert_eq!(catalog.request_url("drive").unwrap(), INITIAL_DELTA_URL);
        assert!(catalog.needs_reconcile("drive").unwrap());
        assert_eq!(catalog.items("drive").unwrap().len(), 1);
    }

    #[tokio::test]
    async fn scan_retries_expired_cursor_without_discarding_catalog() {
        let mut catalog = Catalog::in_memory().unwrap();
        catalog
            .apply_page(
                "drive",
                &page(&format!(
                    r#"{{"value":[{{"id":"old","name":"keep","file":{{}}}}],"@odata.deltaLink":"{CURSOR}"}}"#
                )),
            )
            .unwrap();
        let responses = Rc::new(RefCell::new(VecDeque::from([
            Err(FetchError::Gone),
            Ok(page(&format!(
                r#"{{"value":[{{"id":"new","name":"fresh","file":{{}}}}],"@odata.deltaLink":"{NEXT}"}}"#
            ))),
        ])));
        let requested = Rc::new(RefCell::new(Vec::new()));
        let responses_for_fetch = Rc::clone(&responses);
        let requested_for_fetch = Rc::clone(&requested);
        scan_with(&mut catalog, "drive", move |url| {
            requested_for_fetch.borrow_mut().push(url);
            std::future::ready(responses_for_fetch.borrow_mut().pop_front().unwrap())
        })
        .await
        .unwrap();
        assert_eq!(requested.borrow().as_slice(), [CURSOR, INITIAL_DELTA_URL]);
        assert!(!catalog.needs_reconcile("drive").unwrap());
        let items = catalog.items("drive").unwrap();
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .any(|item| item.item_id == "old" && item.deleted)
        );
        assert!(
            items
                .iter()
                .any(|item| item.item_id == "new" && !item.deleted)
        );
    }

    #[tokio::test]
    async fn failed_next_page_remains_pending() {
        let mut catalog = Catalog::in_memory().unwrap();
        let responses = Rc::new(RefCell::new(VecDeque::from([
            Ok(page(&format!(
                r#"{{"value":[{{"id":"one","name":"one","file":{{}}}}],"@odata.nextLink":"{NEXT}"}}"#
            ))),
            Err(FetchError::Failed),
        ])));
        let responses_for_fetch = Rc::clone(&responses);
        let result = scan_with(&mut catalog, "drive", move |_| {
            std::future::ready(responses_for_fetch.borrow_mut().pop_front().unwrap())
        })
        .await;
        assert!(matches!(result, Err(ScanError::Fetch(FetchError::Failed))));
        assert_eq!(catalog.request_url("drive").unwrap(), NEXT);
        assert_eq!(catalog.items("drive").unwrap().len(), 1);
    }

    #[test]
    fn interrupted_reconciliation_only_deletes_missing_items_after_final_page() {
        let mut catalog = Catalog::in_memory().unwrap();
        catalog
            .apply_page(
                "drive",
                &page(&format!(
                    r#"{{"value":[{{"id":"kept","name":"kept","file":{{}}}},{{"id":"gone","name":"gone","file":{{}}}}],"@odata.deltaLink":"{CURSOR}"}}"#
                )),
            )
            .unwrap();
        catalog.reset_expired_cursor("drive").unwrap();
        catalog
            .apply_page(
                "drive",
                &page(&format!(
                    r#"{{"value":[{{"id":"kept","name":"renamed","file":{{}}}}],"@odata.nextLink":"{NEXT}"}}"#
                )),
            )
            .unwrap();
        assert!(catalog.needs_reconcile("drive").unwrap());
        assert!(
            catalog
                .items("drive")
                .unwrap()
                .iter()
                .all(|item| !item.deleted)
        );
        catalog
            .apply_page(
                "drive",
                &page(&format!(r#"{{"value":[],"@odata.deltaLink":"{CURSOR}"}}"#)),
            )
            .unwrap();
        assert!(!catalog.needs_reconcile("drive").unwrap());
        let items = catalog.items("drive").unwrap();
        assert!(
            items
                .iter()
                .any(|item| item.item_id == "kept" && !item.deleted)
        );
        assert!(
            items
                .iter()
                .any(|item| item.item_id == "gone" && item.deleted)
        );
    }
}
