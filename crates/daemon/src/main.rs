mod auth;
mod graph;
mod transfer;

use auth::LoginAttempt;
use graph::{GraphClient, GraphUpload};
use keyring::v1::Entry;
use oauth2::{TokenResponse, basic::BasicTokenResponse};
use onedrive_catalog::{Catalog, FetchError};
use onedrive_core::{BUS_NAME, EngineState, OBJECT_PATH};
use onedrive_sync::{
    Action, SyncStore,
    upload::{UploadError, UploadStep, upload_queued},
};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Duration, timeout},
};
use zbus::connection::Builder;

const KEYRING_SERVICE: &str = "io.github.onedrive_linux";
const KEYRING_USER: &str = "personal";

struct Daemon {
    state: Arc<Mutex<EngineState>>,
    login_in_progress: Arc<AtomicBool>,
    catalog: Arc<Mutex<Catalog>>,
    sync: Arc<Mutex<SyncStore>>,
    scan_in_progress: Arc<AtomicBool>,
    download_in_progress: Arc<AtomicBool>,
    upload_in_progress: Arc<AtomicBool>,
}

#[zbus::interface(name = "io.github.onedrive_linux.Daemon")]
impl Daemon {
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
            if scan_catalog(catalog, client_id).await.is_ok() {
                *state.lock().unwrap() = EngineState::Connected;
            } else {
                *state.lock().unwrap() = EngineState::Error;
                eprintln!("catalog scan failed; request URLs and credentials were not logged");
            }
            in_progress.store(false, Ordering::SeqCst);
        });
        Ok(())
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
                        && !selector.contains('/')
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

fn client_id() -> Option<String> {
    std::env::var("ONEDRIVE_CLIENT_ID")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| client_id_from_dotenv(&fs::read_to_string(".env").ok()?))
        .or_else(|| option_env!("ONEDRIVE_CLIENT_ID").map(str::to_owned))
        .filter(|value| !value.is_empty())
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

async fn scan_catalog(catalog: Arc<Mutex<Catalog>>, client_id: String) -> Result<(), ()> {
    let access_token = access_token(&client_id).await?;
    let graph = GraphClient::new().map_err(|_| ())?;
    let drive_id = graph.drive_id(&access_token).await.map_err(|_| ())?;
    let root_id = graph.root_id(&access_token).await.map_err(|_| ())?;
    catalog
        .lock()
        .unwrap()
        .set_active_drive_id(&drive_id)
        .map_err(|_| ())?;
    catalog
        .lock()
        .unwrap()
        .set_root_id(&drive_id, &root_id)
        .map_err(|_| ())?;
    let mut handled_gone = false;
    loop {
        let url = catalog
            .lock()
            .unwrap()
            .request_url(&drive_id)
            .map_err(|_| ())?;
        let page = match graph.delta_page(&url, &access_token).await {
            Ok(page) => page,
            Err(FetchError::Gone) if !handled_gone => {
                catalog
                    .lock()
                    .unwrap()
                    .reset_expired_cursor(&drive_id)
                    .map_err(|_| ())?;
                handled_gone = true;
                continue;
            }
            Err(_) => return Err(()),
        };
        let complete = page.delta_link.is_some();
        catalog
            .lock()
            .unwrap()
            .apply_page(&drive_id, &page)
            .map_err(|_| ())?;
        if complete {
            break;
        }
    }
    Ok(())
}

async fn access_token(client_id: &str) -> Result<String, ()> {
    let refresh = tokio::task::spawn_blocking(|| {
        Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .and_then(|entry| entry.get_password())
            .map_err(|_| ())
    })
    .await
    .map_err(|_| ())??;
    let token = auth::refresh_token(client_id, &refresh)
        .await
        .map_err(|_| ())?;
    if let Some(rotated) = token.refresh_token() {
        store_refresh(rotated.secret().to_owned()).await?;
    }
    Ok(token.access_token().secret().to_owned())
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
        *state.lock().unwrap() = if result.is_ok() {
            EngineState::Connected
        } else {
            EngineState::Error
        };
        scan_in_progress.store(false, Ordering::SeqCst);
    }
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
    let daemon = Daemon {
        state: Arc::clone(&state),
        login_in_progress: Arc::new(AtomicBool::new(false)),
        catalog: Arc::clone(&catalog),
        sync,
        scan_in_progress: Arc::clone(&scan_in_progress),
        download_in_progress: Arc::clone(&download_in_progress),
        upload_in_progress: Arc::clone(&upload_in_progress),
    };
    let _connection = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, daemon)?
        .build()
        .await?;
    if let Some(client_id) = client_id() {
        tokio::spawn(restore_login(Arc::clone(&state), client_id.clone()));
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
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::client_id_from_dotenv;

    #[test]
    fn reads_client_id_without_matching_other_keys_or_empty_values() {
        let contents = "# registration\nOTHER_CLIENT_ID=wrong\nONEDRIVE_CLIENT_ID=\nONEDRIVE_CLIENT_ID=\"registered-id\"\n";
        assert_eq!(
            client_id_from_dotenv(contents).as_deref(),
            Some("registered-id")
        );
    }
}
