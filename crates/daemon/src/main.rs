mod auth;

use auth::LoginAttempt;
use keyring::v1::Entry;
use oauth2::{TokenResponse, basic::BasicTokenResponse};
use onedrive_core::{BUS_NAME, EngineState, OBJECT_PATH};
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
}

#[zbus::interface(name = "io.github.onedrive_linux.Daemon")]
impl Daemon {
    fn status(&self) -> String {
        self.state.lock().unwrap().as_str().to_owned()
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(Mutex::new(EngineState::NeedsLogin));
    let daemon = Daemon {
        state: Arc::clone(&state),
        login_in_progress: Arc::new(AtomicBool::new(false)),
    };
    let _connection = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, daemon)?
        .build()
        .await?;
    if let Some(client_id) = client_id() {
        tokio::spawn(restore_login(state, client_id));
    }
    eprintln!("onedrive-daemon listening on {BUS_NAME}");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
