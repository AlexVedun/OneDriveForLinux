use adw::prelude::*;
use onedrive_core::{BUS_NAME, INTERFACE_NAME, OBJECT_PATH};

fn read_status() -> String {
    let result = (|| -> zbus::Result<String> {
        let connection = zbus::blocking::Connection::session()?;
        let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
        proxy.call("Status", &())
    })();
    result.unwrap_or_else(|_| "daemon-unavailable".to_owned())
}

fn main() {
    let app = adw::Application::builder()
        .application_id("io.github.onedrive_linux.Desktop")
        .build();
    app.connect_activate(|app| {
        let status = adw::StatusPage::builder()
            .icon_name("folder-remote-symbolic")
            .title("OneDrive для Linux")
            .description(format!("Состояние: {}", read_status()))
            .build();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("OneDrive для Linux")
            .default_width(540)
            .default_height(360)
            .content(&status)
            .build();
        window.present();
    });
    app.run();
}
