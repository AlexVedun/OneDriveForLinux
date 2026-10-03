mod auth;
mod graph;
mod transfer;

use auth::LoginAttempt;
use graph::GraphClient;
use keyring::v1::Entry;
use oauth2::{TokenResponse, basic::BasicTokenResponse};
use onedrive_catalog::{Catalog, FetchError};
use onedrive_core::{BUS_NAME, EngineState, OBJECT_PATH};
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
    scan_in_progress: Arc<AtomicBool>,
    download_in_progress: Arc<AtomicBool>,
}

#[zbus::interface(name = "io.github.onedrive_linux.Daemon")]
impl Daemon {
    fn status(&self) -> String {
        let state = *self.state.lock().unwrap();
        if state == EngineState::Connected {
            let catalog = self.catalog.lock().unwrap();
            if let Ok(Some(drive_id)) = catalog.active_drive_id()
                && catalog.needs_reconcile(&drive_id).unwrap_or(false)
            {
                return EngineState::NeedsReconcile.as_str().to_owned();
            }
        }
        state.as_str().to_owned()
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

    fn refresh_catalog(&self) -> zbus::fdo::Result<()> {
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

fn client_id() -> Option<String> {
    std::env::var("ONEDRIVE_CLIENT_ID")
        .ok()
        .or_else(|| option_env!("ONEDRIVE_CLIENT_ID").map(str::to_owned))
        .filter(|value| !value.is_empty())
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
    catalog
        .lock()
        .unwrap()
        .set_active_drive_id(&drive_id)
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

fn catalog_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or("HOME is not set")?;
    let directory = state_home.join("onedrive-linux");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let path = directory.join("catalog.sqlite3");
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
    let catalog = Arc::new(Mutex::new(Catalog::open(&catalog_path()?)?));
    let scan_in_progress = Arc::new(AtomicBool::new(false));
    let download_in_progress = Arc::new(AtomicBool::new(false));
    let daemon = Daemon {
        state: Arc::clone(&state),
        login_in_progress: Arc::new(AtomicBool::new(false)),
        catalog: Arc::clone(&catalog),
        scan_in_progress: Arc::clone(&scan_in_progress),
        download_in_progress: Arc::clone(&download_in_progress),
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
            client_id,
        ));
    }
    eprintln!("onedrive-daemon listening on {BUS_NAME}");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
