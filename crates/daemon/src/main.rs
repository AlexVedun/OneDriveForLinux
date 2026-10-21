mod auth;
mod graph;
mod mount_record;
mod transfer;
mod virtual_drafts;
mod virtual_upload;

use auth::LoginAttempt;
use graph::{
    CreatedVirtualExpectation, FolderCreateError, GraphClient, GraphUpload, QueuedDownloadError,
    QueuedDownloadTarget, RemoteDeleteError, RemoteDeleteInput, RemoteRenameError,
    RemoteRenameInput, VirtualDownloadError,
};
use keyring::v1::Entry;
use oauth2::{TokenResponse, basic::BasicTokenResponse};
use onedrive_cache::{CacheError, CacheStore};
use onedrive_catalog::{Catalog, CatalogItem, FetchError};
use onedrive_core::{BUS_NAME, EngineState, OBJECT_PATH};
use onedrive_mount::{DownloadRequest, VirtualTree};
use onedrive_mount::{
    directory::DirectoryStore,
    draft::{DraftStore, DraftUploadFailure, DraftUploadIssue},
    fuse::{PublishedVersionController, VirtualFuse},
    mutation::{Mutation, MutationKind, MutationStatus, MutationStore},
    preflight,
};
use onedrive_sync::{
    Action, Baseline, QueuedOperation, SyncStore,
    folders::local_directory_exists,
    local_delete::{DeleteError, quarantine},
    local_rename::{RenameError, apply as apply_local_rename, fingerprint_matches, path_is_absent},
    monitor::LocalMonitor,
    remote_files, remote_parent_exists, remote_parent_id, scan_local,
    upload::{UploadError, UploadStep, upload_queued},
};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    time::{Duration, timeout},
};
use zbus::connection::Builder;

const KEYRING_SERVICE: &str = "io.github.onedrive_linux";
const KEYRING_USER: &str = "personal";
const VIRTUAL_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
const MIN_VIRTUAL_CACHE_BYTES: u64 = VIRTUAL_CACHE_BYTES / 4;
const MAX_VIRTUAL_CACHE_BYTES: u64 = VIRTUAL_CACHE_BYTES * 64;

struct BusyFlagGuard(Arc<AtomicBool>);

impl Drop for BusyFlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct TransferStateGuard {
    state: Arc<Mutex<EngineState>>,
    complete: bool,
}

impl TransferStateGuard {
    fn start(state: &Arc<Mutex<EngineState>>) -> Self {
        *state.lock().unwrap() = EngineState::Transferring;
        Self {
            state: Arc::clone(state),
            complete: false,
        }
    }

    fn finish(&mut self) {
        *self.state.lock().unwrap() = EngineState::Connected;
        self.complete = true;
    }

    fn offline(&mut self) {
        *self.state.lock().unwrap() = EngineState::Offline;
        self.complete = true;
    }
}

impl Drop for TransferStateGuard {
    fn drop(&mut self) {
        if !self.complete {
            *self.state.lock().unwrap() = EngineState::Error;
        }
    }
}

fn publish_virtual_file(
    cache_root: &std::path::Path,
    request: &DownloadRequest,
    downloaded: &std::path::Path,
) -> Result<(), CacheError> {
    let mut cache = CacheStore::open(cache_root, VIRTUAL_CACHE_BYTES)?;
    cache.put(&request.cache_key(), downloaded, request.size)
}

fn inspect_virtual_cache(
    cache_root: &std::path::Path,
    request: &DownloadRequest,
) -> Result<(bool, u64), CacheError> {
    let mut cache = CacheStore::open(cache_root, VIRTUAL_CACHE_BYTES)?;
    let cached = match cache.get(&request.cache_key())? {
        Some(file) => file.metadata()?.len() == request.size,
        None => false,
    };
    Ok((cached, cache.limit_bytes()))
}

#[derive(Clone)]
struct Daemon {
    state: Arc<Mutex<EngineState>>,
    login_in_progress: Arc<AtomicBool>,
    catalog: Arc<Mutex<Catalog>>,
    sync: Arc<Mutex<SyncStore>>,
    scan_in_progress: Arc<AtomicBool>,
    download_in_progress: Arc<AtomicBool>,
    upload_in_progress: Arc<AtomicBool>,
    local_change_epoch: Arc<AtomicU64>,
    virtual_mount: Arc<Mutex<Option<MountedVirtualDrive>>>,
    mount_in_progress: Arc<AtomicBool>,
}

struct MountedVirtualDrive {
    path: PathBuf,
    session: fuser::BackgroundSession,
    published_versions: PublishedVersionController,
}

#[derive(Clone)]
struct VirtualDownloadContext {
    state: Arc<Mutex<EngineState>>,
    catalog: Arc<Mutex<Catalog>>,
    download_in_progress: Arc<AtomicBool>,
    upload_in_progress: Arc<AtomicBool>,
}

impl From<&Daemon> for VirtualDownloadContext {
    fn from(daemon: &Daemon) -> Self {
        Self {
            state: Arc::clone(&daemon.state),
            catalog: Arc::clone(&daemon.catalog),
            download_in_progress: Arc::clone(&daemon.download_in_progress),
            upload_in_progress: Arc::clone(&daemon.upload_in_progress),
        }
    }
}

async fn prepare_virtual_request(
    context: &VirtualDownloadContext,
    request: DownloadRequest,
) -> zbus::fdo::Result<String> {
    if context.upload_in_progress.load(Ordering::SeqCst) {
        return Err(zbus::fdo::Error::Failed("upload is running".to_owned()));
    }
    let current = {
        let catalog = context.catalog.lock().unwrap();
        let (_, tree) = VirtualTree::from_catalog(&catalog)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        tree.request_for_item(&request.item_id)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
    };
    if current != request {
        return Err(zbus::fdo::Error::Failed(
            "virtual file version changed".to_owned(),
        ));
    }
    if context
        .download_in_progress
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(zbus::fdo::Error::Failed(
            "download is already running".to_owned(),
        ));
    }
    let _guard = BusyFlagGuard(Arc::clone(&context.download_in_progress));
    let state_root = state_directory()
        .map_err(|_| zbus::fdo::Error::Failed("cannot access state directory".to_owned()))?;
    let cache_root = state_root.join("virtual-cache");
    let request_for_check = request.clone();
    let cache_root_for_check = cache_root.clone();
    let (already_cached, cache_limit) = tokio::task::spawn_blocking(move || {
        inspect_virtual_cache(&cache_root_for_check, &request_for_check)
    })
    .await
    .map_err(|_| zbus::fdo::Error::Failed("cache check failed".to_owned()))?
    .map_err(|_| zbus::fdo::Error::Failed("cannot read virtual cache".to_owned()))?;
    if already_cached {
        return Ok("cached".to_owned());
    }
    if request.size > cache_limit {
        return Err(zbus::fdo::Error::Failed(
            "file exceeds virtual cache limit".to_owned(),
        ));
    }
    let client_id = client_id().ok_or_else(|| {
        zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
    })?;
    let token = match access_token_detailed(&client_id).await {
        Ok(token) => token,
        Err(TokenAccessError::Offline) => {
            *context.state.lock().unwrap() = EngineState::Offline;
            return Err(zbus::fdo::Error::NoNetwork(
                "file is not cached and OneDrive is unavailable".to_owned(),
            ));
        }
        Err(TokenAccessError::Auth) => {
            return Err(zbus::fdo::Error::Failed("sign-in is required".to_owned()));
        }
    };
    let staging = tempfile::tempdir_in(&state_root)
        .map_err(|_| zbus::fdo::Error::Failed("cannot stage virtual file".to_owned()))?;
    let target = staging.path().join("content");
    let graph = GraphClient::new()
        .map_err(|_| zbus::fdo::Error::Failed("cannot create Graph client".to_owned()))?;
    let mut transfer_state = TransferStateGuard::start(&context.state);
    match graph.download_cache_item(&request, &token, &target).await {
        Ok(()) => {}
        Err(VirtualDownloadError::Offline) => {
            transfer_state.offline();
            return Err(zbus::fdo::Error::NoNetwork(
                "file is not cached and OneDrive is unavailable".to_owned(),
            ));
        }
        Err(VirtualDownloadError::Changed) => {
            return Err(zbus::fdo::Error::Failed(
                "virtual file version changed".to_owned(),
            ));
        }
        Err(VirtualDownloadError::Failed) => {
            return Err(zbus::fdo::Error::Failed(
                "virtual file download failed".to_owned(),
            ));
        }
    }
    let cache_root_for_put = cache_root;
    let stored = tokio::task::spawn_blocking(move || {
        publish_virtual_file(&cache_root_for_put, &request, &target)
    })
    .await
    .map_err(|_| zbus::fdo::Error::Failed("cache write failed".to_owned()))?
    .map_err(|_| zbus::fdo::Error::Failed("cannot publish virtual file".to_owned()));
    stored?;
    transfer_state.finish();
    Ok("downloaded".to_owned())
}

fn virtual_request_for_item(
    catalog: &Arc<Mutex<Catalog>>,
    item_id: &str,
) -> zbus::fdo::Result<DownloadRequest> {
    let catalog = catalog.lock().unwrap();
    let (_, tree) = VirtualTree::from_catalog(&catalog)
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
    tree.request_for_item(item_id)
        .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))
}

fn resolved_virtual_parent(root: &std::path::Path, parent_id: &str) -> io::Result<String> {
    if !parent_id.starts_with("local-dir:") {
        return Ok(parent_id.to_owned());
    }
    DirectoryStore::open(root)?
        .remote_parent_id(parent_id)?
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
}

impl Daemon {
    async fn run_virtual_directory_upload(&self, item_id: String) -> zbus::fdo::Result<String> {
        if self
            .sync
            .lock()
            .unwrap()
            .root()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .is_some()
        {
            return Err(zbus::fdo::Error::Failed(
                "virtual upload cannot run while folder sync is configured".to_owned(),
            ));
        }
        if item_id.is_empty()
            || self.mount_in_progress.load(Ordering::SeqCst)
            || self.sync_paused()?
            || self.scan_in_progress.load(Ordering::SeqCst)
            || self
                .upload_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "virtual directory upload is busy or unavailable".to_owned(),
            ));
        }
        let _busy = BusyFlagGuard(Arc::clone(&self.upload_in_progress));
        if self.mount_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed(
                "mount operation is running".to_owned(),
            ));
        }
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let store = DirectoryStore::open(&root)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let directory = store
            .list()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .into_iter()
            .find(|directory| directory.item_id == item_id && directory.remote_id.is_none())
            .ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs("local directory is missing".to_owned())
            })?;
        let mounted_controller = self
            .virtual_mount
            .lock()
            .unwrap()
            .as_ref()
            .map(|mounted| mounted.published_versions.clone());
        let publication = mounted_controller
            .as_ref()
            .map(|controller| {
                controller
                    .try_begin_directory(&directory.drive_id, &directory.item_id)
                    .map_err(|_| {
                        zbus::fdo::Error::Failed(
                            "virtual directory is busy or missing from the mount".to_owned(),
                        )
                    })
            })
            .transpose()?;
        let parent_id = if directory.parent_id.starts_with("local-dir:") {
            store
                .list()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .into_iter()
                .find(|parent| {
                    parent.item_id == directory.parent_id && parent.drive_id == directory.drive_id
                })
                .and_then(|parent| parent.remote_id)
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("parent directory has not been uploaded".to_owned())
                })?
        } else {
            directory.parent_id.clone()
        };
        {
            let catalog = self.catalog.lock().unwrap();
            if catalog.active_drive_id().ok().flatten().as_deref() != Some(&directory.drive_id)
                || !catalog.is_complete(&directory.drive_id).unwrap_or(false)
            {
                return Err(zbus::fdo::Error::Failed("catalog is not ready".to_owned()));
            }
            let parent_exists = catalog
                .root_id(&directory.drive_id)
                .ok()
                .flatten()
                .as_deref()
                == Some(&parent_id)
                || catalog
                    .item(&directory.drive_id, &parent_id)
                    .ok()
                    .flatten()
                    .is_some_and(|item| !item.deleted && item.kind == "folder");
            let name_taken = catalog
                .items(&directory.drive_id)
                .ok()
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        !item.deleted
                            && item.parent_id.as_deref() == Some(&parent_id)
                            && item.name.as_deref() == Some(&directory.name)
                            && Some(item.item_id.as_str()) != directory.created_remote_id.as_deref()
                    })
                });
            if !parent_exists || name_taken {
                return Err(zbus::fdo::Error::Failed(
                    "directory needs review before upload".to_owned(),
                ));
            }
        }
        let client_id = client_id()
            .ok_or_else(|| zbus::fdo::Error::Failed("client ID is missing".to_owned()))?;
        let token = access_token_detailed(&client_id)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("login is required".to_owned()))?;
        let graph = GraphClient::new()
            .map_err(|_| zbus::fdo::Error::Failed("Graph client failed".to_owned()))?;
        if graph.drive_id(&token).await.ok().as_deref() != Some(&directory.drive_id) {
            return Err(zbus::fdo::Error::Failed("active drive changed".to_owned()));
        }
        let (directory, remote_id) = if let Some(remote_id) = &directory.created_remote_id {
            (directory.clone(), remote_id.clone())
        } else {
            let remote_id = graph
                .create_remote_folder(&directory.drive_id, &parent_id, &directory.name, &token)
                .await
                .map_err(|error| {
                    zbus::fdo::Error::Failed(format!("cannot create remote directory: {error:?}"))
                })?;
            let created = store
                .record_created(&directory, &remote_id)
                .map_err(|error| {
                    zbus::fdo::Error::Failed(format!(
                        "folder created but its ID was not saved; review required: {error}"
                    ))
                })?;
            (created, remote_id)
        };
        let etag = graph
            .created_virtual_directory_etag(
                &directory.drive_id,
                &remote_id,
                &parent_id,
                &directory.name,
                &token,
            )
            .await
            .map_err(|_| {
                zbus::fdo::Error::Failed(
                    "cannot verify created directory; local mapping retained".to_owned(),
                )
            })?
            .ok_or_else(|| {
                zbus::fdo::Error::Failed(
                    "created directory needs review; local mapping retained".to_owned(),
                )
            })?;
        if let (Some(controller), Some(lease)) = (&mounted_controller, &publication) {
            controller
                .validate_directory(lease, &directory.item_id, &remote_id, &etag)
                .map_err(|error| {
                    zbus::fdo::Error::Failed(format!(
                        "mounted directory cannot be published; local mapping retained: {error}"
                    ))
                })?;
        }
        store
            .mark_uploaded(&directory, &remote_id)
            .map_err(|error| {
                zbus::fdo::Error::Failed(format!("folder created but mapping not saved: {error}"))
            })?;
        if let (Some(controller), Some(lease)) = (&mounted_controller, &publication) {
            controller
                .publish_directory(lease, &directory.item_id, &remote_id, &etag)
                .map_err(|error| {
                    zbus::fdo::Error::Failed(format!(
                        "folder created but mounted directory was not updated: {error}"
                    ))
                })?;
        }
        Ok(remote_id)
    }

    fn virtual_draft_auto_upload_enabled(&self) -> bool {
        self.virtual_draft_upload_enabled()
            && std::env::var("ONEDRIVE_EXPERIMENTAL_VIRTUAL_AUTO_UPLOAD").as_deref() == Ok("1")
    }

    /// Advisory catalog view. Upload must recheck the remote eTag before writing.
    fn virtual_draft_statuses(&self) -> zbus::fdo::Result<Vec<virtual_drafts::VirtualDraftStatus>> {
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let store =
            DraftStore::open(&root).map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let drafts = store
            .list()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let catalog = self.catalog.lock().unwrap();
        let active_drive = catalog
            .active_drive_id()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let complete = match &active_drive {
            Some(drive_id) => catalog
                .is_complete(drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?,
            None => false,
        };
        drafts
            .iter()
            .map(|draft| {
                let item = if active_drive.as_deref() == Some(draft.drive_id.as_str())
                    && complete
                    && draft.new_file.is_none()
                {
                    catalog
                        .item(&draft.drive_id, &draft.item_id)
                        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                } else {
                    None
                };
                let mut status = if let Some(location) = &draft.new_file {
                    let parent_id = resolved_virtual_parent(&root, &location.parent_id)
                        .unwrap_or_else(|_| location.parent_id.clone());
                    let status = if active_drive.as_deref() != Some(draft.drive_id.as_str()) {
                        "other-drive"
                    } else if !complete {
                        "catalog-incomplete"
                    } else {
                        let parent_exists = catalog
                            .root_id(&draft.drive_id)
                            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                            .as_deref()
                            == Some(&parent_id)
                            || catalog
                                .item(&draft.drive_id, &parent_id)
                                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                                .is_some_and(|parent| parent.kind == "folder" && !parent.deleted);
                        let name_taken = catalog
                            .items(&draft.drive_id)
                            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                            .iter()
                            .any(|item| {
                                !item.deleted
                                    && item.parent_id.as_deref() == Some(&parent_id)
                                    && item.name.as_deref() == Some(&location.name)
                            });
                        if !parent_exists {
                            "remote-missing"
                        } else if name_taken {
                            "remote-changed"
                        } else if store
                            .recover(draft)
                            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                            .ok_or_else(|| zbus::fdo::Error::Failed("draft is missing".to_owned()))?
                            .len()
                            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                            == 0
                            && std::env::var("ONEDRIVE_EXPERIMENTAL_EMPTY_UPLOAD").as_deref()
                                != Ok("1")
                        {
                            "new-empty"
                        } else {
                            "ready-new"
                        }
                    };
                    virtual_drafts::VirtualDraftStatus {
                        drive_id: draft.drive_id.clone(),
                        item_id: draft.item_id.clone(),
                        name: Some(location.name.clone()),
                        status,
                        last_upload_issue: None,
                    }
                } else {
                    virtual_drafts::classify(
                        draft,
                        active_drive.as_deref(),
                        complete,
                        item.as_ref(),
                    )
                };
                status.last_upload_issue = store
                    .upload_failure(draft)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                    .map(|failure| failure.issue);
                Ok(status)
            })
            .collect::<zbus::fdo::Result<Vec<_>>>()
    }

    fn virtual_auto_upload_candidate(&self, after: Option<&str>) -> Option<(String, String)> {
        if !self.virtual_draft_auto_upload_enabled()
            || !matches!(
                *self.state.lock().unwrap(),
                EngineState::Connected | EngineState::Synced
            )
            || self.sync_paused().ok()?
            || self.sync.lock().unwrap().root().ok()?.is_some()
            || self.mount_in_progress.load(Ordering::SeqCst)
            || self.scan_in_progress.load(Ordering::SeqCst)
            || self.download_in_progress.load(Ordering::SeqCst)
            || self.upload_in_progress.load(Ordering::SeqCst)
        {
            return None;
        }
        let statuses = self.virtual_draft_statuses().ok()?;
        let item_id = virtual_drafts::next_ready_item_id(&statuses, after)?;
        statuses
            .iter()
            .find(|draft| draft.item_id == item_id && matches!(draft.status, "ready" | "ready-new"))
            .map(|draft| (draft.drive_id.clone(), draft.item_id.clone()))
    }
}

#[zbus::interface(name = "io.github.onedrive_linux.Daemon")]
impl Daemon {
    fn virtual_directories(&self) -> zbus::fdo::Result<String> {
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let directories = DirectoryStore::open(&root)
            .and_then(|store| store.list())
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let catalog = self.catalog.lock().unwrap();
        let active_drive = catalog
            .active_drive_id()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let statuses: Vec<_> = directories
            .iter()
            .map(|directory| {
                let status = if active_drive.as_deref() != Some(&directory.drive_id) {
                    "other-drive"
                } else if directory.remote_id.is_some() {
                    "uploaded"
                } else if !catalog.is_complete(&directory.drive_id).unwrap_or(false) {
                    "catalog-incomplete"
                } else {
                    let parent_id = if directory.parent_id.starts_with("local-dir:") {
                        directories
                            .iter()
                            .find(|parent| {
                                parent.drive_id == directory.drive_id
                                    && parent.item_id == directory.parent_id
                            })
                            .and_then(|parent| parent.remote_id.as_deref())
                    } else {
                        Some(directory.parent_id.as_str())
                    };
                    if let Some(parent_id) = parent_id {
                        let parent_exists = catalog
                            .root_id(&directory.drive_id)
                            .ok()
                            .flatten()
                            .as_deref()
                            == Some(parent_id)
                            || catalog
                                .item(&directory.drive_id, parent_id)
                                .ok()
                                .flatten()
                                .is_some_and(|item| !item.deleted && item.kind == "folder");
                        let name_taken =
                            catalog
                                .items(&directory.drive_id)
                                .ok()
                                .is_some_and(|items| {
                                    items.iter().any(|item| {
                                        !item.deleted
                                            && item.parent_id.as_deref() == Some(parent_id)
                                            && item.name.as_deref() == Some(&directory.name)
                                            && Some(item.item_id.as_str())
                                                != directory.created_remote_id.as_deref()
                                    })
                                });
                        if !parent_exists {
                            "parent-missing"
                        } else if name_taken {
                            "name-conflict"
                        } else if directory.created_remote_id.is_some() {
                            "verifying"
                        } else {
                            "pending"
                        }
                    } else {
                        "waiting-parent"
                    }
                };
                serde_json::json!({
                    "drive_id": directory.drive_id,
                    "item_id": directory.item_id,
                    "name": directory.name,
                    "status": status,
                })
            })
            .collect();
        serde_json::to_string(&statuses)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn virtual_mutations(&self) -> zbus::fdo::Result<String> {
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let mutations = if root.exists() {
            MutationStore::open(&root)
                .and_then(|store| store.list())
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
        } else {
            Vec::new()
        };
        serde_json::to_string(&mutations)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn cancel_conflicted_virtual_mutation(
        &self,
        drive_id: String,
        item_id: String,
    ) -> zbus::fdo::Result<()> {
        if drive_id.is_empty() || item_id.is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "item ID is missing".to_owned(),
            ));
        }
        if self.virtual_mount.lock().unwrap().is_some()
            || self.mount_in_progress.load(Ordering::SeqCst)
        {
            return Err(zbus::fdo::Error::Failed(
                "unmount the virtual drive before cancelling a conflict".to_owned(),
            ));
        }
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        MutationStore::open(&root)
            .and_then(|store| store.remove_conflict(&drive_id, &item_id))
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn confirm_virtual_mass_delete(&self, expected_count: u64) -> zbus::fdo::Result<()> {
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let count = usize::try_from(expected_count)
            .map_err(|_| zbus::fdo::Error::InvalidArgs("invalid deletion count".to_owned()))?;
        MutationStore::open(&root)
            .and_then(|store| store.approve_deletions(count))
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    /// UI capability flag only; RunVirtualDraftUpload repeats all safety checks.
    fn virtual_draft_upload_enabled(&self) -> bool {
        std::env::var("ONEDRIVE_EXPERIMENTAL_VIRTUAL_UPLOAD").as_deref() == Ok("1")
    }

    fn virtual_draft_count(&self) -> zbus::fdo::Result<u64> {
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let count = DraftStore::open(&root)
            .and_then(|store| store.list())
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .len();
        u64::try_from(count).map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn virtual_drafts(&self) -> zbus::fdo::Result<String> {
        let statuses = self.virtual_draft_statuses()?;
        serde_json::to_string(&statuses)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    /// Export a local copy without removing the draft or contacting OneDrive.
    async fn export_virtual_draft(
        &self,
        drive_id: String,
        item_id: String,
        destination: String,
    ) -> zbus::fdo::Result<()> {
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        tokio::task::spawn_blocking(move || {
            DraftStore::open(&root)?.export_item(
                &drive_id,
                &item_id,
                std::path::Path::new(&destination),
            )
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("draft export task failed".to_owned()))?
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    /// Explicit experimental transfer of one existing virtual-drive draft.
    async fn run_virtual_draft_upload(&self, item_id: String) -> zbus::fdo::Result<String> {
        if !self.virtual_draft_upload_enabled() {
            return Err(zbus::fdo::Error::Failed(
                "virtual draft upload is experimental and disabled".to_owned(),
            ));
        }
        if item_id.is_empty() || self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed(
                "invalid item or sync is paused".to_owned(),
            ));
        }
        if self
            .sync
            .lock()
            .unwrap()
            .root()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .is_some()
        {
            return Err(zbus::fdo::Error::Failed(
                "virtual upload cannot run while folder sync is configured".to_owned(),
            ));
        }
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.download_in_progress.load(Ordering::SeqCst)
            || self
                .upload_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "transfer is already running".to_owned(),
            ));
        }
        let _busy = BusyFlagGuard(Arc::clone(&self.upload_in_progress));
        if self.mount_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed(
                "mount operation is running".to_owned(),
            ));
        }
        let (drive_id, draft, resolved_parent) = {
            let catalog = self.catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            {
                return Err(zbus::fdo::Error::Failed("catalog is incomplete".to_owned()));
            }
            let root = state_directory()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .join("virtual-drafts");
            let store = DraftStore::open(&root)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            let draft = store
                .recover_item(&drive_id, &item_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("draft is missing".to_owned()))?;
            let remote = catalog
                .item(&drive_id, &item_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            let resolved_parent = draft
                .identity()
                .new_file
                .as_ref()
                .map(|location| resolved_virtual_parent(&root, &location.parent_id))
                .transpose()
                .map_err(|_| {
                    zbus::fdo::Error::Failed("parent directory has not been uploaded".to_owned())
                })?;
            let ready = if let Some(location) = &draft.identity().new_file {
                let parent_id = resolved_parent.as_ref().expect("new file has parent");
                let parent_exists = catalog
                    .root_id(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                    .as_deref()
                    == Some(parent_id)
                    || catalog
                        .item(&drive_id, parent_id)
                        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                        .is_some_and(|parent| parent.kind == "folder" && !parent.deleted);
                let name_taken = catalog
                    .items(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                    .iter()
                    .any(|item| {
                        !item.deleted
                            && item.parent_id.as_deref() == Some(parent_id)
                            && item.name.as_deref() == Some(&location.name)
                    });
                parent_exists && !name_taken
            } else {
                virtual_drafts::classify(draft.identity(), Some(&drive_id), true, remote.as_ref())
                    .status
                    == "ready"
            };
            if !ready {
                return Err(zbus::fdo::Error::Failed(
                    "draft needs review before upload".to_owned(),
                ));
            }
            (drive_id, draft.identity().clone(), resolved_parent)
        };
        let mounted_controller = self
            .virtual_mount
            .lock()
            .unwrap()
            .as_ref()
            .map(|mounted| mounted.published_versions.clone());
        let _publication = if let Some(controller) = &mounted_controller {
            Some(
                (if draft.new_file.is_some() {
                    controller.try_begin_new(&drive_id, &item_id)
                } else {
                    controller.try_begin_existing(&drive_id, &item_id)
                })
                .map_err(|_| {
                    zbus::fdo::Error::Failed(
                        "close the virtual file before uploading its draft".to_owned(),
                    )
                })?,
            )
        } else {
            None
        };
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        let token = access_token(&client_id)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("login is required".to_owned()))?;
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let snapshot =
            tokio::task::spawn_blocking(move || DraftStore::open(&root)?.snapshot(&draft))
                .await
                .map_err(|_| zbus::fdo::Error::Failed("draft snapshot task failed".to_owned()))?
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("draft is missing".to_owned()))?;
        if snapshot.size == 0
            && std::env::var("ONEDRIVE_EXPERIMENTAL_EMPTY_UPLOAD").as_deref() != Ok("1")
        {
            return Err(zbus::fdo::Error::Failed(
                "empty draft upload requires ONEDRIVE_EXPERIMENTAL_EMPTY_UPLOAD=1".to_owned(),
            ));
        }
        let graph = GraphClient::new()
            .map_err(|_| zbus::fdo::Error::Failed("cannot create Graph client".to_owned()))?;
        if snapshot.identity.new_file.is_none()
            && !graph
                .matches_remote_file_version(
                    &drive_id,
                    &item_id,
                    &snapshot.identity.base_etag,
                    &token,
                )
                .await
                .map_err(|_| zbus::fdo::Error::Failed("cannot verify remote version".to_owned()))?
        {
            return Err(zbus::fdo::Error::Failed(
                "remote file changed; local draft retained".to_owned(),
            ));
        }
        let transport = GraphUpload::new(&graph, &drive_id, &token)
            .map_err(|_| zbus::fdo::Error::Failed("cannot create upload transport".to_owned()))?;
        let checkpoint_root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let checkpoint_store = DraftStore::open(&checkpoint_root)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let mut transfer_state = TransferStateGuard::start(&self.state);
        let result = if let Some(parent_id) = resolved_parent.as_deref() {
            virtual_upload::upload_new_snapshot_to_parent(
                &transport,
                &checkpoint_store,
                &snapshot,
                parent_id,
            )
            .await
        } else {
            virtual_upload::upload_snapshot(&transport, &checkpoint_store, &snapshot).await
        }
        .map_err(|_| zbus::fdo::Error::Failed("upload failed; local draft retained".to_owned()))?;
        let UploadStep::Complete {
            remote_id: uploaded_id,
            etag: Some(uploaded_etag),
            ..
        } = result
        else {
            return Err(zbus::fdo::Error::Failed(
                "upload result needs review; local draft retained".to_owned(),
            ));
        };
        let catalog_recorded = if let Some(location) = &snapshot.identity.new_file {
            if !graph
                .matches_created_virtual_file(
                    &drive_id,
                    &CreatedVirtualExpectation {
                        item_id: &uploaded_id,
                        parent_id: resolved_parent.as_deref().unwrap_or(&location.parent_id),
                        name: &location.name,
                        etag: &uploaded_etag,
                        size: snapshot.size,
                    },
                    &token,
                )
                .await
                .map_err(|_| {
                    zbus::fdo::Error::Failed(
                        "cannot verify created file; draft retained".to_owned(),
                    )
                })?
            {
                return Err(zbus::fdo::Error::Failed(
                    "created file needs review; draft retained".to_owned(),
                ));
            }
            self.catalog.lock().unwrap().record_created_file(
                &drive_id,
                &uploaded_id,
                resolved_parent.as_deref().unwrap_or(&location.parent_id),
                &location.name,
                &uploaded_etag,
                snapshot.size,
            )
        } else {
            self.catalog.lock().unwrap().record_uploaded_version(
                &drive_id,
                &item_id,
                &snapshot.identity.base_etag,
                &uploaded_etag,
                snapshot.size,
            )
        }
        .map_err(|_| {
            zbus::fdo::Error::Failed("catalog update failed; draft retained".to_owned())
        })?;
        if !catalog_recorded {
            return Err(zbus::fdo::Error::Failed(
                "catalog changed during upload; local draft retained".to_owned(),
            ));
        }
        if let Some(controller) = &mounted_controller {
            let request = DownloadRequest {
                drive_id: drive_id.clone(),
                item_id: if snapshot.identity.new_file.is_some() {
                    uploaded_id.clone()
                } else {
                    item_id.clone()
                },
                etag: uploaded_etag.clone(),
                size: snapshot.size,
            };
            let cache_root = state_directory()
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .join("virtual-cache");
            let snapshot_path = snapshot.file.path().to_path_buf();
            tokio::task::spawn_blocking(move || {
                CacheStore::open_waiting(&cache_root, VIRTUAL_CACHE_BYTES).and_then(|mut cache| {
                    cache.put(&request.cache_key(), &snapshot_path, request.size)
                })
            })
            .await
            .map_err(|_| {
                zbus::fdo::Error::Failed("cache task failed; local draft retained".to_owned())
            })?
            .map_err(|error| {
                zbus::fdo::Error::Failed(format!(
                    "uploaded file could not be cached; local draft retained: {error}"
                ))
            })?;
            if snapshot.identity.new_file.is_some() {
                controller
                    .validate_new(
                        _publication.as_ref().expect("mounted publication lease"),
                        &item_id,
                        &uploaded_id,
                        &uploaded_etag,
                    )
                    .map_err(|error| {
                        zbus::fdo::Error::Failed(format!(
                            "mounted file cannot be published; local draft retained: {error}"
                        ))
                    })?;
            } else {
                controller.publish_existing(&item_id, uploaded_etag.clone(), snapshot.size);
            }
        }
        let root = state_directory()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .join("virtual-drafts");
        let completed_identity = snapshot.identity.clone();
        let uploaded_new_file = snapshot.identity.new_file.is_some();
        let uploaded_size = snapshot.size;
        let acknowledged =
            tokio::task::spawn_blocking(move || DraftStore::open(&root)?.acknowledge(&snapshot))
                .await
                .map_err(|_| {
                    zbus::fdo::Error::Failed("draft acknowledgement task failed".to_owned())
                })?
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        if acknowledged
            && uploaded_new_file
            && let Some(controller) = &mounted_controller
        {
            controller
                .publish_new(
                    _publication.as_ref().expect("mounted publication lease"),
                    &item_id,
                    &uploaded_id,
                    uploaded_etag,
                    uploaded_size,
                )
                .map_err(|error| {
                    zbus::fdo::Error::Failed(format!(
                        "mounted file cannot be published after upload: {error}"
                    ))
                })?;
        }
        checkpoint_store
            .clear_upload_checkpoint(&completed_identity)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        if !acknowledged
            && checkpoint_store
                .clear_upload_failure(&completed_identity)
                .is_err()
        {
            eprintln!("cannot clear previous virtual draft upload failure");
        }
        transfer_state.finish();
        Ok(if acknowledged {
            "uploaded".to_owned()
        } else {
            "uploaded-but-edited-locally".to_owned()
        })
    }

    /// Device status and optional unprivileged mount helper status.
    fn virtual_fuse_diagnostics(&self) -> (String, bool) {
        let device = match preflight::check_fuse_device() {
            Ok(()) => "ready",
            Err(preflight::MountError::FuseUnavailable) => "device-missing",
            Err(preflight::MountError::FusePermission) => "device-permission",
            Err(_) => "device-error",
        };
        (device.to_owned(), preflight::fuse_mount_helper_available())
    }

    fn virtual_mount_path(&self) -> String {
        self.virtual_mount
            .lock()
            .unwrap()
            .as_ref()
            .map(|mount| mount.path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn virtual_configured_mount_path(&self) -> zbus::fdo::Result<String> {
        let root =
            state_directory().map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        mount_record::configured(&root)
            .map(|path| {
                path.map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn virtual_mount_recovery_path(&self) -> zbus::fdo::Result<String> {
        if self.virtual_mount.lock().unwrap().is_some()
            || self.mount_in_progress.load(Ordering::SeqCst)
        {
            return Ok(String::new());
        }
        let root =
            state_directory().map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        mount_record::pending(&root)
            .map(|path| {
                path.map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    async fn mount_virtual_drive(&self, mount_path: &str) -> zbus::fdo::Result<String> {
        if self.upload_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("upload is running".to_owned()));
        }
        let mount_path = PathBuf::from(mount_path);
        if !mount_path.is_absolute() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "mount point must be absolute".to_owned(),
            ));
        }
        if self
            .mount_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "mount operation is running".to_owned(),
            ));
        }
        let _guard = BusyFlagGuard(Arc::clone(&self.mount_in_progress));
        if self.upload_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("upload is running".to_owned()));
        }
        if self.virtual_mount.lock().unwrap().is_some() {
            return Err(zbus::fdo::Error::Failed(
                "virtual drive is already mounted".to_owned(),
            ));
        }
        let sync_root = self
            .sync
            .lock()
            .unwrap()
            .root()
            .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?;
        let state_root = state_directory()
            .map_err(|_| zbus::fdo::Error::Failed("cannot access state directory".to_owned()))?;
        if let Some(previous) = mount_record::pending(&state_root)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
        {
            return Err(zbus::fdo::Error::Failed(format!(
                "previous virtual mount remains occupied: {}",
                previous.display()
            )));
        }
        let cache_root = state_root.join("virtual-cache");
        let draft_root = state_root.join("virtual-drafts");
        let mount_path =
            preflight::validate_mount_paths(&mount_path, sync_root.as_deref(), &cache_root)
                .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        onedrive_core::validate_distinct_paths(&[&mount_path, &draft_root])
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        mount_record::configure(&state_root, &mount_path).map_err(|error| {
            zbus::fdo::Error::Failed(format!("cannot save virtual mount path: {error}"))
        })?;
        preflight::check_fuse_device()
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let tree = {
            let catalog = self.catalog.lock().unwrap();
            VirtualTree::from_catalog(&catalog)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
                .1
        };
        let context = VirtualDownloadContext::from(self);
        let runtime = tokio::runtime::Handle::current();
        let loader = Arc::new(move |request: &DownloadRequest| -> io::Result<()> {
            let expected = request.clone();
            runtime
                .block_on(prepare_virtual_request(&context, expected))
                .map(|_| ())
                .map_err(|error| match error {
                    zbus::fdo::Error::NoNetwork(_) => io::ErrorKind::NotConnected.into(),
                    other => io::Error::other(other),
                })
        });
        let fuse =
            VirtualFuse::new_writable(tree, cache_root, draft_root, VIRTUAL_CACHE_BYTES, loader);
        let published_versions = fuse.published_version_controller();
        mount_record::write(&state_root, &mount_path).map_err(|error| {
            zbus::fdo::Error::Failed(format!("cannot record mount path: {error}"))
        })?;
        let path_for_mount = mount_path.clone();
        let mounted = tokio::task::spawn_blocking(move || fuse.mount(&path_for_mount)).await;
        let session = match mounted {
            Ok(Ok(session)) => session,
            other => {
                let _ = mount_record::pending(&state_root);
                return Err(zbus::fdo::Error::Failed(match other {
                    Ok(Err(error)) => format!("cannot mount virtual drive: {error}"),
                    Err(error) => format!("FUSE mount task failed: {error}"),
                    Ok(Ok(_)) => unreachable!(),
                }));
            }
        };
        *self.virtual_mount.lock().unwrap() = Some(MountedVirtualDrive {
            path: mount_path.clone(),
            session,
            published_versions,
        });
        Ok(mount_path.to_string_lossy().into_owned())
    }

    async fn unmount_virtual_drive(&self) -> zbus::fdo::Result<()> {
        if self
            .mount_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "mount operation is running".to_owned(),
            ));
        }
        let _guard = BusyFlagGuard(Arc::clone(&self.mount_in_progress));
        if self.upload_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("upload is running".to_owned()));
        }
        let mounted =
            self.virtual_mount.lock().unwrap().take().ok_or_else(|| {
                zbus::fdo::Error::Failed("virtual drive is not mounted".to_owned())
            })?;
        tokio::task::spawn_blocking(move || mounted.session.umount_and_join())
            .await
            .map_err(|_| zbus::fdo::Error::Failed("FUSE unmount task failed".to_owned()))?
            .map_err(|error| {
                zbus::fdo::Error::Failed(format!("cannot unmount virtual drive: {error}"))
            })?;
        let root =
            state_directory().map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        mount_record::clear(&root).map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    /// Fetch a file into the virtual-drive cache on demand. Returns "cached" or "downloaded".
    async fn prepare_virtual_file(&self, item_id: &str) -> zbus::fdo::Result<String> {
        let request = virtual_request_for_item(&self.catalog, item_id)?;
        let context = VirtualDownloadContext::from(self);
        prepare_virtual_request(&context, request).await
    }

    /// "missing", "cached" or "pinned" for the current catalog version.
    async fn virtual_file_cache_status(&self, item_id: &str) -> zbus::fdo::Result<String> {
        let request = virtual_request_for_item(&self.catalog, item_id)?;
        let root = state_directory()
            .map_err(|_| zbus::fdo::Error::Failed("cannot access state directory".to_owned()))?
            .join("virtual-cache");
        let state = tokio::task::spawn_blocking(move || {
            let mut cache = CacheStore::open(&root, VIRTUAL_CACHE_BYTES)?;
            cache.cached_pin_state(&request.cache_key())
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("cache status task failed".to_owned()))?
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok(match state {
            None => "missing",
            Some(false) => "cached",
            Some(true) => "pinned",
        }
        .to_owned())
    }

    /// Download the current version before pinning; unpinning never makes a network request.
    async fn set_virtual_file_pinned(
        &self,
        item_id: &str,
        pinned: bool,
    ) -> zbus::fdo::Result<String> {
        let request = virtual_request_for_item(&self.catalog, item_id)?;
        if pinned {
            let context = VirtualDownloadContext::from(self);
            prepare_virtual_request(&context, request.clone()).await?;
            if virtual_request_for_item(&self.catalog, item_id)? != request {
                return Err(zbus::fdo::Error::Failed(
                    "virtual file version changed before pinning".to_owned(),
                ));
            }
        }
        let root = state_directory()
            .map_err(|_| zbus::fdo::Error::Failed("cannot access state directory".to_owned()))?
            .join("virtual-cache");
        tokio::task::spawn_blocking(move || {
            let mut cache = CacheStore::open(&root, VIRTUAL_CACHE_BYTES)?;
            if cache.cached_pin_state(&request.cache_key())?.is_none() {
                return Err(CacheError::MissingEntry);
            }
            cache.set_pinned(&request.cache_key(), pinned)
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("cache pin task failed".to_owned()))?
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        Ok(if pinned { "pinned" } else { "cached" }.to_owned())
    }

    /// Current cache use and persistent limit, in bytes.
    async fn virtual_cache_stats(&self) -> zbus::fdo::Result<(u64, u64)> {
        let root = state_directory()
            .map_err(|_| zbus::fdo::Error::Failed("cannot access state directory".to_owned()))?
            .join("virtual-cache");
        tokio::task::spawn_blocking(move || {
            let cache = CacheStore::open(&root, VIRTUAL_CACHE_BYTES)?;
            Ok::<_, CacheError>((cache.usage_bytes()?, cache.limit_bytes()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("cache stats task failed".to_owned()))?
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    /// Apply a new limit immediately; pinned files can never be evicted to satisfy it.
    async fn set_virtual_cache_limit(&self, bytes: u64) -> zbus::fdo::Result<(u64, u64)> {
        if !(MIN_VIRTUAL_CACHE_BYTES..=MAX_VIRTUAL_CACHE_BYTES).contains(&bytes) {
            return Err(zbus::fdo::Error::InvalidArgs(
                "cache limit must be between 256 MiB and 64 GiB".to_owned(),
            ));
        }
        let root = state_directory()
            .map_err(|_| zbus::fdo::Error::Failed("cannot access state directory".to_owned()))?
            .join("virtual-cache");
        tokio::task::spawn_blocking(move || {
            let mut cache = CacheStore::open(&root, VIRTUAL_CACHE_BYTES)?;
            cache.set_limit(bytes)?;
            Ok::<_, CacheError>((cache.usage_bytes()?, cache.limit_bytes()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("cache limit task failed".to_owned()))?
        .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn status(&self) -> String {
        let state = *self.state.lock().unwrap();
        if state == EngineState::Connected {
            if self.sync.lock().unwrap().is_paused().unwrap_or(false) {
                return EngineState::Paused.as_str().to_owned();
            }
            let catalog = self.catalog.lock().unwrap();
            if let Ok(Some(drive_id)) = catalog.active_drive_id()
                && catalog.needs_reconcile(&drive_id).unwrap_or(false)
            {
                return EngineState::NeedsReconcile.as_str().to_owned();
            }
        }
        state.as_str().to_owned()
    }

    fn sync_paused(&self) -> zbus::fdo::Result<bool> {
        self.sync
            .lock()
            .unwrap()
            .is_paused()
            .map_err(|_| zbus::fdo::Error::Failed("cannot read sync pause".to_owned()))
    }

    fn set_sync_paused(&self, paused: bool) -> zbus::fdo::Result<()> {
        self.sync
            .lock()
            .unwrap()
            .set_paused(paused)
            .map_err(|_| zbus::fdo::Error::Failed("cannot save sync pause".to_owned()))
    }

    fn sync_review_count(&self) -> zbus::fdo::Result<u64> {
        self.sync
            .lock()
            .unwrap()
            .review_count()
            .map_err(|_| zbus::fdo::Error::Failed("cannot read sync review count".to_owned()))
    }

    fn sync_outstanding_count(&self) -> zbus::fdo::Result<u64> {
        self.sync
            .lock()
            .unwrap()
            .outstanding_count()
            .map_err(|_| zbus::fdo::Error::Failed("cannot read outstanding count".to_owned()))
    }

    fn review_operations(&self) -> zbus::fdo::Result<String> {
        let operations = self
            .sync
            .lock()
            .unwrap()
            .review_operations()
            .map_err(|_| zbus::fdo::Error::Failed("cannot read review queue".to_owned()))?;
        serde_json::to_string(&operations)
            .map_err(|_| zbus::fdo::Error::Failed("cannot encode review queue".to_owned()))
    }

    fn operation_history(&self) -> zbus::fdo::Result<String> {
        let history =
            self.sync.lock().unwrap().operation_history().map_err(|_| {
                zbus::fdo::Error::Failed("cannot read operation history".to_owned())
            })?;
        serde_json::to_string(&history)
            .map_err(|_| zbus::fdo::Error::Failed("cannot encode operation history".to_owned()))
    }

    fn approve_operation(&self, id: i64) -> zbus::fdo::Result<()> {
        self.sync
            .lock()
            .unwrap()
            .approve_operation(id)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))
    }

    fn dismiss_operation(&self, id: i64) -> zbus::fdo::Result<()> {
        self.sync
            .lock()
            .unwrap()
            .dismiss_operation(id)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))
    }

    fn local_change_epoch(&self) -> u64 {
        self.local_change_epoch.load(Ordering::Relaxed)
    }

    fn catalog_items(&self) -> zbus::fdo::Result<String> {
        let catalog = self.catalog.lock().unwrap();
        let drive_id = catalog
            .active_drive_id()
            .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
        let Some(drive_id) = drive_id else {
            return Ok("[]".to_owned());
        };
        let items = catalog
            .items(&drive_id)
            .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
        serde_json::to_string(&items)
            .map_err(|_| zbus::fdo::Error::Failed("cannot encode catalog".to_owned()))
    }

    fn set_sync_root(&self, root: &str) -> zbus::fdo::Result<String> {
        let state_directory = state_directory()
            .map_err(|_| zbus::fdo::Error::Failed("cannot access state directory".to_owned()))?;
        let path = self
            .sync
            .lock()
            .unwrap()
            .set_root(&PathBuf::from(root), &state_directory)
            .map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn sync_root(&self) -> zbus::fdo::Result<String> {
        self.sync
            .lock()
            .unwrap()
            .root()
            .map(|root| {
                root.map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))
    }

    async fn preview_sync(&self) -> zbus::fdo::Result<String> {
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?;
            drop(catalog);
            sync.lock()
                .unwrap()
                .ensure_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            let preview = sync
                .lock()
                .unwrap()
                .preview(&items, root_id.as_deref())
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            serde_json::to_string(&preview)
                .map_err(|_| zbus::fdo::Error::Failed("cannot encode preview".to_owned()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("preview task failed".to_owned()))?
    }

    async fn queue_safe_uploads(&self) -> zbus::fdo::Result<u64> {
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            drop(catalog);
            let mut sync = sync.lock().unwrap();
            sync.bind_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            sync.enqueue_safe_uploads(&items, &root_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("queue task failed".to_owned()))?
    }

    async fn queue_safe_downloads(&self) -> zbus::fdo::Result<u64> {
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            drop(catalog);
            let mut sync = sync.lock().unwrap();
            sync.bind_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            sync.enqueue_safe_downloads(&items, &root_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("queue task failed".to_owned()))?
    }

    async fn queue_safe_local_renames(&self) -> zbus::fdo::Result<u64> {
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            drop(catalog);
            let mut sync = sync.lock().unwrap();
            sync.bind_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            sync.enqueue_safe_local_renames(&items, &root_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("queue task failed".to_owned()))?
    }

    async fn queue_review_actions(&self) -> zbus::fdo::Result<u64> {
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            drop(catalog);
            let mut sync = sync.lock().unwrap();
            sync.bind_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            sync.enqueue_review_actions(&items, &root_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("review queue task failed".to_owned()))?
    }

    async fn prepare_local_folders(&self) -> zbus::fdo::Result<String> {
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            drop(catalog);
            let sync = sync.lock().unwrap();
            sync.bind_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            let result = sync
                .prepare_local_folders(&items, &root_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            serde_json::to_string(&result)
                .map_err(|_| zbus::fdo::Error::Failed("cannot encode folder result".to_owned()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("folder task failed".to_owned()))?
    }

    async fn queue_safe_remote_folders(&self) -> zbus::fdo::Result<u64> {
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            drop(catalog);
            let mut sync = sync.lock().unwrap();
            sync.bind_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            sync.enqueue_safe_remote_folders(&items, &root_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("folder queue task failed".to_owned()))?
    }

    async fn queue_upload_changed(&self, item_id: &str) -> zbus::fdo::Result<i64> {
        queue_upload_candidate(
            Arc::clone(&self.catalog),
            Arc::clone(&self.sync),
            Action::UploadChanged,
            item_id.to_owned(),
        )
        .await
    }

    async fn queue_upload_new(&self, path: &str) -> zbus::fdo::Result<i64> {
        queue_upload_candidate(
            Arc::clone(&self.catalog),
            Arc::clone(&self.sync),
            Action::UploadNew,
            path.to_owned(),
        )
        .await
    }

    async fn run_queued_remote_folder(&self) -> zbus::fdo::Result<i64> {
        if std::env::var("ONEDRIVE_EXPERIMENTAL_UPLOAD").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::Failed(
                "remote folder creation is experimental and disabled".to_owned(),
            ));
        }
        if self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed("sync is paused".to_owned()));
        }
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.download_in_progress.load(Ordering::SeqCst)
            || self
                .upload_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "another transfer is running".to_owned(),
            ));
        }
        let prepare = async {
            let token = access_token(&client_id)
                .await
                .map_err(|_| zbus::fdo::Error::Failed("login is required".to_owned()))?;
            let catalog = self.catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            let (operation, root) = {
                let mut sync = self.sync.lock().unwrap();
                sync.ensure_drive(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                let root = sync
                    .root()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("sync root is not configured".to_owned())
                    })?;
                let operation = sync
                    .claim_remote_folder()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read folder queue".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("no remote folder is pending".to_owned())
                    })?;
                (operation, root)
            };
            let candidate = remote_folder_candidate(&items, &root_id, &root, &operation);
            let Some((parent_id, name)) = candidate else {
                self.sync
                    .lock()
                    .unwrap()
                    .mark_blocked(operation.id, "remote folder needs review")
                    .map_err(|_| {
                        zbus::fdo::Error::Failed("cannot block stale folder".to_owned())
                    })?;
                return Err(zbus::fdo::Error::Failed(
                    "remote folder needs review".to_owned(),
                ));
            };
            Ok::<_, zbus::fdo::Error>((token, drive_id, operation, parent_id, name))
        }
        .await;
        let (token, drive_id, operation, parent_id, name) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.upload_in_progress.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let id = operation.id;
        let sync = Arc::clone(&self.sync);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.upload_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let result = async {
                let graph = GraphClient::new().map_err(|_| FolderCreateError::Retry)?;
                graph
                    .create_remote_folder(&drive_id, &parent_id, &name, &token)
                    .await?;
                sync.lock()
                    .unwrap()
                    .mark_done(id)
                    .map_err(|_| FolderCreateError::NeedsReconcile)
            }
            .await;
            if let Err(error) = &result {
                let update = match error {
                    FolderCreateError::Conflict | FolderCreateError::NeedsReconcile => sync
                        .lock()
                        .unwrap()
                        .mark_blocked(id, "remote folder needs reconciliation"),
                    FolderCreateError::Retry => sync
                        .lock()
                        .unwrap()
                        .mark_retry(id, "remote folder creation failed; retry is possible"),
                };
                if update.is_err() {
                    eprintln!("cannot update remote folder queue status");
                }
            }
            *state.lock().unwrap() = if result.is_ok() {
                EngineState::Connected
            } else {
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    async fn run_queued_remote_rename(&self) -> zbus::fdo::Result<i64> {
        if std::env::var("ONEDRIVE_EXPERIMENTAL_UPLOAD").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::Failed(
                "remote rename execution is experimental and disabled".to_owned(),
            ));
        }
        if self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed("sync is paused".to_owned()));
        }
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.download_in_progress.load(Ordering::SeqCst)
            || self
                .upload_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "another transfer is running".to_owned(),
            ));
        }
        let token = match access_token(&client_id).await {
            Ok(token) => token,
            Err(_) => {
                self.upload_in_progress.store(false, Ordering::SeqCst);
                return Err(zbus::fdo::Error::Failed("login is required".to_owned()));
            }
        };
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        let prepare = tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            drop(catalog);
            let (operation, root, baselines) = {
                let mut store = sync.lock().unwrap();
                store
                    .ensure_drive(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                let root = store
                    .root()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("sync root is not configured".to_owned())
                    })?;
                let operation = store
                    .claim_remote_rename()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read rename queue".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("no approved remote rename is pending".to_owned())
                    })?;
                let baselines = store.baselines().map_err(|_| {
                    zbus::fdo::Error::Failed("cannot read sync baseline".to_owned())
                })?;
                (operation, root, baselines)
            };
            let Some(candidate) =
                remote_rename_candidate(&items, &root_id, &root, &baselines, &operation)
            else {
                sync.lock()
                    .unwrap()
                    .mark_blocked(operation.id, "remote rename needs review")
                    .map_err(|_| {
                        zbus::fdo::Error::Failed("cannot block stale rename".to_owned())
                    })?;
                return Err(zbus::fdo::Error::Failed(
                    "remote rename needs review".to_owned(),
                ));
            };
            Ok::<_, zbus::fdo::Error>((drive_id, operation, root, candidate))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("rename task failed".to_owned()))?;
        let (drive_id, operation, root, candidate) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.upload_in_progress.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let id = operation.id;
        let sync = Arc::clone(&self.sync);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.upload_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let result = async {
                let graph = GraphClient::new().map_err(|_| RemoteRenameError::Retry)?;
                let etag = graph
                    .rename_remote_file(
                        &drive_id,
                        &RemoteRenameInput {
                            item_id: &candidate.item_id,
                            source_parent_id: &candidate.source_parent_id,
                            source_name: &candidate.source_name,
                            target_parent_id: &candidate.target_parent_id,
                            target_name: &candidate.target_name,
                            etag: &candidate.etag,
                        },
                        &token,
                    )
                    .await?;
                let local_path = operation.path.clone();
                let expected = candidate.fingerprint.clone();
                let unchanged = tokio::task::spawn_blocking(move || {
                    fingerprint_matches(&root, &local_path, &expected)
                })
                .await
                .map_err(|_| RemoteRenameError::NeedsReconcile)?;
                if !unchanged {
                    return Err(RemoteRenameError::NeedsReconcile);
                }
                sync.lock()
                    .unwrap()
                    .finish_remote_rename(&operation, &etag)
                    .map_err(|_| RemoteRenameError::NeedsReconcile)
            }
            .await;
            if let Err(error) = &result {
                let update = match error {
                    RemoteRenameError::Conflict | RemoteRenameError::NeedsReconcile => sync
                        .lock()
                        .unwrap()
                        .mark_blocked(id, "remote rename needs reconciliation"),
                    RemoteRenameError::Retry => sync
                        .lock()
                        .unwrap()
                        .mark_retry(id, "remote rename failed; retry is possible"),
                };
                if update.is_err() {
                    eprintln!("cannot update remote rename queue status");
                }
            }
            *state.lock().unwrap() = if result.is_ok() {
                EngineState::Connected
            } else {
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    async fn run_queued_remote_delete(&self) -> zbus::fdo::Result<i64> {
        if std::env::var("ONEDRIVE_EXPERIMENTAL_UPLOAD").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::Failed(
                "remote deletion is experimental and disabled".to_owned(),
            ));
        }
        if self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed("sync is paused".to_owned()));
        }
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.download_in_progress.load(Ordering::SeqCst)
            || self
                .upload_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "another transfer is running".to_owned(),
            ));
        }
        let token = match access_token(&client_id).await {
            Ok(token) => token,
            Err(_) => {
                self.upload_in_progress.store(false, Ordering::SeqCst);
                return Err(zbus::fdo::Error::Failed("login is required".to_owned()));
            }
        };
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        let prepare = tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            drop(catalog);
            let (operation, root, baselines) = {
                let mut store = sync.lock().unwrap();
                store
                    .ensure_drive(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                let root = store
                    .root()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("sync root is not configured".to_owned())
                    })?;
                let operation = store
                    .claim_remote_delete()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read delete queue".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed(
                            "no approved remote deletion is pending".to_owned(),
                        )
                    })?;
                let baselines = store.baselines().map_err(|_| {
                    zbus::fdo::Error::Failed("cannot read sync baseline".to_owned())
                })?;
                (operation, root, baselines)
            };
            let Some(candidate) =
                remote_delete_candidate(&items, &root_id, &root, &baselines, &operation)
            else {
                sync.lock()
                    .unwrap()
                    .mark_blocked(operation.id, "remote deletion needs review")
                    .map_err(|_| {
                        zbus::fdo::Error::Failed("cannot block stale deletion".to_owned())
                    })?;
                return Err(zbus::fdo::Error::Failed(
                    "remote deletion needs review".to_owned(),
                ));
            };
            Ok::<_, zbus::fdo::Error>((drive_id, operation, root, candidate))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("delete task failed".to_owned()))?;
        let (drive_id, operation, root, candidate) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.upload_in_progress.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let id = operation.id;
        let sync = Arc::clone(&self.sync);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.upload_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let result = async {
                let graph = GraphClient::new().map_err(|_| RemoteDeleteError::Retry)?;
                graph
                    .delete_remote_file(
                        &drive_id,
                        &candidate.item_id,
                        &candidate.parent_id,
                        &candidate.name,
                        &candidate.etag,
                        &token,
                    )
                    .await?;
                let local_path = operation.path.clone();
                let still_absent =
                    tokio::task::spawn_blocking(move || path_is_absent(&root, &local_path))
                        .await
                        .map_err(|_| RemoteDeleteError::NeedsReconcile)?;
                if !still_absent {
                    return Err(RemoteDeleteError::NeedsReconcile);
                }
                sync.lock()
                    .unwrap()
                    .finish_remote_deleted(&operation)
                    .map_err(|_| RemoteDeleteError::NeedsReconcile)
            }
            .await;
            if let Err(error) = &result {
                let update = match error {
                    RemoteDeleteError::Conflict | RemoteDeleteError::NeedsReconcile => sync
                        .lock()
                        .unwrap()
                        .mark_blocked(id, "remote deletion needs reconciliation"),
                    RemoteDeleteError::Retry => sync
                        .lock()
                        .unwrap()
                        .mark_retry(id, "remote deletion failed; retry is possible"),
                };
                if update.is_err() {
                    eprintln!("cannot update remote deletion queue status");
                }
            }
            *state.lock().unwrap() = if result.is_ok() {
                EngineState::Connected
            } else {
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    async fn run_queued_upload(&self) -> zbus::fdo::Result<i64> {
        if std::env::var("ONEDRIVE_EXPERIMENTAL_UPLOAD").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::Failed(
                "upload execution is experimental and disabled".to_owned(),
            ));
        }
        if self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed("sync is paused".to_owned()));
        }
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.download_in_progress.load(Ordering::SeqCst)
            || self
                .upload_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "upload is already running".to_owned(),
            ));
        }
        let prepare = async {
            let token = access_token(&client_id)
                .await
                .map_err(|_| zbus::fdo::Error::Failed("login is required".to_owned()))?;
            let drive_id = self
                .catalog
                .lock()
                .unwrap()
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            let (operation, root) = {
                let mut sync = self.sync.lock().unwrap();
                sync.ensure_drive(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                let root = sync
                    .root()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("sync root is not configured".to_owned())
                    })?;
                let operation = sync
                    .claim_upload()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read upload queue".to_owned()))?
                    .ok_or_else(|| zbus::fdo::Error::Failed("no upload is pending".to_owned()))?;
                (operation, root)
            };
            Ok::<_, zbus::fdo::Error>((token, drive_id, operation, root))
        }
        .await;
        let (token, drive_id, operation, root) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.upload_in_progress.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let id = operation.id;
        let sync = Arc::clone(&self.sync);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.upload_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let result = tokio::task::spawn_blocking(move || {
                let mut store = sync.lock().unwrap();
                let result = (|| {
                    let graph = GraphClient::new().map_err(|_| UploadError::Transient)?;
                    let transport = GraphUpload::new(&graph, &drive_id, &token)
                        .map_err(|_| UploadError::Transient)?;
                    tokio::runtime::Handle::current()
                        .block_on(upload_queued(&store, &transport, &root, &operation))
                })();
                match result {
                    Ok(UploadStep::Complete {
                        remote_id,
                        etag: Some(etag),
                    }) => store
                        .finish_uploaded(&operation, &remote_id, &etag)
                        .map_err(UploadError::Store),
                    Ok(_) => {
                        store
                            .mark_blocked(id, "upload result needs reconciliation")
                            .map_err(UploadError::Store)?;
                        Err(UploadError::InvalidResponse)
                    }
                    Err(error) => {
                        let reason = match error {
                            UploadError::Conflict | UploadError::SourceChanged => {
                                "upload needs review"
                            }
                            _ => "upload failed; retry is possible",
                        };
                        let update = if matches!(
                            error,
                            UploadError::Conflict | UploadError::SourceChanged
                        ) {
                            store.mark_blocked(id, reason)
                        } else {
                            store.mark_retry(id, reason)
                        };
                        update.map_err(UploadError::Store)?;
                        Err(error)
                    }
                }
            })
            .await;
            *state.lock().unwrap() = if matches!(result, Ok(Ok(()))) {
                EngineState::Connected
            } else {
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    fn refresh_catalog(&self) -> zbus::fdo::Result<()> {
        if self.upload_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("upload is running".to_owned()));
        }
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        if self
            .scan_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "scan is already running".to_owned(),
            ));
        }
        let catalog = Arc::clone(&self.catalog);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.scan_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Checking;
            let outcome = scan_catalog(catalog, client_id).await;
            *state.lock().unwrap() = scan_outcome_state(&outcome);
            if outcome.is_err() {
                eprintln!("catalog scan failed; request URLs and credentials were not logged");
            }
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn run_queued_download(&self) -> zbus::fdo::Result<i64> {
        if std::env::var("ONEDRIVE_EXPERIMENTAL_DOWNLOAD").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::Failed(
                "queued download execution is experimental and disabled".to_owned(),
            ));
        }
        if self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed("sync is paused".to_owned()));
        }
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.upload_in_progress.load(Ordering::SeqCst)
            || self
                .download_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "download is already running".to_owned(),
            ));
        }
        let prepare = async {
            let token = access_token(&client_id)
                .await
                .map_err(|_| zbus::fdo::Error::Failed("login is required".to_owned()))?;
            let catalog = self.catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let (operation, root) = {
                let mut sync = self.sync.lock().unwrap();
                sync.ensure_drive(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                let root = sync
                    .root()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("sync root is not configured".to_owned())
                    })?;
                let operation = sync
                    .claim_download()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read download queue".to_owned()))?
                    .ok_or_else(|| zbus::fdo::Error::Failed("no download is pending".to_owned()))?;
                (operation, root)
            };
            let item = operation
                .remote_id
                .as_deref()
                .and_then(|id| catalog.item(&drive_id, id).ok().flatten());
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?;
            let remote_path_matches = if let Some(root_id) = root_id {
                let items = catalog
                    .items(&drive_id)
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
                remote_files(&items, &root_id).0.iter().any(|file| {
                    Some(file.id.as_str()) == operation.remote_id.as_deref()
                        && file.path == operation.path
                })
            } else {
                false
            };
            let valid = item.as_ref().is_some_and(|item| {
                !item.deleted
                    && item.kind == "file"
                    && operation
                        .expected_etag
                        .as_deref()
                        .is_some_and(|etag| !etag.is_empty())
                    && item.etag.as_deref() == operation.expected_etag.as_deref()
                    && item.size.is_some_and(|size| size >= 0)
            }) && remote_path_matches;
            if !valid {
                self.sync
                    .lock()
                    .unwrap()
                    .mark_blocked(operation.id, "remote file changed")
                    .map_err(|_| {
                        zbus::fdo::Error::Failed("cannot block stale download".to_owned())
                    })?;
                return Err(zbus::fdo::Error::Failed("remote file changed".to_owned()));
            }
            let size = u64::try_from(item.unwrap().size.unwrap())
                .map_err(|_| zbus::fdo::Error::Failed("invalid catalog size".to_owned()))?;
            Ok::<_, zbus::fdo::Error>((token, drive_id, operation, root, size))
        }
        .await;
        let (token, drive_id, operation, root, size) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.download_in_progress.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let id = operation.id;
        let sync = Arc::clone(&self.sync);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.download_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let result = async {
                let graph = GraphClient::new().map_err(|_| QueuedDownloadError::Failed)?;
                let remote_id = operation
                    .remote_id
                    .as_deref()
                    .ok_or(QueuedDownloadError::Changed)?;
                let etag = operation
                    .expected_etag
                    .as_deref()
                    .ok_or(QueuedDownloadError::Changed)?;
                let target = root.join(&operation.path);
                let destination = QueuedDownloadTarget {
                    root: &root,
                    path: &target,
                    expected_local: operation.local_fingerprint.as_deref(),
                };
                let fingerprint = graph
                    .download_queued(&drive_id, remote_id, &token, destination, etag, size)
                    .await?;
                sync.lock()
                    .unwrap()
                    .finish_downloaded(&operation, &fingerprint)
                    .map_err(|_| QueuedDownloadError::Failed)
            }
            .await;
            if let Err(error) = &result {
                let update = match error {
                    QueuedDownloadError::Changed => sync
                        .lock()
                        .unwrap()
                        .mark_blocked(id, "download needs review"),
                    QueuedDownloadError::Failed => sync
                        .lock()
                        .unwrap()
                        .mark_retry(id, "download failed; retry is possible"),
                };
                if update.is_err() {
                    eprintln!("cannot update download queue status");
                }
            }
            *state.lock().unwrap() = if result.is_ok() {
                EngineState::Connected
            } else {
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    async fn run_queued_local_rename(&self) -> zbus::fdo::Result<i64> {
        if std::env::var("ONEDRIVE_EXPERIMENTAL_DOWNLOAD").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::Failed(
                "queued local rename execution is experimental and disabled".to_owned(),
            ));
        }
        if self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed("sync is paused".to_owned()));
        }
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.upload_in_progress.load(Ordering::SeqCst)
            || self
                .download_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "local change is already running".to_owned(),
            ));
        }
        let prepare = (|| {
            let catalog = self.catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let root_id = catalog
                .root_id(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("OneDrive root ID is missing".to_owned())
                })?;
            let files = remote_files(
                &catalog
                    .items(&drive_id)
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?,
                &root_id,
            )
            .0;
            let mut sync = self.sync.lock().unwrap();
            sync.ensure_drive(&drive_id)
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            let root = sync
                .root()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?
                .ok_or_else(|| {
                    zbus::fdo::Error::Failed("sync root is not configured".to_owned())
                })?;
            let operation = sync
                .claim_local_rename()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read rename queue".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("no local rename is pending".to_owned()))?;
            let valid_remote = files.iter().any(|file| {
                Some(file.id.as_str()) == operation.remote_id.as_deref()
                    && file.path == operation.path
                    && file.etag.as_deref() == operation.expected_etag.as_deref()
                    && file.etag.as_deref().is_some_and(|etag| !etag.is_empty())
            });
            let valid_mapping = sync
                .baselines()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read sync baseline".to_owned()))?
                .iter()
                .any(|base| {
                    Some(base.remote_id.as_str()) == operation.remote_id.as_deref()
                        && Some(base.path.as_str()) == operation.from_path.as_deref()
                        && base.etag == operation.expected_etag
                        && Some(base.fingerprint.as_str()) == operation.local_fingerprint.as_deref()
                });
            if !valid_remote || !valid_mapping {
                sync.mark_blocked(operation.id, "rename needs review")
                    .map_err(|_| {
                        zbus::fdo::Error::Failed("cannot block stale rename".to_owned())
                    })?;
                return Err(zbus::fdo::Error::Failed("rename needs review".to_owned()));
            }
            Ok::<_, zbus::fdo::Error>((operation, root))
        })();
        let (operation, root) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.download_in_progress.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let id = operation.id;
        let sync = Arc::clone(&self.sync);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.download_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let sync_worker = Arc::clone(&sync);
            let result = tokio::task::spawn_blocking(move || {
                let moved = apply_local_rename(&root, &operation);
                match moved {
                    Ok(()) => sync_worker
                        .lock()
                        .unwrap()
                        .finish_local_rename(&operation)
                        .map_err(|_| RenameError::Changed),
                    Err(error) => Err(error),
                }
            })
            .await;
            let success = matches!(result, Ok(Ok(())));
            if !success {
                let update = match result {
                    Ok(Err(RenameError::Changed)) => sync
                        .lock()
                        .unwrap()
                        .mark_blocked(id, "local rename needs review"),
                    _ => sync
                        .lock()
                        .unwrap()
                        .mark_retry(id, "local rename failed; retry is possible"),
                };
                if update.is_err() {
                    eprintln!("cannot update local rename queue status");
                }
            }
            *state.lock().unwrap() = if success {
                EngineState::Connected
            } else {
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    async fn run_queued_local_delete(&self) -> zbus::fdo::Result<i64> {
        if std::env::var("ONEDRIVE_EXPERIMENTAL_DOWNLOAD").as_deref() != Ok("1") {
            return Err(zbus::fdo::Error::Failed(
                "local deletion is experimental and disabled".to_owned(),
            ));
        }
        if self.sync_paused()? {
            return Err(zbus::fdo::Error::Failed("sync is paused".to_owned()));
        }
        if self.scan_in_progress.load(Ordering::SeqCst)
            || self.upload_in_progress.load(Ordering::SeqCst)
            || self
                .download_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "another local change is running".to_owned(),
            ));
        }
        let catalog = Arc::clone(&self.catalog);
        let sync = Arc::clone(&self.sync);
        let prepare = tokio::task::spawn_blocking(move || {
            let catalog = catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
            if !catalog
                .is_complete(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
            {
                return Err(zbus::fdo::Error::Failed(
                    "catalog scan is incomplete".to_owned(),
                ));
            }
            let items = catalog
                .items(&drive_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
            drop(catalog);
            let state_path = state_directory().map_err(|_| {
                zbus::fdo::Error::Failed("cannot access state directory".to_owned())
            })?;
            let (operation, root, baselines) = {
                let mut store = sync.lock().unwrap();
                store
                    .ensure_drive(&drive_id)
                    .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
                let root = store
                    .root()
                    .map_err(|_| zbus::fdo::Error::Failed("cannot read sync root".to_owned()))?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("sync root is not configured".to_owned())
                    })?;
                let operation = store
                    .claim_local_delete()
                    .map_err(|_| {
                        zbus::fdo::Error::Failed("cannot read local delete queue".to_owned())
                    })?
                    .ok_or_else(|| {
                        zbus::fdo::Error::Failed("no approved local deletion is pending".to_owned())
                    })?;
                let baselines = store.baselines().map_err(|_| {
                    zbus::fdo::Error::Failed("cannot read sync baseline".to_owned())
                })?;
                (operation, root, baselines)
            };
            if !local_delete_candidate(&items, &root, &baselines, &operation) {
                sync.lock()
                    .unwrap()
                    .mark_blocked(operation.id, "local deletion needs review")
                    .map_err(|_| {
                        zbus::fdo::Error::Failed("cannot block stale deletion".to_owned())
                    })?;
                return Err(zbus::fdo::Error::Failed(
                    "local deletion needs review".to_owned(),
                ));
            }
            Ok::<_, zbus::fdo::Error>((operation, root, state_path))
        })
        .await
        .map_err(|_| zbus::fdo::Error::Failed("local delete task failed".to_owned()))?;
        let (operation, root, state_path) = match prepare {
            Ok(value) => value,
            Err(error) => {
                self.download_in_progress.store(false, Ordering::SeqCst);
                return Err(error);
            }
        };
        let id = operation.id;
        let sync = Arc::clone(&self.sync);
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.download_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let sync_worker = Arc::clone(&sync);
            let result = tokio::task::spawn_blocking(move || {
                quarantine(&root, &state_path, &operation)?;
                sync_worker
                    .lock()
                    .unwrap()
                    .finish_local_deleted(&operation)
                    .map_err(|error| match error {
                        onedrive_sync::SyncError::InvalidCheckpoint => DeleteError::Changed,
                        _ => DeleteError::Io(std::io::Error::other(error.to_string())),
                    })
            })
            .await;
            let success = matches!(result, Ok(Ok(())));
            if !success {
                let update = match result {
                    Ok(Err(DeleteError::Changed)) => sync
                        .lock()
                        .unwrap()
                        .mark_blocked(id, "local deletion needs review"),
                    Ok(Err(DeleteError::CrossDevice)) => sync.lock().unwrap().mark_blocked(
                        id,
                        "sync root and recovery directory are on different filesystems",
                    ),
                    _ => sync
                        .lock()
                        .unwrap()
                        .mark_retry(id, "local deletion failed; retry is possible"),
                };
                if update.is_err() {
                    eprintln!("cannot update local deletion queue status");
                }
            }
            *state.lock().unwrap() = if success {
                EngineState::Connected
            } else {
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(id)
    }

    fn download_item(&self, item_id: &str, target_path: &str) -> zbus::fdo::Result<()> {
        if self.upload_in_progress.load(Ordering::SeqCst) {
            return Err(zbus::fdo::Error::Failed("upload is running".to_owned()));
        }
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        let target = PathBuf::from(target_path);
        if !target.is_absolute() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "destination must be an absolute path".to_owned(),
            ));
        }
        let (drive_id, size) = {
            let catalog = self.catalog.lock().unwrap();
            let drive_id = catalog
                .active_drive_id()
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("catalog is empty".to_owned()))?;
            let item = catalog
                .item(&drive_id, item_id)
                .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
                .ok_or_else(|| zbus::fdo::Error::Failed("file is not in catalog".to_owned()))?;
            if item.deleted || item.kind != "file" {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "selected item is not a file".to_owned(),
                ));
            }
            let size = item
                .size
                .map(u64::try_from)
                .transpose()
                .map_err(|_| zbus::fdo::Error::Failed("invalid catalog size".to_owned()))?;
            (drive_id, size)
        };
        if self
            .download_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "another download is running".to_owned(),
            ));
        }
        let item_id = item_id.to_owned();
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.download_in_progress);
        tokio::spawn(async move {
            *state.lock().unwrap() = EngineState::Transferring;
            let result = async {
                let access_token = access_token(&client_id).await?;
                let graph = GraphClient::new().map_err(|_| ())?;
                graph
                    .download_item(&drive_id, &item_id, &access_token, &target, size)
                    .await
                    .map_err(|_| ())
            }
            .await;
            *state.lock().unwrap() = if result.is_ok() {
                EngineState::Connected
            } else {
                eprintln!("download failed; file and preauthenticated URL were not logged");
                EngineState::Error
            };
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn begin_login(&self) -> zbus::fdo::Result<String> {
        let client_id = client_id().ok_or_else(|| {
            zbus::fdo::Error::Failed("ONEDRIVE_CLIENT_ID is not configured".to_owned())
        })?;
        if self
            .login_in_progress
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(zbus::fdo::Error::Failed(
                "login is already in progress".to_owned(),
            ));
        }
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(_) => {
                self.login_in_progress.store(false, Ordering::SeqCst);
                return Err(zbus::fdo::Error::Failed(
                    "cannot listen for browser callback".to_owned(),
                ));
            }
        };
        let port = listener.local_addr().unwrap().port();
        let (attempt, url) = match LoginAttempt::begin(&client_id, port) {
            Ok(values) => values,
            Err(_) => {
                self.login_in_progress.store(false, Ordering::SeqCst);
                return Err(zbus::fdo::Error::Failed(
                    "cannot prepare authorization URL".to_owned(),
                ));
            }
        };
        let state = Arc::clone(&self.state);
        let in_progress = Arc::clone(&self.login_in_progress);
        tokio::spawn(async move {
            if run_login(listener, attempt, state.clone()).await.is_err() {
                *state.lock().unwrap() = EngineState::Error;
                eprintln!("login failed; credentials and callback details were not logged");
            }
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(url.to_string())
    }
}

async fn queue_upload_candidate(
    catalog: Arc<Mutex<Catalog>>,
    sync: Arc<Mutex<SyncStore>>,
    action: Action,
    selector: String,
) -> zbus::fdo::Result<i64> {
    tokio::task::spawn_blocking(move || {
        let catalog = catalog.lock().unwrap();
        let drive_id = catalog
            .active_drive_id()
            .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?
            .ok_or_else(|| zbus::fdo::Error::Failed("OneDrive catalog is empty".to_owned()))?;
        if !catalog
            .is_complete(&drive_id)
            .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog cursor".to_owned()))?
        {
            return Err(zbus::fdo::Error::Failed(
                "catalog scan is incomplete".to_owned(),
            ));
        }
        let items = catalog
            .items(&drive_id)
            .map_err(|_| zbus::fdo::Error::Failed("cannot read catalog".to_owned()))?;
        let root_id = catalog
            .root_id(&drive_id)
            .map_err(|_| zbus::fdo::Error::Failed("cannot read drive root".to_owned()))?;
        drop(catalog);
        let mut sync = sync.lock().unwrap();
        sync.bind_drive(&drive_id)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let preview = sync
            .preview(&items, root_id.as_deref())
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        let operation = preview
            .operations
            .into_iter()
            .find(|operation| match action {
                Action::UploadChanged => {
                    operation.action == Action::UploadChanged
                        && operation.remote_id.as_deref() == Some(selector.as_str())
                        && operation.expected_etag.is_some()
                }
                Action::UploadNew => {
                    operation.action == Action::UploadNew
                        && operation.path == selector
                        && root_id
                            .as_deref()
                            .is_some_and(|root_id| remote_parent_exists(&items, root_id, &selector))
                }
                _ => false,
            })
            .ok_or_else(|| zbus::fdo::Error::Failed("no safe upload is available".to_owned()))?;
        sync.enqueue(&operation)
            .map_err(|_| zbus::fdo::Error::Failed("cannot queue upload".to_owned()))
    })
    .await
    .map_err(|_| zbus::fdo::Error::Failed("queue task failed".to_owned()))?
}

fn remote_folder_candidate(
    items: &[CatalogItem],
    root_id: &str,
    root: &std::path::Path,
    operation: &QueuedOperation,
) -> Option<(String, String)> {
    if operation.kind != "CreateRemoteFolder" {
        return None;
    }
    let parent_id = remote_parent_id(items, root_id, &operation.path)?;
    if operation.remote_id.as_deref() != Some(parent_id)
        || !local_directory_exists(root, &operation.path).ok()?
    {
        return None;
    }
    let name = operation.path.rsplit('/').next()?;
    let folded = name.to_lowercase();
    if items.iter().any(|item| {
        !item.deleted
            && item.parent_id.as_deref() == Some(parent_id)
            && item
                .name
                .as_deref()
                .is_some_and(|other| other.to_lowercase() == folded)
    }) {
        return None;
    }
    Some((parent_id.to_owned(), name.to_owned()))
}

struct RemoteRenameCandidate {
    item_id: String,
    source_parent_id: String,
    source_name: String,
    target_parent_id: String,
    target_name: String,
    etag: String,
    fingerprint: String,
}

fn remote_rename_candidate(
    items: &[CatalogItem],
    root_id: &str,
    root: &std::path::Path,
    baselines: &[Baseline],
    operation: &QueuedOperation,
) -> Option<RemoteRenameCandidate> {
    if operation.kind != "RenameRemote" {
        return None;
    }
    let source = operation.from_path.as_deref()?;
    if source == operation.path {
        return None;
    }
    let item_id = operation.remote_id.as_deref()?;
    let etag = operation.expected_etag.as_deref()?;
    let fingerprint = operation.local_fingerprint.as_deref()?;
    if item_id.is_empty()
        || etag.is_empty()
        || fingerprint.is_empty()
        || baselines.iter().any(|base| {
            base.remote_id != item_id && base.path.to_lowercase() == operation.path.to_lowercase()
        })
        || !baselines.iter().any(|base| {
            base.remote_id == item_id
                && base.path == source
                && base.etag.as_deref() == Some(etag)
                && base.fingerprint == fingerprint
        })
    {
        return None;
    }
    let source_parent_id = remote_parent_id(items, root_id, source)?;
    let target_parent_id = remote_parent_id(items, root_id, &operation.path)?;
    let source_name = source.rsplit('/').next()?;
    let target_name = operation.path.rsplit('/').next()?;
    if !remote_files(items, root_id)
        .0
        .iter()
        .any(|file| file.id == item_id && file.path == source && file.etag.as_deref() == Some(etag))
    {
        return None;
    }
    let target_folded = target_name.to_lowercase();
    if items.iter().any(|item| {
        !item.deleted
            && item.item_id != item_id
            && item.parent_id.as_deref() == Some(target_parent_id)
            && item
                .name
                .as_deref()
                .is_some_and(|name| name.to_lowercase() == target_folded)
    }) {
        return None;
    }
    let (local, skipped) = scan_local(root).ok()?;
    if onedrive_sync::folders::scan_local_directories(root)
        .ok()?
        .iter()
        .any(|path| path.to_lowercase() == operation.path.to_lowercase())
    {
        return None;
    }
    if skipped.iter().any(|path| {
        path == source
            || path == &operation.path
            || source.starts_with(&format!("{path}/"))
            || operation.path.starts_with(&format!("{path}/"))
    }) {
        return None;
    }
    if local.iter().any(|file| file.path == source)
        || local
            .iter()
            .filter(|file| file.fingerprint == fingerprint)
            .count()
            != 1
        || local
            .iter()
            .filter(|file| file.path.to_lowercase() == operation.path.to_lowercase())
            .count()
            != 1
        || !local
            .iter()
            .any(|file| file.path == operation.path && file.fingerprint == fingerprint)
        || !fingerprint_matches(root, &operation.path, fingerprint)
    {
        return None;
    }
    Some(RemoteRenameCandidate {
        item_id: item_id.to_owned(),
        source_parent_id: source_parent_id.to_owned(),
        source_name: source_name.to_owned(),
        target_parent_id: target_parent_id.to_owned(),
        target_name: target_name.to_owned(),
        etag: etag.to_owned(),
        fingerprint: fingerprint.to_owned(),
    })
}

struct RemoteDeleteCandidate {
    item_id: String,
    parent_id: String,
    name: String,
    etag: String,
}

fn remote_delete_candidate(
    items: &[CatalogItem],
    root_id: &str,
    root: &std::path::Path,
    baselines: &[Baseline],
    operation: &QueuedOperation,
) -> Option<RemoteDeleteCandidate> {
    if operation.kind != "DeleteRemote" || operation.from_path.is_some() {
        return None;
    }
    let item_id = operation.remote_id.as_deref()?;
    let etag = operation.expected_etag.as_deref()?;
    let fingerprint = operation.local_fingerprint.as_deref()?;
    if item_id.is_empty()
        || etag.is_empty()
        || fingerprint.is_empty()
        || !baselines.iter().any(|base| {
            base.remote_id == item_id
                && base.path == operation.path
                && base.etag.as_deref() == Some(etag)
                && base.fingerprint == fingerprint
        })
        || !remote_files(items, root_id).0.iter().any(|file| {
            file.id == item_id && file.path == operation.path && file.etag.as_deref() == Some(etag)
        })
        || !path_is_absent(root, &operation.path)
    {
        return None;
    }
    let parent_id = remote_parent_id(items, root_id, &operation.path)?;
    let name = operation.path.rsplit('/').next()?;
    Some(RemoteDeleteCandidate {
        item_id: item_id.to_owned(),
        parent_id: parent_id.to_owned(),
        name: name.to_owned(),
        etag: etag.to_owned(),
    })
}

fn local_delete_candidate(
    items: &[CatalogItem],
    root: &std::path::Path,
    baselines: &[Baseline],
    operation: &QueuedOperation,
) -> bool {
    if operation.kind != "DeleteLocal" || operation.from_path.is_some() {
        return false;
    }
    let (Some(item_id), Some(etag), Some(fingerprint)) = (
        operation.remote_id.as_deref(),
        operation.expected_etag.as_deref(),
        operation.local_fingerprint.as_deref(),
    ) else {
        return false;
    };
    !item_id.is_empty()
        && !etag.is_empty()
        && !fingerprint.is_empty()
        && !items
            .iter()
            .any(|item| item.item_id == item_id && !item.deleted)
        && baselines.iter().any(|base| {
            base.remote_id == item_id
                && base.path == operation.path
                && base.etag.as_deref() == Some(etag)
                && base.fingerprint == fingerprint
        })
        && (fingerprint_matches(root, &operation.path, fingerprint)
            || path_is_absent(root, &operation.path))
}

fn watch_local_root(sync: Arc<Mutex<SyncStore>>, epoch: Arc<AtomicU64>) {
    std::thread::spawn(move || {
        let mut watched_root: Option<PathBuf> = None;
        let mut monitor: Option<LocalMonitor> = None;
        loop {
            let root = sync.lock().unwrap().root().ok().flatten();
            if root != watched_root {
                watched_root = root.clone();
                monitor = None;
                if root.is_some() {
                    epoch.fetch_add(1, Ordering::Relaxed);
                }
            }
            if monitor.is_none()
                && let Some(path) = root.as_deref()
                && let Ok(created) = LocalMonitor::new(path)
            {
                monitor = Some(created);
                epoch.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(active) = monitor.as_mut() {
                match active.poll() {
                    Ok(true) => {
                        epoch.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {}
                    Err(_) => {
                        monitor = None;
                        epoch.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

fn client_id() -> Option<String> {
    match std::env::var("ONEDRIVE_CLIENT_ID") {
        Ok(value) => client_id_from_sources(Some(&value), None, None, None),
        Err(std::env::VarError::NotUnicode(_)) => None,
        Err(std::env::VarError::NotPresent) => {
            let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let config = client_config_path(config_home.as_deref(), home.as_deref())
                .and_then(|path| fs::read_to_string(path).ok());
            let dotenv = fs::read_to_string(".env").ok();
            client_id_from_sources(
                None,
                config.as_deref(),
                dotenv.as_deref(),
                option_env!("ONEDRIVE_CLIENT_ID"),
            )
        }
    }
}

fn client_config_path(
    config_home: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> Option<PathBuf> {
    config_home
        .filter(|path| path.is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .map(|path| path.join("onedrive-linux/config.env"))
}

fn client_id_from_sources(
    explicit: Option<&str>,
    config: Option<&str>,
    dotenv: Option<&str>,
    compiled: Option<&str>,
) -> Option<String> {
    if let Some(value) = explicit {
        return (!value.is_empty()).then(|| value.to_owned());
    }
    config
        .and_then(client_id_from_dotenv)
        .or_else(|| dotenv.and_then(client_id_from_dotenv))
        .or_else(|| {
            compiled
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
}

fn client_id_from_dotenv(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        if key.trim() != "ONEDRIVE_CLIENT_ID" {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

async fn run_login(
    listener: TcpListener,
    attempt: LoginAttempt,
    state: Arc<Mutex<EngineState>>,
) -> Result<(), ()> {
    let code = timeout(Duration::from_secs(300), wait_for_code(&listener, &attempt))
        .await
        .map_err(|_| ())??;
    let token = attempt.exchange_code(code).await.map_err(|_| ())?;
    verify_drive(&token).await?;
    let refresh = token.refresh_token().ok_or(())?.secret().to_owned();
    store_refresh(refresh).await?;
    *state.lock().unwrap() = EngineState::Connected;
    Ok(())
}

async fn wait_for_code(listener: &TcpListener, attempt: &LoginAttempt) -> Result<String, ()> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|_| ())?;
        let mut request = [0_u8; 4096];
        let mut size = 0;
        while size < request.len() && !request[..size].contains(&b'\n') {
            let read = timeout(Duration::from_secs(5), stream.read(&mut request[size..]))
                .await
                .map_err(|_| ())?
                .map_err(|_| ())?;
            if read == 0 {
                break;
            }
            size += read;
        }
        let first_line = std::str::from_utf8(&request[..size])
            .ok()
            .and_then(|text| text.lines().next())
            .unwrap_or("");
        let target = first_line
            .strip_prefix("GET ")
            .and_then(|line| line.split(' ').next());
        let result = target.and_then(|value| attempt.parse_callback(value).ok());
        let response = if result.is_some() {
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Length: 34\r\n\r\nLogin complete. Return to the app."
        } else {
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n"
        };
        let _ = stream.write_all(response.as_bytes()).await;
        if let Some(code) = result {
            return Ok(code);
        }
    }
}

async fn verify_drive(token: &BasicTokenResponse) -> Result<(), ()> {
    let http = oauth2::reqwest::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| ())?;
    let response = http
        .get("https://graph.microsoft.com/v1.0/me/drive")
        .bearer_auth(token.access_token().secret())
        .send()
        .await
        .map_err(|_| ())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(())
    }
}

async fn store_refresh(refresh: String) -> Result<(), ()> {
    tokio::task::spawn_blocking(move || {
        Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .and_then(|entry| entry.set_password(&refresh))
            .map_err(|_| ())
    })
    .await
    .map_err(|_| ())?
}

async fn restore_login(state: Arc<Mutex<EngineState>>, client_id: String) {
    let refresh = tokio::task::spawn_blocking(|| {
        Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .and_then(|entry| entry.get_password())
            .ok()
    })
    .await
    .ok()
    .flatten();
    let Some(refresh) = refresh else { return };
    let Ok(token) = auth::refresh_token(&client_id, &refresh).await else {
        return;
    };
    if verify_drive(&token).await.is_err() {
        return;
    }
    if let Some(rotated) = token.refresh_token()
        && store_refresh(rotated.secret().to_owned()).await.is_err()
    {
        return;
    }
    *state.lock().unwrap() = EngineState::Connected;
}

#[derive(Debug, PartialEq, Eq)]
enum ScanCatalogError {
    Offline,
    Failed,
}

impl From<FetchError> for ScanCatalogError {
    fn from(error: FetchError) -> Self {
        if error == FetchError::Offline {
            Self::Offline
        } else {
            Self::Failed
        }
    }
}

fn scan_outcome_state(result: &Result<(), ScanCatalogError>) -> EngineState {
    match result {
        Ok(()) => EngineState::Connected,
        Err(ScanCatalogError::Offline) => EngineState::Offline,
        Err(ScanCatalogError::Failed) => EngineState::Error,
    }
}

async fn scan_catalog(
    catalog: Arc<Mutex<Catalog>>,
    client_id: String,
) -> Result<(), ScanCatalogError> {
    let access_token = access_token_detailed(&client_id)
        .await
        .map_err(|error| match error {
            TokenAccessError::Offline => ScanCatalogError::Offline,
            TokenAccessError::Auth => ScanCatalogError::Failed,
        })?;
    let graph = GraphClient::new().map_err(|_| ScanCatalogError::Failed)?;
    let drive_id = graph.drive_id(&access_token).await?;
    let root_id = graph.root_id(&access_token).await?;
    catalog
        .lock()
        .unwrap()
        .set_active_drive_id(&drive_id)
        .map_err(|_| ScanCatalogError::Failed)?;
    catalog
        .lock()
        .unwrap()
        .set_root_id(&drive_id, &root_id)
        .map_err(|_| ScanCatalogError::Failed)?;
    let mut handled_gone = false;
    loop {
        let url = catalog
            .lock()
            .unwrap()
            .request_url(&drive_id)
            .map_err(|_| ScanCatalogError::Failed)?;
        let page = match graph.delta_page(&url, &access_token).await {
            Ok(page) => page,
            Err(FetchError::Gone) if !handled_gone => {
                catalog
                    .lock()
                    .unwrap()
                    .reset_expired_cursor(&drive_id)
                    .map_err(|_| ScanCatalogError::Failed)?;
                handled_gone = true;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let complete = page.delta_link.is_some();
        catalog
            .lock()
            .unwrap()
            .apply_page(&drive_id, &page)
            .map_err(|_| ScanCatalogError::Failed)?;
        if complete {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum TokenAccessError {
    Auth,
    Offline,
}

fn is_network_failure(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(error) = source {
        if let Some(transport) = error.downcast_ref::<reqwest::Error>()
            && (transport.is_connect() || transport.is_timeout() || transport.is_body())
        {
            return true;
        }
        if let Some(io) = error.downcast_ref::<io::Error>()
            && matches!(
                io.kind(),
                io::ErrorKind::NotConnected
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
            )
        {
            return true;
        }
        source = error.source();
    }
    false
}

async fn access_token_detailed(client_id: &str) -> Result<String, TokenAccessError> {
    let refresh = tokio::task::spawn_blocking(|| {
        Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .and_then(|entry| entry.get_password())
            .map_err(|_| TokenAccessError::Auth)
    })
    .await
    .map_err(|_| TokenAccessError::Auth)??;
    let token = auth::refresh_token(client_id, &refresh)
        .await
        .map_err(|error| {
            if is_network_failure(error.as_ref()) {
                TokenAccessError::Offline
            } else {
                TokenAccessError::Auth
            }
        })?;
    if let Some(rotated) = token.refresh_token() {
        store_refresh(rotated.secret().to_owned())
            .await
            .map_err(|_| TokenAccessError::Auth)?;
    }
    Ok(token.access_token().secret().to_owned())
}

async fn access_token(client_id: &str) -> Result<String, ()> {
    access_token_detailed(client_id).await.map_err(|_| ())
}

async fn poll_catalog(
    state: Arc<Mutex<EngineState>>,
    catalog: Arc<Mutex<Catalog>>,
    scan_in_progress: Arc<AtomicBool>,
    download_in_progress: Arc<AtomicBool>,
    upload_in_progress: Arc<AtomicBool>,
    client_id: String,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    loop {
        interval.tick().await;
        let current = *state.lock().unwrap();
        if matches!(
            current,
            EngineState::NeedsLogin | EngineState::Transferring | EngineState::Checking
        ) || download_in_progress.load(Ordering::SeqCst)
            || upload_in_progress.load(Ordering::SeqCst)
            || scan_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            continue;
        }
        *state.lock().unwrap() = EngineState::Checking;
        let result = scan_catalog(Arc::clone(&catalog), client_id.clone()).await;
        *state.lock().unwrap() = scan_outcome_state(&result);
        scan_in_progress.store(false, Ordering::SeqCst);
    }
}

/// Drafts are the durable queue; one candidate is retried per interval.
async fn poll_virtual_drafts(daemon: Daemon) {
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut last_attempted = None;
    loop {
        interval.tick().await;
        let Some((drive_id, item_id)) =
            daemon.virtual_auto_upload_candidate(last_attempted.as_deref())
        else {
            continue;
        };
        last_attempted = Some(item_id.clone());
        if let Err(error) = daemon.run_virtual_draft_upload(item_id.clone()).await {
            let issue = virtual_upload_issue(&error);
            let result = tokio::task::spawn_blocking(move || {
                record_virtual_upload_failure(&drive_id, &item_id, issue)
            })
            .await;
            if !matches!(result, Ok(Ok(()))) {
                eprintln!("cannot record automatic virtual draft upload failure");
            }
            eprintln!("automatic virtual draft upload failed; local draft retained");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationAttempt {
    Applied,
    Conflict,
    Retry,
}

fn virtual_mutation_matches_catalog(catalog: &Catalog, mutation: &Mutation) -> bool {
    if catalog.active_drive_id().ok().flatten().as_deref() != Some(&mutation.drive_id)
        || !catalog.is_complete(&mutation.drive_id).unwrap_or(false)
    {
        return false;
    }
    let Ok(Some(item)) = catalog.item(&mutation.drive_id, &mutation.item_id) else {
        return false;
    };
    !item.deleted
        && item.parent_id.as_deref() == Some(&mutation.parent_id)
        && item.name.as_deref() == Some(&mutation.name)
        && item.etag.as_deref() == Some(&mutation.etag)
        && (item.kind == "folder") == mutation.directory
}

async fn run_virtual_mutation(
    daemon: &Daemon,
    mutation: &Mutation,
    client_id: &str,
) -> MutationAttempt {
    let token = match access_token_detailed(client_id).await {
        Ok(token) => token,
        Err(_) => return MutationAttempt::Retry,
    };
    let graph = match GraphClient::new() {
        Ok(graph) => graph,
        Err(_) => return MutationAttempt::Retry,
    };
    let live_drive = match graph.drive_id(&token).await {
        Ok(drive) => drive,
        Err(_) => return MutationAttempt::Retry,
    };
    if live_drive != mutation.drive_id {
        return MutationAttempt::Conflict;
    }
    {
        let catalog = daemon.catalog.lock().unwrap();
        if !virtual_mutation_matches_catalog(&catalog, mutation) {
            return MutationAttempt::Conflict;
        }
        if let MutationKind::Rename {
            target_parent_id,
            target_name,
        } = &mutation.kind
        {
            let parent_exists = catalog
                .root_id(&mutation.drive_id)
                .ok()
                .flatten()
                .as_deref()
                == Some(target_parent_id)
                || catalog
                    .item(&mutation.drive_id, target_parent_id)
                    .ok()
                    .flatten()
                    .is_some_and(|item| !item.deleted && item.kind == "folder");
            let occupied = catalog.items(&mutation.drive_id).ok().is_some_and(|items| {
                items.iter().any(|item| {
                    !item.deleted
                        && item.item_id != mutation.item_id
                        && item.parent_id.as_deref() == Some(target_parent_id)
                        && item.name.as_deref() == Some(target_name)
                })
            });
            if !parent_exists || occupied {
                return MutationAttempt::Conflict;
            }
        }
    }
    match &mutation.kind {
        MutationKind::Delete => {
            if mutation.directory {
                match graph
                    .remote_folder_is_empty(&mutation.drive_id, &mutation.item_id, &token)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return MutationAttempt::Conflict,
                    Err(_) => return MutationAttempt::Retry,
                }
            }
            match graph
                .delete_remote_item(
                    &mutation.drive_id,
                    &RemoteDeleteInput {
                        item_id: &mutation.item_id,
                        parent_id: &mutation.parent_id,
                        name: &mutation.name,
                        etag: &mutation.etag,
                    },
                    mutation.directory,
                    &token,
                )
                .await
            {
                Ok(()) => MutationAttempt::Applied,
                Err(RemoteDeleteError::Conflict) => MutationAttempt::Conflict,
                Err(RemoteDeleteError::Retry | RemoteDeleteError::NeedsReconcile) => {
                    MutationAttempt::Retry
                }
            }
        }
        MutationKind::Rename {
            target_parent_id,
            target_name,
        } => {
            let input = RemoteRenameInput {
                item_id: &mutation.item_id,
                source_parent_id: &mutation.parent_id,
                source_name: &mutation.name,
                target_parent_id,
                target_name,
                etag: &mutation.etag,
            };
            match graph
                .rename_remote_item(&mutation.drive_id, &input, mutation.directory, &token)
                .await
            {
                Ok(_) => MutationAttempt::Applied,
                Err(RemoteRenameError::Conflict) => MutationAttempt::Conflict,
                Err(RemoteRenameError::Retry | RemoteRenameError::NeedsReconcile) => {
                    MutationAttempt::Retry
                }
            }
        }
    }
}

async fn poll_virtual_mutations(daemon: Daemon, state_root: PathBuf, client_id: String) {
    let mut interval = tokio::time::interval(Duration::from_secs(15));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if !matches!(
            *daemon.state.lock().unwrap(),
            EngineState::Connected | EngineState::Synced
        ) || daemon.sync_paused().unwrap_or(true)
            || daemon.scan_in_progress.load(Ordering::SeqCst)
            || daemon.download_in_progress.load(Ordering::SeqCst)
            || daemon
                .upload_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            continue;
        }
        let _busy = BusyFlagGuard(Arc::clone(&daemon.upload_in_progress));
        let root = state_root.join("virtual-drafts");
        if !root.exists() {
            continue;
        }
        let store = match MutationStore::open(&root) {
            Ok(store) => store,
            Err(error) => {
                eprintln!("cannot open virtual mutation queue: {error}");
                continue;
            }
        };
        let mutations = match store.list() {
            Ok(mutations) => mutations,
            Err(error) => {
                eprintln!("cannot read virtual mutation queue: {error}");
                continue;
            }
        };
        // A large deletion batch needs explicit review before any remote removal.
        if !store.deletions_approved(&mutations).unwrap_or(false) {
            continue;
        }
        let Some(mutation) = mutations
            .into_iter()
            .find(|entry| entry.status == MutationStatus::Pending)
        else {
            continue;
        };
        let result = run_virtual_mutation(&daemon, &mutation, &client_id).await;
        let status = match result {
            MutationAttempt::Applied => MutationStatus::Applied,
            MutationAttempt::Conflict => MutationStatus::Conflict,
            MutationAttempt::Retry => continue,
        };
        if let Err(error) = store.update_status(&mutation, status) {
            eprintln!("cannot update virtual mutation: {error}");
        }
    }
}

async fn poll_virtual_directories(daemon: Daemon, state_root: PathBuf, client_id: String) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut reported = std::collections::HashSet::new();
    loop {
        interval.tick().await;
        if !matches!(
            *daemon.state.lock().unwrap(),
            EngineState::Connected | EngineState::Synced
        ) || daemon.upload_in_progress.load(Ordering::SeqCst)
            || daemon.scan_in_progress.load(Ordering::SeqCst)
        {
            continue;
        }
        let root = state_root.join("virtual-drafts");
        if !root.exists() {
            continue;
        }
        let directories = match DirectoryStore::open(&root).and_then(|store| store.list()) {
            Ok(directories) => directories,
            Err(error) => {
                eprintln!("cannot read local virtual directories: {error}");
                continue;
            }
        };
        let mapped: std::collections::HashSet<_> = directories
            .iter()
            .filter(|entry| entry.remote_id.is_some())
            .map(|entry| (entry.drive_id.clone(), entry.item_id.clone()))
            .collect();
        let Some(directory) = directories.into_iter().find(|directory| {
            directory.remote_id.is_none()
                && (!directory.parent_id.starts_with("local-dir:")
                    || mapped.contains(&(directory.drive_id.clone(), directory.parent_id.clone())))
        }) else {
            continue;
        };
        match daemon
            .run_virtual_directory_upload(directory.item_id.clone())
            .await
        {
            Ok(_) => {
                reported.remove(&directory.item_id);
                if daemon
                    .scan_in_progress
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    *daemon.state.lock().unwrap() = EngineState::Checking;
                    let outcome =
                        scan_catalog(Arc::clone(&daemon.catalog), client_id.clone()).await;
                    *daemon.state.lock().unwrap() = scan_outcome_state(&outcome);
                    daemon.scan_in_progress.store(false, Ordering::SeqCst);
                }
            }
            Err(error) => {
                if reported.insert(directory.item_id.clone()) {
                    eprintln!(
                        "cannot upload virtual directory {}: {error}",
                        directory.item_id
                    );
                }
            }
        }
    }
}

fn catalog_ready_for_drive(catalog: &Catalog, drive_id: &str) -> bool {
    catalog.active_drive_id().ok().flatten().as_deref() == Some(drive_id)
        && catalog.is_complete(drive_id).unwrap_or(false)
        && catalog.root_id(drive_id).ok().flatten().is_some()
}

/// A saved mount path is attempted once the account and its catalog are ready.
/// A failed FUSE mount is left for the user to inspect rather than retried over a busy path.
async fn auto_mount_configured(daemon: Daemon, state_root: PathBuf, client_id: String) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let path = match mount_record::configured(&state_root) {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                eprintln!("cannot read configured virtual mount path: {error}");
                return;
            }
        };
        if daemon.virtual_mount.lock().unwrap().is_some() {
            return;
        }
        if !matches!(
            *daemon.state.lock().unwrap(),
            EngineState::Connected | EngineState::Synced
        ) {
            continue;
        }
        if daemon.mount_in_progress.load(Ordering::SeqCst)
            || daemon.upload_in_progress.load(Ordering::SeqCst)
            || daemon.scan_in_progress.load(Ordering::SeqCst)
        {
            continue;
        }
        let token = match access_token_detailed(&client_id).await {
            Ok(token) => token,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let graph = match GraphClient::new() {
            Ok(graph) => graph,
            Err(_) => return,
        };
        let drive_id = match graph.drive_id(&token).await {
            Ok(drive_id) => drive_id,
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        if !catalog_ready_for_drive(&daemon.catalog.lock().unwrap(), &drive_id) {
            if daemon
                .scan_in_progress
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            let _busy = BusyFlagGuard(Arc::clone(&daemon.scan_in_progress));
            let result = scan_catalog(Arc::clone(&daemon.catalog), client_id.clone()).await;
            drop(_busy);
            if let Err(error) = result {
                eprintln!("cannot prepare catalog for automatic virtual mount: {error:?}");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            continue;
        }
        let Some(path) = path.to_str() else {
            eprintln!("configured virtual mount path is not UTF-8");
            return;
        };
        if let Err(error) = daemon.mount_virtual_drive(path).await {
            eprintln!("cannot automatically mount virtual drive at {path}: {error}");
        }
        return;
    }
}

fn virtual_upload_issue(error: &zbus::fdo::Error) -> DraftUploadIssue {
    let message = error.to_string();
    if message.contains("login is required") {
        DraftUploadIssue::LoginRequired
    } else if message.contains("remote file changed") {
        DraftUploadIssue::RemoteChanged
    } else if message.contains("upload failed") {
        DraftUploadIssue::TransferFailed
    } else {
        DraftUploadIssue::Other
    }
}

fn record_virtual_upload_failure(
    drive_id: &str,
    item_id: &str,
    issue: DraftUploadIssue,
) -> io::Result<()> {
    let root = state_directory()
        .map_err(|_| io::Error::other("cannot access state directory"))?
        .join("virtual-drafts");
    let store = DraftStore::open(&root)?;
    let Some(draft) = store.recover_item(drive_id, item_id)? else {
        return Ok(());
    };
    store.save_upload_failure(&DraftUploadFailure {
        identity: draft.identity().clone(),
        issue,
        attempted_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    })
}

fn state_directory() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or("HOME is not set")?;
    let directory = state_home.join("onedrive-linux");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    Ok(directory)
}

fn state_database_path(
    directory: &std::path::Path,
    name: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = directory.join(name);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(Mutex::new(EngineState::NeedsLogin));
    let state_path = state_directory()?;
    match mount_record::pending(&state_path) {
        Ok(Some(path)) => eprintln!(
            "previous virtual mount is still occupied: {}",
            path.display()
        ),
        Err(error) => eprintln!("cannot inspect previous virtual mount: {error}"),
        Ok(None) => {}
    }
    let catalog = Arc::new(Mutex::new(Catalog::open(&state_database_path(
        &state_path,
        "catalog.sqlite3",
    )?)?));
    let sync = Arc::new(Mutex::new(SyncStore::open(&state_database_path(
        &state_path,
        "sync.sqlite3",
    )?)?));
    let scan_in_progress = Arc::new(AtomicBool::new(false));
    let download_in_progress = Arc::new(AtomicBool::new(false));
    let upload_in_progress = Arc::new(AtomicBool::new(false));
    let local_change_epoch = Arc::new(AtomicU64::new(0));
    let virtual_mount = Arc::new(Mutex::new(None::<MountedVirtualDrive>));
    watch_local_root(Arc::clone(&sync), Arc::clone(&local_change_epoch));
    let daemon = Daemon {
        state: Arc::clone(&state),
        login_in_progress: Arc::new(AtomicBool::new(false)),
        catalog: Arc::clone(&catalog),
        sync,
        scan_in_progress: Arc::clone(&scan_in_progress),
        download_in_progress: Arc::clone(&download_in_progress),
        upload_in_progress: Arc::clone(&upload_in_progress),
        local_change_epoch,
        virtual_mount: Arc::clone(&virtual_mount),
        mount_in_progress: Arc::new(AtomicBool::new(false)),
    };
    let automatic_upload = daemon.clone();
    let automatic_mount = daemon.clone();
    let automatic_mutations = daemon.clone();
    let automatic_directories = daemon.clone();
    let _connection = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, daemon)?
        .build()
        .await?;
    if let Some(client_id) = client_id() {
        if std::env::var("ONEDRIVE_SMOKE_NO_RESTORE").as_deref() != Ok("1") {
            tokio::spawn(poll_virtual_mutations(
                automatic_mutations,
                state_path.clone(),
                client_id.clone(),
            ));
            tokio::spawn(poll_virtual_directories(
                automatic_directories,
                state_path.clone(),
                client_id.clone(),
            ));
        }
        let configured_mount = match mount_record::configured(&state_path) {
            Ok(path) => path.is_some(),
            Err(error) => {
                eprintln!("cannot read configured virtual mount path: {error}");
                false
            }
        };
        if configured_mount && std::env::var("ONEDRIVE_SMOKE_NO_RESTORE").as_deref() != Ok("1") {
            tokio::spawn(auto_mount_configured(
                automatic_mount,
                state_path.clone(),
                client_id.clone(),
            ));
        }
        if automatic_upload.virtual_draft_auto_upload_enabled() {
            tokio::spawn(poll_virtual_drafts(automatic_upload));
        }
        if std::env::var("ONEDRIVE_SMOKE_NO_RESTORE").as_deref() != Ok("1") {
            tokio::spawn(restore_login(Arc::clone(&state), client_id.clone()));
        }
        tokio::spawn(poll_catalog(
            state,
            catalog,
            scan_in_progress,
            download_in_progress,
            upload_in_progress,
            client_id,
        ));
    }
    eprintln!("onedrive-daemon listening on {BUS_NAME}");
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    let mounted = virtual_mount.lock().unwrap().take();
    if let Some(mounted) = mounted {
        tokio::task::spawn_blocking(move || mounted.session.umount_and_join()).await??;
        mount_record::clear(&state_path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Baseline, CatalogItem, QueuedOperation, ScanCatalogError, VIRTUAL_CACHE_BYTES,
        VirtualDownloadContext, catalog_ready_for_drive, client_config_path, client_id_from_dotenv,
        client_id_from_sources, inspect_virtual_cache, is_network_failure, local_delete_candidate,
        prepare_virtual_request, publish_virtual_file, remote_delete_candidate,
        remote_folder_candidate, remote_rename_candidate, scan_outcome_state, virtual_upload_issue,
    };
    use onedrive_cache::{CacheError, CacheStore};
    use onedrive_catalog::{Catalog, DeltaPage};
    use onedrive_core::EngineState;
    use onedrive_mount::{DownloadRequest, draft::DraftUploadIssue};
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::sync::{Arc, Mutex, atomic::AtomicBool};

    #[test]
    fn automatic_mount_waits_for_matching_complete_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        catalog.set_active_drive_id("drive").unwrap();
        assert!(!catalog_ready_for_drive(&catalog, "drive"));
        catalog.set_root_id("drive", "root").unwrap();
        catalog
            .apply_page(
                "drive",
                &DeltaPage {
                    value: Vec::new(),
                    next_link: None,
                    delta_link: Some(
                        "https://graph.microsoft.com/v1.0/me/drive/root/delta?done=1".into(),
                    ),
                },
            )
            .unwrap();
        assert!(catalog_ready_for_drive(&catalog, "drive"));
        assert!(!catalog_ready_for_drive(&catalog, "other-drive"));
    }

    #[test]
    fn upload_failure_record_uses_only_known_codes() {
        assert_eq!(
            virtual_upload_issue(&zbus::fdo::Error::Failed("login is required".into())),
            DraftUploadIssue::LoginRequired
        );
        assert_eq!(
            virtual_upload_issue(&zbus::fdo::Error::Failed("remote file changed".into())),
            DraftUploadIssue::RemoteChanged
        );
        assert_eq!(
            virtual_upload_issue(&zbus::fdo::Error::Failed(
                "upload failed; secret URL".into()
            )),
            DraftUploadIssue::TransferFailed
        );
        assert_eq!(
            virtual_upload_issue(&zbus::fdo::Error::Failed("unexpected secret URL".into())),
            DraftUploadIssue::Other
        );
    }

    #[test]
    fn queued_folder_is_rechecked_before_graph_write() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("Notes")).unwrap();
        let operation = QueuedOperation {
            id: 1,
            kind: "CreateRemoteFolder".into(),
            path: "Notes".into(),
            from_path: None,
            remote_id: Some("root".into()),
            expected_etag: None,
            local_fingerprint: None,
            attempts: 1,
        };
        assert_eq!(
            remote_folder_candidate(&[], "root", temp.path(), &operation),
            Some(("root".into(), "Notes".into()))
        );
        let mut wrong_parent = operation.clone();
        wrong_parent.remote_id = Some("other".into());
        assert!(remote_folder_candidate(&[], "root", temp.path(), &wrong_parent).is_none());
        let occupied = CatalogItem {
            drive_id: "drive".into(),
            item_id: "other".into(),
            parent_id: Some("root".into()),
            name: Some("notes".into()),
            kind: "file".into(),
            size: Some(1),
            etag: None,
            ctag: None,
            modified: None,
            deleted: false,
        };
        assert!(remote_folder_candidate(&[occupied], "root", temp.path(), &operation).is_none());
        std::fs::remove_dir(temp.path().join("Notes")).unwrap();
        assert!(remote_folder_candidate(&[], "root", temp.path(), &operation).is_none());
    }

    #[test]
    fn remote_rename_candidate_rechecks_both_sides() {
        use sha2::{Digest, Sha256};
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("moved.txt"), b"content").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"content"));
        let operation = QueuedOperation {
            id: 1,
            kind: "RenameRemote".into(),
            path: "moved.txt".into(),
            from_path: Some("old.txt".into()),
            remote_id: Some("id".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some(hash.clone()),
            attempts: 1,
        };
        let base = Baseline {
            remote_id: "id".into(),
            path: "old.txt".into(),
            etag: Some("v1".into()),
            fingerprint: hash.clone(),
        };
        let item = CatalogItem {
            drive_id: "drive".into(),
            item_id: "id".into(),
            parent_id: Some("root".into()),
            name: Some("old.txt".into()),
            kind: "file".into(),
            size: Some(7),
            etag: Some("v1".into()),
            ctag: None,
            modified: None,
            deleted: false,
        };
        let candidate = remote_rename_candidate(
            std::slice::from_ref(&item),
            "root",
            temp.path(),
            std::slice::from_ref(&base),
            &operation,
        )
        .unwrap();
        assert_eq!(candidate.item_id, "id");
        assert_eq!(candidate.target_parent_id, "root");
        assert_eq!(candidate.target_name, "moved.txt");
        let mut changed = item.clone();
        changed.etag = Some("v2".into());
        assert!(
            remote_rename_candidate(
                &[changed],
                "root",
                temp.path(),
                std::slice::from_ref(&base),
                &operation
            )
            .is_none()
        );
        let mut occupied = item.clone();
        occupied.item_id = "other".into();
        occupied.name = Some("MOVED.TXT".into());
        assert!(
            remote_rename_candidate(
                &[item.clone(), occupied],
                "root",
                temp.path(),
                std::slice::from_ref(&base),
                &operation
            )
            .is_none()
        );
        std::fs::write(temp.path().join("old.txt"), b"other").unwrap();
        assert!(
            remote_rename_candidate(
                std::slice::from_ref(&item),
                "root",
                temp.path(),
                std::slice::from_ref(&base),
                &operation
            )
            .is_none()
        );
        std::fs::remove_file(temp.path().join("old.txt")).unwrap();
        std::fs::write(temp.path().join("moved.txt"), b"changed").unwrap();
        assert!(
            remote_rename_candidate(&[item], "root", temp.path(), &[base], &operation).is_none()
        );
    }

    #[test]
    fn remote_delete_candidate_requires_exact_mapping_remote_and_absence() {
        let temp = tempfile::tempdir().unwrap();
        let operation = QueuedOperation {
            id: 1,
            kind: "DeleteRemote".into(),
            path: "gone.txt".into(),
            from_path: None,
            remote_id: Some("id".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some("hash".into()),
            attempts: 1,
        };
        let base = Baseline {
            remote_id: "id".into(),
            path: "gone.txt".into(),
            etag: Some("v1".into()),
            fingerprint: "hash".into(),
        };
        let item = CatalogItem {
            drive_id: "drive".into(),
            item_id: "id".into(),
            parent_id: Some("root".into()),
            name: Some("gone.txt".into()),
            kind: "file".into(),
            size: Some(4),
            etag: Some("v1".into()),
            ctag: None,
            modified: None,
            deleted: false,
        };
        let candidate = remote_delete_candidate(
            std::slice::from_ref(&item),
            "root",
            temp.path(),
            std::slice::from_ref(&base),
            &operation,
        )
        .unwrap();
        assert_eq!(candidate.item_id, "id");
        assert_eq!(candidate.parent_id, "root");
        let mut changed = item.clone();
        changed.etag = Some("v2".into());
        assert!(
            remote_delete_candidate(
                &[changed],
                "root",
                temp.path(),
                std::slice::from_ref(&base),
                &operation,
            )
            .is_none()
        );
        std::fs::write(temp.path().join("gone.txt"), b"new").unwrap();
        assert!(
            remote_delete_candidate(
                std::slice::from_ref(&item),
                "root",
                temp.path(),
                std::slice::from_ref(&base),
                &operation,
            )
            .is_none()
        );
        std::fs::remove_file(temp.path().join("gone.txt")).unwrap();
        let mut nested = operation.clone();
        nested.path = "missing/file.txt".into();
        assert!(
            remote_delete_candidate(
                std::slice::from_ref(&item),
                "root",
                temp.path(),
                std::slice::from_ref(&base),
                &nested,
            )
            .is_none()
        );
        let mut wrong_baseline = base.clone();
        wrong_baseline.fingerprint = "other".into();
        assert!(
            remote_delete_candidate(
                std::slice::from_ref(&item),
                "root",
                temp.path(),
                &[wrong_baseline],
                &operation,
            )
            .is_none()
        );
    }

    #[test]
    fn local_delete_candidate_requires_tombstone_and_unchanged_file() {
        use sha2::{Digest, Sha256};
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("gone.txt"), b"content").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"content"));
        let operation = QueuedOperation {
            id: 1,
            kind: "DeleteLocal".into(),
            path: "gone.txt".into(),
            from_path: None,
            remote_id: Some("id".into()),
            expected_etag: Some("v1".into()),
            local_fingerprint: Some(hash.clone()),
            attempts: 1,
        };
        let base = Baseline {
            remote_id: "id".into(),
            path: "gone.txt".into(),
            etag: Some("v1".into()),
            fingerprint: hash,
        };
        let live = CatalogItem {
            drive_id: "drive".into(),
            item_id: "id".into(),
            parent_id: Some("root".into()),
            name: Some("gone.txt".into()),
            kind: "file".into(),
            size: Some(7),
            etag: Some("v1".into()),
            ctag: None,
            modified: None,
            deleted: false,
        };
        assert!(!local_delete_candidate(
            std::slice::from_ref(&live),
            temp.path(),
            std::slice::from_ref(&base),
            &operation
        ));
        let mut tombstone = live;
        tombstone.deleted = true;
        assert!(local_delete_candidate(
            std::slice::from_ref(&tombstone),
            temp.path(),
            std::slice::from_ref(&base),
            &operation
        ));
        std::fs::write(temp.path().join("gone.txt"), b"changed").unwrap();
        assert!(!local_delete_candidate(
            std::slice::from_ref(&tombstone),
            temp.path(),
            std::slice::from_ref(&base),
            &operation
        ));
        std::fs::remove_file(temp.path().join("gone.txt")).unwrap();
        assert!(local_delete_candidate(
            std::slice::from_ref(&tombstone),
            temp.path(),
            std::slice::from_ref(&base),
            &operation
        ));
        let mut stale = base;
        stale.etag = Some("v2".into());
        assert!(!local_delete_candidate(
            &[tombstone],
            temp.path(),
            &[stale],
            &operation
        ));
    }

    #[test]
    fn reads_client_id_without_matching_other_keys_or_empty_values() {
        let contents = "# registration\nOTHER_CLIENT_ID=wrong\nONEDRIVE_CLIENT_ID=\nONEDRIVE_CLIENT_ID=\"registered-id\"\n";
        assert_eq!(
            client_id_from_dotenv(contents).as_deref(),
            Some("registered-id")
        );
    }

    #[test]
    fn empty_environment_value_disables_dotenv_fallback() {
        assert_eq!(
            client_id_from_sources(
                Some(""),
                Some("ONEDRIVE_CLIENT_ID=config"),
                Some("ONEDRIVE_CLIENT_ID=registered"),
                Some("built")
            ),
            None
        );
        assert_eq!(
            client_id_from_sources(
                None,
                None,
                Some("ONEDRIVE_CLIENT_ID=registered"),
                Some("built")
            )
            .as_deref(),
            Some("registered")
        );
    }

    #[test]
    fn xdg_config_precedes_project_dotenv_and_has_absolute_fallback() {
        assert_eq!(
            client_id_from_sources(
                None,
                Some("ONEDRIVE_CLIENT_ID=configured"),
                Some("ONEDRIVE_CLIENT_ID=project"),
                Some("built"),
            )
            .as_deref(),
            Some("configured")
        );
        assert_eq!(
            client_config_path(
                Some(Path::new("/tmp/config")),
                Some(Path::new("/home/user"))
            )
            .unwrap(),
            Path::new("/tmp/config/onedrive-linux/config.env")
        );
        assert_eq!(
            client_config_path(Some(Path::new("relative")), Some(Path::new("/home/user"))).unwrap(),
            Path::new("/home/user/.config/onedrive-linux/config.env")
        );
        assert!(client_config_path(Some(Path::new("relative")), None).is_none());
    }

    #[test]
    fn network_failure_detection_does_not_confuse_authentication() {
        assert!(is_network_failure(&io::Error::from(
            io::ErrorKind::NotConnected
        )));
        assert!(!is_network_failure(&io::Error::other("access denied")));
        assert_eq!(
            scan_outcome_state(&Err(ScanCatalogError::Offline)),
            EngineState::Offline
        );
        assert_eq!(
            scan_outcome_state(&Err(ScanCatalogError::Failed)),
            EngineState::Error
        );
    }

    #[test]
    fn virtual_cache_publishes_only_complete_versioned_file() {
        let directory = tempfile::tempdir().unwrap();
        let cache_root = directory.path().join("cache");
        let source = directory.path().join("source");
        let request = DownloadRequest {
            drive_id: "drive".into(),
            item_id: "remote-id".into(),
            etag: "v1".into(),
            size: 5,
        };
        fs::write(&source, b"short").unwrap();
        publish_virtual_file(&cache_root, &request, &source).unwrap();
        let mut cache = CacheStore::open(&cache_root, VIRTUAL_CACHE_BYTES).unwrap();
        assert!(cache.get(&request.cache_key()).unwrap().is_some());
        drop(cache);

        let changed = DownloadRequest {
            etag: "v2".into(),
            size: 6,
            ..request.clone()
        };
        assert!(matches!(
            publish_virtual_file(&cache_root, &changed, &source),
            Err(CacheError::SizeMismatch)
        ));
        let mut cache = CacheStore::open(&cache_root, VIRTUAL_CACHE_BYTES).unwrap();
        assert!(cache.get(&request.cache_key()).unwrap().is_some());
        assert!(cache.get(&changed.cache_key()).unwrap().is_none());
    }

    #[test]
    fn virtual_download_uses_persisted_cache_limit() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cache");
        let mut cache = CacheStore::open(&root, VIRTUAL_CACHE_BYTES).unwrap();
        cache.set_limit(VIRTUAL_CACHE_BYTES * 2).unwrap();
        drop(cache);
        let request = DownloadRequest {
            drive_id: "drive".into(),
            item_id: "large-file".into(),
            etag: "v1".into(),
            size: VIRTUAL_CACHE_BYTES + 1,
        };
        assert_eq!(
            inspect_virtual_cache(&root, &request).unwrap(),
            (false, VIRTUAL_CACHE_BYTES * 2)
        );
        let mut cache = CacheStore::open(&root, VIRTUAL_CACHE_BYTES).unwrap();
        cache.set_limit(VIRTUAL_CACHE_BYTES / 4).unwrap();
        drop(cache);
        assert_eq!(
            inspect_virtual_cache(&root, &request).unwrap(),
            (false, VIRTUAL_CACHE_BYTES / 4)
        );
    }

    #[tokio::test]
    async fn virtual_loader_rejects_stale_snapshot_before_network() {
        let mut catalog = Catalog::in_memory().unwrap();
        catalog.set_active_drive_id("drive").unwrap();
        catalog.set_root_id("drive", "root").unwrap();
        let page: DeltaPage = serde_json::from_str(
            r#"{"value":[{"id":"file","parentReference":{"id":"root"},"name":"name","file":{},"size":5,"eTag":"v2"}],"@odata.deltaLink":"https://graph.microsoft.com/v1.0/me/drive/root/delta?token=done"}"#,
        ).unwrap();
        catalog.apply_page("drive", &page).unwrap();
        let context = VirtualDownloadContext {
            state: Arc::new(Mutex::new(EngineState::Connected)),
            catalog: Arc::new(Mutex::new(catalog)),
            download_in_progress: Arc::new(AtomicBool::new(false)),
            upload_in_progress: Arc::new(AtomicBool::new(false)),
        };
        let stale = DownloadRequest {
            drive_id: "drive".into(),
            item_id: "file".into(),
            etag: "v1".into(),
            size: 5,
        };
        assert!(prepare_virtual_request(&context, stale).await.is_err());
        assert_eq!(*context.state.lock().unwrap(), EngineState::Connected);
        assert!(
            !context
                .download_in_progress
                .load(std::sync::atomic::Ordering::SeqCst)
        );
    }
}
