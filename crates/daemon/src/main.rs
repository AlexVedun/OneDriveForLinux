use onedrive_core::{BUS_NAME, EngineState, OBJECT_PATH};
use zbus::connection::Builder;

struct Daemon;

#[zbus::interface(name = "io.github.onedrive_linux.Daemon")]
impl Daemon {
    /// Return a stable, machine-readable engine state.
    fn status(&self) -> &'static str {
        EngineState::NeedsLogin.as_str()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _connection = Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, Daemon)?
        .build()
        .await?;
    eprintln!("onedrive-daemon listening on {BUS_NAME}");
    tokio::signal::ctrl_c().await?;
    Ok(())
}
