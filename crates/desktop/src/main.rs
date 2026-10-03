use adw::{gio, gtk, prelude::*};
use onedrive_core::{BUS_NAME, INTERFACE_NAME, OBJECT_PATH};

fn read_status() -> String {
    let result = (|| -> zbus::Result<String> {
        let connection = zbus::blocking::Connection::session()?;
        let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
        proxy.call("Status", &())
    })();
    result.unwrap_or_else(|_| "daemon-unavailable".to_owned())
}

fn begin_login() -> zbus::Result<String> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("BeginLogin", &())
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
        let login = gtk::Button::with_label("Войти через Microsoft");
        let status_after_login = status.clone();
        login.connect_clicked(move |_| match begin_login() {
            Ok(url) => {
                if gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>)
                    .is_err()
                {
                    status_after_login.set_description(Some("Не удалось открыть браузер"));
                }
            }
            Err(_) => status_after_login.set_description(Some("Не удалось начать вход")),
        });
        let refresh = gtk::Button::with_label("Обновить состояние");
        let status_for_refresh = status.clone();
        refresh.connect_clicked(move |_| {
            status_for_refresh.set_description(Some(&format!("Состояние: {}", read_status())));
        });
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.append(&status);
        content.append(&login);
        content.append(&refresh);
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("OneDrive для Linux")
            .default_width(540)
            .default_height(360)
            .content(&content)
            .build();
        window.present();
    });
    app.run();
}
