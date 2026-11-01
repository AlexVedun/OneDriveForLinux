mod locale;
mod tray;

use adw::{gio, gtk, prelude::*};
use onedrive_core::{BUS_NAME, DAEMON_API_REVISION, INTERFACE_NAME, OBJECT_PATH};
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "io.github.onedrive_linux";

fn t(english: &'static str, ukrainian: &'static str) -> &'static str {
    locale::text(english, ukrainian)
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct UiItem {
    item_id: String,
    parent_id: Option<String>,
    name: Option<String>,
    kind: String,
    size: Option<i64>,
    modified: Option<String>,
    deleted: bool,
}

#[derive(Deserialize)]
struct UiPreview {
    operations: Vec<UiOperation>,
    skipped: Vec<String>,
}

#[derive(Deserialize)]
struct UiOperation {
    action: String,
    path: String,
    from_path: Option<String>,
    requires_confirmation: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct UiReviewOperation {
    id: i64,
    kind: String,
    path: String,
    from_path: Option<String>,
    remote_id: Option<String>,
    expected_etag: Option<String>,
    can_approve: bool,
}

#[derive(Deserialize)]
struct UiOperationHistory {
    total: u64,
    operations: Vec<UiOperationSummary>,
}

#[derive(Debug, Deserialize)]
struct UiVirtualDraft {
    drive_id: String,
    item_id: String,
    name: Option<String>,
    status: String,
    last_upload_issue: Option<String>,
    last_upload_http_status: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct UiVirtualMutation {
    drive_id: String,
    item_id: String,
    name: String,
    status: String,
    kind: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct UiVirtualDirectory {
    name: String,
    status: String,
}

#[derive(Deserialize)]
struct UiOperationSummary {
    id: i64,
    kind: String,
    path: String,
    from_path: Option<String>,
    status: String,
    approved: bool,
    attempts: u32,
    last_error: Option<String>,
}

fn daemon_call<R>(method: &str) -> zbus::Result<R>
where
    R: for<'de> zbus::zvariant::DynamicDeserialize<'de>,
{
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call(method, &())
}

fn app_title() -> String {
    format!("OneDriveForLinux - {}", env!("CARGO_PKG_VERSION"))
}

fn login_error_text(error: &str) -> String {
    if error.contains("ONEDRIVE_CLIENT_ID is not configured") {
        t(
            "ONEDRIVE_CLIENT_ID is missing. Add it to ~/.config/onedrive-linux/config.env",
            "Не задано ONEDRIVE_CLIENT_ID. Додайте його до ~/.config/onedrive-linux/config.env",
        )
        .to_owned()
    } else if error.contains("login is already in progress") {
        t(
            "Sign-in is already in progress. Finish it in the browser or wait five minutes.",
            "Вхід уже триває. Завершіть його у браузері або зачекайте п’ять хвилин.",
        )
        .to_owned()
    } else {
        format!(
            "{}: {error}",
            t("Could not start sign-in", "Не вдалося розпочати вхід")
        )
    }
}

fn read_status() -> String {
    let status = daemon_call("Status").ok();
    let revision = status
        .as_ref()
        .and_then(|_| daemon_call("ApiRevision").ok());
    classify_daemon_status(status, revision)
}

fn classify_daemon_status(status: Option<String>, revision: Option<u32>) -> String {
    match (status, revision) {
        (Some(status), Some(DAEMON_API_REVISION)) => status,
        (Some(_), _) => "daemon-outdated".to_owned(),
        (None, _) => "daemon-unavailable".to_owned(),
    }
}

fn state_label(state: &str) -> &str {
    match state {
        "needs-login" => t("Microsoft sign-in required", "Потрібен вхід до Microsoft"),
        "error" => t("Sync error", "Помилка синхронізації"),
        "reconciliation-needed" => t("Catalog review required", "Потрібна перевірка каталогу"),
        "offline" => t("Offline", "Немає з’єднання"),
        "paused" => t("Sync paused", "Синхронізацію призупинено"),
        "transferring" => t("Transferring files", "Передавання файлів"),
        "checking" => t("Checking for changes", "Перевірка змін"),
        "connected" => t("Connected", "Підключено"),
        "synced" => t("Synced", "Синхронізовано"),
        "daemon-unavailable" => t(
            "Background daemon unavailable",
            "Фоновий процес недоступний",
        ),
        "daemon-outdated" => t(
            "An older daemon is running. Close the old application and restart the AppImage.",
            "Працює старий фоновий процес. Закрийте попередню програму й перезапустіть AppImage.",
        ),
        _ => state,
    }
}

fn attention_notification(
    previous: Option<&str>,
    current: &str,
) -> Option<(&'static str, &'static str)> {
    let previous = previous?;
    if previous == current {
        return None;
    }
    match current {
        "needs-login" => Some((
            t("OneDrive sign-in required", "Потрібен вхід до OneDrive"),
            t(
                "Open the application and sign in to your Microsoft account.",
                "Відкрийте програму й увійдіть до облікового запису Microsoft.",
            ),
        )),
        "error" => Some((
            t("Sync error", "Помилка синхронізації"),
            t(
                "Open the queue and history to see the cause.",
                "Відкрийте чергу та історію, щоб побачити причину.",
            ),
        )),
        "reconciliation-needed" => Some((
            t("OneDrive review required", "Потрібна перевірка OneDrive"),
            t(
                "Open the application and check the OneDrive catalog.",
                "Відкрийте програму й перевірте каталог OneDrive.",
            ),
        )),
        "offline" => Some((
            t("OneDrive unavailable", "OneDrive недоступний"),
            t(
                "Check your network connection.",
                "Перевірте мережеве з’єднання.",
            ),
        )),
        "daemon-unavailable" => Some((
            t(
                "Background daemon unavailable",
                "Фоновий процес недоступний",
            ),
            t(
                "Start the OneDrive daemon.",
                "Запустіть фоновий процес OneDrive.",
            ),
        )),
        "daemon-outdated" => Some((
            t(
                "An older OneDrive daemon is running",
                "Працює старий фоновий процес OneDrive",
            ),
            t(
                "Close the old application and restart the new AppImage.",
                "Закрийте попередню програму й перезапустіть новий AppImage.",
            ),
        )),
        _ => None,
    }
}

#[derive(Clone)]
struct StateMonitor {
    app: adw::Application,
    label: gtk::Label,
    pause: gtk::Button,
    pause_busy: Rc<Cell<bool>>,
    last: Rc<RefCell<String>>,
    pending: Rc<Cell<bool>>,
    tray: Option<Rc<tray::TrayIndicator>>,
}

impl StateMonitor {
    fn new(
        app: &adw::Application,
        label: &gtk::Label,
        pause: &gtk::Button,
        initial: String,
    ) -> Self {
        label.set_text(&format!(
            "{}: {}",
            t("Status", "Стан"),
            state_label(&initial)
        ));
        let app_for_menu = app.downgrade();
        let menu_action = Rc::new(move |action: tray::MenuAction| {
            let Some(app) = app_for_menu.upgrade() else {
                return;
            };
            let name = match action {
                tray::MenuAction::Show => "show",
                tray::MenuAction::OpenFolder => "open-sync-folder",
                tray::MenuAction::History => "history",
                tray::MenuAction::TogglePause => "toggle-pause",
                tray::MenuAction::Refresh => "refresh",
                tray::MenuAction::Quit => "quit",
            };
            if let Some(action) = app.lookup_action(name) {
                action.activate(None);
            }
        });
        Self {
            app: app.clone(),
            label: label.clone(),
            pause: pause.clone(),
            pause_busy: Rc::new(Cell::new(false)),
            tray: match tray::TrayIndicator::start(app, &initial, menu_action) {
                Ok(indicator) => Some(Rc::new(indicator)),
                Err(error) => {
                    eprintln!("status notifier unavailable: {error}");
                    None
                }
            },
            last: Rc::new(RefCell::new(initial)),
            pending: Rc::new(Cell::new(false)),
        }
    }

    fn observe(&self, current: String) {
        let previous = self.last.borrow();
        if previous.as_str() != current {
            if let Some(tray) = &self.tray {
                tray.update(&current);
            }
            if let Some((title, body)) = attention_notification(Some(previous.as_str()), &current) {
                let notification = gio::Notification::new(title);
                notification.set_body(Some(body));
                notification.set_default_action("app.show");
                self.app
                    .send_notification(Some("sync-status"), &notification);
            } else {
                self.app.withdraw_notification("sync-status");
            }
        }
        drop(previous);
        self.label.set_text(&format!(
            "{}: {}",
            t("Status", "Стан"),
            state_label(&current)
        ));
        self.last.replace(current);
    }

    fn update_paused(&self, paused: Option<bool>) {
        if !self.pause_busy.get() {
            match paused {
                Some(paused) => update_pause_button(&self.pause, paused),
                None => {
                    self.pause
                        .set_label(t("Pause unavailable", "Пауза недоступна"));
                    self.pause.set_sensitive(false);
                }
            }
        }
        if let Some(tray) = &self.tray {
            tray.update_paused(paused);
        }
    }

    fn update_root_available(&self, available: bool) {
        if let Some(tray) = &self.tray {
            tray.update_root_available(available);
        }
    }

    fn update_outstanding(&self, count: Option<u64>) {
        if let Some(tray) = &self.tray {
            tray.update_outstanding(count);
        }
    }

    fn start(&self) {
        let monitor = self.clone();
        gtk::glib::timeout_add_local(Duration::from_secs(10), move || {
            if monitor.pending.replace(true) {
                return gtk::glib::ControlFlow::Continue;
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send((
                    read_status(),
                    read_sync_paused().ok(),
                    read_sync_root().ok().flatten().is_some(),
                    read_outstanding_count().ok(),
                ));
            });
            let monitor_for_result = monitor.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok((current, paused, root_available, outstanding)) => {
                        monitor_for_result.observe(current);
                        monitor_for_result.update_paused(paused);
                        monitor_for_result.update_root_available(root_available);
                        monitor_for_result.update_outstanding(outstanding);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        monitor_for_result.observe("daemon-unavailable".to_owned());
                        monitor_for_result.update_paused(None);
                        monitor_for_result.update_root_available(false);
                        monitor_for_result.update_outstanding(None);
                    }
                }
                monitor_for_result.pending.set(false);
                gtk::glib::ControlFlow::Break
            });
            gtk::glib::ControlFlow::Continue
        });
    }
}

fn read_sync_paused() -> zbus::Result<bool> {
    daemon_call("SyncPaused")
}

fn read_outstanding_count() -> zbus::Result<u64> {
    daemon_call("SyncOutstandingCount")
}

fn toggle_sync_paused() -> zbus::Result<bool> {
    let paused = read_sync_paused()?;
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    let next = !paused;
    let _: () = proxy.call("SetSyncPaused", &next)?;
    read_sync_paused()
}

fn update_pause_button(button: &gtk::Button, paused: bool) {
    button.set_label(if paused {
        t("Resume sync", "Відновити синхронізацію")
    } else {
        t("Pause sync", "Призупинити синхронізацію")
    });
    button.set_sensitive(true);
}

fn read_catalog() -> Result<Vec<UiItem>, String> {
    let json: String = daemon_call("CatalogItems").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn download_item(item_id: &str, destination: &str) -> zbus::Result<()> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("DownloadItem", &(item_id, destination))
}

fn set_sync_root(path: &str) -> zbus::Result<String> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("SetSyncRoot", &(path))
}

fn validated_sync_root(root: &str) -> Result<Option<PathBuf>, String> {
    if root.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(root);
    if !path.is_absolute() || !path.is_dir() || path.canonicalize().ok().as_ref() != Some(&path) {
        return Err("Папка синхронизации недоступна или изменила путь".to_owned());
    }
    Ok(Some(path))
}

fn read_sync_root() -> Result<Option<PathBuf>, String> {
    let root: String = daemon_call("SyncRoot").map_err(|error| error.to_string())?;
    validated_sync_root(&root)
}

fn read_virtual_mount_path() -> zbus::Result<String> {
    daemon_call("VirtualMountPath")
}

fn read_virtual_configured_mount_path() -> zbus::Result<String> {
    daemon_call("VirtualConfiguredMountPath")
}

fn read_virtual_mount_recovery_path() -> zbus::Result<String> {
    daemon_call("VirtualMountRecoveryPath")
}

fn read_virtual_fuse_diagnostics() -> zbus::Result<(String, bool)> {
    daemon_call("VirtualFuseDiagnostics")
}

fn read_virtual_drafts() -> Result<Vec<UiVirtualDraft>, String> {
    let json: String = daemon_call("VirtualDrafts").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn read_virtual_mutations() -> Result<Vec<UiVirtualMutation>, String> {
    let json: String = daemon_call("VirtualMutations").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn read_virtual_directories() -> Result<Vec<UiVirtualDirectory>, String> {
    let json: String = daemon_call("VirtualDirectories").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn virtual_directories_text(directories: &[UiVirtualDirectory]) -> String {
    let pending = directories
        .iter()
        .filter(|item| item.status == "pending")
        .count();
    let waiting = directories
        .iter()
        .filter(|item| item.status == "waiting-parent")
        .count();
    let verifying = directories
        .iter()
        .filter(|item| item.status == "verifying")
        .count();
    let uploaded = directories
        .iter()
        .filter(|item| item.status == "uploaded")
        .count();
    let issues: Vec<_> = directories
        .iter()
        .filter(|item| matches!(item.status.as_str(), "parent-missing" | "name-conflict"))
        .collect();
    if directories.is_empty() {
        return String::new();
    }
    let mut text = format!(
        "Папки: {pending} к отправке · {waiting} ждут родителя · {verifying} проверяются · {uploaded} созданы · {} требуют сверки.",
        issues.len()
    );
    if let Some(issue) = issues.first() {
        let reason = if issue.status == "name-conflict" {
            "имя занято"
        } else {
            "родитель не найден"
        };
        text.push_str(&format!("\n• Папка «{}»: {reason}.", issue.name));
    }
    text
}

fn confirm_virtual_mass_delete(count: u64) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| error.to_string())?;
    proxy
        .call("ConfirmVirtualMassDelete", &(count))
        .map_err(|error| error.to_string())
}

fn cancel_conflicted_virtual_mutation(drive_id: &str, item_id: &str) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| error.to_string())?;
    proxy
        .call("CancelConflictedVirtualMutation", &(drive_id, item_id))
        .map_err(|error| error.to_string())
}

fn virtual_mutations_text(mutations: &[UiVirtualMutation]) -> String {
    if mutations.is_empty() {
        return String::new();
    }
    let pending = mutations
        .iter()
        .filter(|item| item.status == "pending")
        .count();
    let conflicts = mutations
        .iter()
        .filter(|item| item.status == "conflict")
        .count();
    let applied = mutations
        .iter()
        .filter(|item| item.status == "applied")
        .count();
    let deletes = mutations
        .iter()
        .filter(|item| item.status == "pending" && item.kind == "delete")
        .count();
    let mut text =
        format!("Операции: {pending} ожидают · {applied} выполнены · {conflicts} требуют сверки.");
    if deletes > 10 {
        text.push_str("\n• Массовое удаление приостановлено.");
    }
    if let Some(issue) = mutations.iter().find(|item| item.status == "conflict") {
        text.push_str(&format!("\n• Конфликт: {}.", issue.name));
    }
    text
}

fn read_virtual_draft_upload_enabled() -> Result<bool, String> {
    daemon_call("VirtualDraftUploadEnabled").map_err(|error| error.to_string())
}

fn upload_virtual_draft(item_id: &str) -> Result<String, String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| error.to_string())?;
    proxy
        .call("RunVirtualDraftUpload", &(item_id))
        .map_err(|error| error.to_string())
}

fn reconcile_virtual_draft(item_id: &str) -> Result<String, String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| error.to_string())?;
    proxy
        .call("ReconcileVirtualDraft", &(item_id))
        .map_err(|error| error.to_string())
}

fn export_virtual_draft(drive_id: &str, item_id: &str, destination: &str) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| error.to_string())?;
    proxy
        .call("ExportVirtualDraft", &(drive_id, item_id, destination))
        .map_err(|error| error.to_string())
}

fn discard_virtual_draft(drive_id: &str, item_id: &str) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| error.to_string())?;
    proxy
        .call("DiscardVirtualDraft", &(drive_id, item_id))
        .map_err(|error| error.to_string())
}

fn virtual_draft_status_label(status: &str) -> &'static str {
    match status {
        "ready" => "готов к отправке; версия в каталоге совпадает",
        "ready-new" => "новый файл сохранён локально; ожидает отправки",
        "new-empty" => {
            "пустой новый файл сохранён локально; для отправки нужен экспериментальный режим"
        }
        "other-drive" => "другая учётная запись или диск",
        "catalog-incomplete" => "каталог ещё не сверён",
        "remote-missing" => "файл отсутствует в каталоге",
        "remote-not-file" => "объект больше не является файлом",
        "remote-changed" => "версия в каталоге изменилась",
        "name-conflict" => "в OneDrive уже есть объект с таким именем",
        "uncertain-new" => {
            "результат создания файла неизвестен; проверяется единственный объект с тем же именем"
        }
        "uncertain-empty-existing" => {
            "результат отправки пустого файла неизвестен; требуется проверка в OneDrive"
        }
        "uncertain-existing" => {
            "результат отправки файла неизвестен; требуется проверка содержимого в OneDrive"
        }
        "uncertain-empty-new" => {
            "результат создания пустого файла неизвестен; требуется сравнение с OneDrive"
        }
        _ => "неизвестное состояние",
    }
}

fn virtual_draft_upload_issue_label(issue: &str) -> &'static str {
    match issue {
        "login-required" => "нужен повторный вход",
        "remote-changed" => "облачный файл изменился",
        "transfer-failed" => "ошибка передачи",
        _ => "ошибка отправки",
    }
}

fn virtual_drafts_text(drafts: &[UiVirtualDraft]) -> String {
    if drafts.is_empty() {
        return "Локальных изменений для отправки нет".to_owned();
    }
    let attention = drafts
        .iter()
        .filter(|draft| draft.status != "ready")
        .count();
    let failed = drafts
        .iter()
        .filter(|draft| draft.last_upload_issue.is_some())
        .count();
    let mut lines = vec![format!(
        "Черновики: {} · требуют внимания: {attention} · ошибки отправки: {failed}.\nОткройте полный список для проверки.",
        drafts.len()
    )];
    for draft in drafts
        .iter()
        .filter(|draft| draft.status != "ready")
        .chain(drafts.iter().filter(|draft| draft.status == "ready"))
        .take(3)
    {
        let name = draft.name.as_deref().unwrap_or(&draft.item_id);
        lines.push(format!(
            "• {name} ({}) — {}",
            draft.drive_id,
            virtual_draft_status_label(&draft.status)
        ));
        if let Some(issue) = &draft.last_upload_issue {
            let http_status = draft
                .last_upload_http_status
                .map(|status| format!(" (HTTP {status})"))
                .unwrap_or_default();
            lines.push(format!(
                "  Предыдущая фоновая отправка: {}{http_status}",
                virtual_draft_upload_issue_label(issue),
            ));
        }
    }
    if drafts.len() > 3 {
        lines.push(format!("И ещё {} локальных изменений", drafts.len() - 3));
    }
    lines.join("\n")
}

fn fuse_diagnostics_label(device: &str, helper: bool) -> &'static str {
    match (device, helper) {
        ("device-missing", _) => {
            "FUSE: отсутствует /dev/fuse. Проверьте модуль ядра и доступность устройства."
        }
        ("device-permission", _) => {
            "FUSE: нет доступа к /dev/fuse. Проверьте разрешения устройства."
        }
        ("device-error", _) => "FUSE: не удалось проверить /dev/fuse.",
        ("ready", false) => {
            "FUSE: устройство доступно. Помощник fusermount3 не найден; он может потребоваться для подключения."
        }
        ("ready", true) => "FUSE: устройство и помощник монтирования доступны.",
        _ => "FUSE: неизвестное состояние устройства.",
    }
}

fn virtual_mount_paths() -> zbus::Result<(String, String, String)> {
    let current = read_virtual_mount_path()?;
    let recovery = read_virtual_mount_recovery_path()?;
    let configured = read_virtual_configured_mount_path()?;
    Ok((current, recovery, configured))
}

fn mount_virtual_drive(path: &str) -> zbus::Result<String> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("MountVirtualDrive", &(path))
}

fn unmount_virtual_drive() -> zbus::Result<()> {
    daemon_call("UnmountVirtualDrive")
}

fn set_virtual_file_pinned(item_id: &str, pinned: bool) -> zbus::Result<String> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("SetVirtualFilePinned", &(item_id, pinned))
}

fn read_virtual_cache_stats() -> zbus::Result<(u64, u64)> {
    daemon_call("VirtualCacheStats")
}

fn set_virtual_cache_limit(bytes: u64) -> zbus::Result<(u64, u64)> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("SetVirtualCacheLimit", &(bytes))
}

fn clear_virtual_cache() -> zbus::Result<(u64, u64)> {
    daemon_call("ClearVirtualCache")
}

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Clone)]
struct VirtualCacheControls {
    label: gtk::Label,
    size: gtk::SpinButton,
    apply: gtk::Button,
    busy: Rc<Cell<bool>>,
    dirty: Rc<Cell<bool>>,
}

impl VirtualCacheControls {
    fn new(label: &gtk::Label, size: &gtk::SpinButton, apply: &gtk::Button) -> Self {
        let dirty = Rc::new(Cell::new(false));
        let dirty_on_edit = Rc::clone(&dirty);
        size.connect_value_changed(move |_| dirty_on_edit.set(true));
        Self {
            label: label.clone(),
            size: size.clone(),
            apply: apply.clone(),
            busy: Rc::new(Cell::new(false)),
            dirty,
        }
    }

    fn show_stats(&self, usage: u64, limit: u64) {
        self.label.set_text(&format!(
            "{}: {:.1} {} {} {:.2} {}",
            t("Cache", "Кеш"),
            usage as f64 / (1024 * 1024) as f64,
            t("MiB", "МіБ"),
            t("of", "з"),
            limit as f64 / GIB as f64,
            t("GiB", "ГіБ"),
        ));
        if !self.dirty.get() {
            self.size.set_value(limit as f64 / GIB as f64);
            self.dirty.set(false);
        }
        self.apply.set_sensitive(true);
    }

    fn refresh(&self) {
        if self.busy.replace(true) {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(read_virtual_cache_stats());
        });
        let controls = self.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return gtk::glib::ControlFlow::Continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    controls.label.set_text(t(
                        "Could not read cache status",
                        "Не вдалося прочитати стан кешу",
                    ));
                    controls.apply.set_sensitive(false);
                    controls.busy.set(false);
                    return gtk::glib::ControlFlow::Break;
                }
            };
            match result {
                Ok((usage, limit)) => controls.show_stats(usage, limit),
                Err(error) => {
                    controls.label.set_text(&format!(
                        "{}: {error}",
                        t("Cache unavailable", "Кеш недоступний")
                    ));
                    controls.apply.set_sensitive(false);
                }
            }
            controls.busy.set(false);
            gtk::glib::ControlFlow::Break
        });
    }

    fn apply(&self, status: &gtk::Label) {
        if self.busy.replace(true) {
            return;
        }
        let bytes = (self.size.value() * GIB as f64).round() as u64;
        self.apply.set_sensitive(false);
        status.set_text(t("Changing cache limit…", "Зміна обмеження кешу…"));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(set_virtual_cache_limit(bytes));
        });
        let controls = self.clone();
        let status = status.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return gtk::glib::ControlFlow::Continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    controls.apply.set_sensitive(true);
                    controls.busy.set(false);
                    status.set_text(t(
                        "Could not change cache limit",
                        "Не вдалося змінити обмеження кешу",
                    ));
                    return gtk::glib::ControlFlow::Break;
                }
            };
            match result {
                Ok((usage, limit)) => {
                    controls.dirty.set(false);
                    controls.show_stats(usage, limit);
                    status.set_text(t("Cache limit saved", "Обмеження кешу збережено"));
                }
                Err(error) => {
                    controls.apply.set_sensitive(true);
                    status.set_text(&format!(
                        "{}: {error}",
                        t(
                            "Could not change cache limit",
                            "Не вдалося змінити обмеження кешу"
                        )
                    ));
                }
            }
            controls.busy.set(false);
            gtk::glib::ControlFlow::Break
        });
    }

    fn clear(&self, button: &gtk::Button, status: &gtk::Label) {
        if self.busy.replace(true) {
            return;
        }
        button.set_sensitive(false);
        status.set_text(t("Clearing cache…", "Очищення кешу…"));
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(clear_virtual_cache());
        });
        let controls = self.clone();
        let button = button.clone();
        let status = status.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return gtk::glib::ControlFlow::Continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    status.set_text(t("Could not clear cache", "Не вдалося очистити кеш"));
                    button.set_sensitive(true);
                    controls.busy.set(false);
                    return gtk::glib::ControlFlow::Break;
                }
            };
            match result {
                Ok((cleared, _)) => {
                    status.set_text(&format!(
                        "{}: {:.1} {}. {}",
                        t("Cache cleared; freed", "Кеш очищено; звільнено"),
                        cleared as f64 / (1024 * 1024) as f64,
                        t("MiB", "МіБ"),
                        t("Offline files are kept.", "Офлайн-файли збережено.")
                    ));
                    controls.busy.set(false);
                    controls.refresh();
                }
                Err(error) => {
                    status.set_text(&format!(
                        "{}: {error}",
                        t("Could not clear cache", "Не вдалося очистити кеш")
                    ));
                    controls.busy.set(false);
                }
            }
            button.set_sensitive(true);
            gtk::glib::ControlFlow::Break
        });
    }
}

#[derive(Clone)]
struct VirtualMountControls {
    label: gtk::Label,
    diagnostics: gtk::Label,
    drafts: gtk::Label,
    mount: gtk::Button,
    unmount: gtk::Button,
    busy: Rc<Cell<bool>>,
}

impl VirtualMountControls {
    fn new(
        label: &gtk::Label,
        diagnostics: &gtk::Label,
        drafts: &gtk::Label,
        mount: &gtk::Button,
        unmount: &gtk::Button,
    ) -> Self {
        Self {
            label: label.clone(),
            diagnostics: diagnostics.clone(),
            drafts: drafts.clone(),
            mount: mount.clone(),
            unmount: unmount.clone(),
            busy: Rc::new(Cell::new(false)),
        }
    }

    fn show_diagnostics(&self, result: zbus::Result<(String, bool)>) {
        match result {
            Ok((device, helper)) => {
                self.diagnostics
                    .set_text(&locale::summary(fuse_diagnostics_label(&device, helper)));
                if device != "ready" {
                    self.mount.set_sensitive(false);
                }
            }
            Err(_) => {
                self.diagnostics.set_text(t(
                    "FUSE: daemon diagnostics unavailable",
                    "FUSE: діагностика фонового процесу недоступна",
                ));
                self.mount.set_sensitive(false);
            }
        }
    }

    fn show_drafts(
        &self,
        result: Result<Vec<UiVirtualDraft>, String>,
        mutations: Result<Vec<UiVirtualMutation>, String>,
        directories: Result<Vec<UiVirtualDirectory>, String>,
    ) {
        match result {
            Ok(drafts) => {
                let mut text = virtual_drafts_text(&drafts);
                if let Ok(mutations) = mutations {
                    let summary = virtual_mutations_text(&mutations);
                    if !summary.is_empty() {
                        text.push_str("\n\n");
                        text.push_str(&summary);
                    }
                }
                if let Ok(directories) = directories {
                    let summary = virtual_directories_text(&directories);
                    if !summary.is_empty() {
                        text.push_str("\n\n");
                        text.push_str(&summary);
                    }
                }
                self.drafts.set_text(&locale::summary(&text));
            }
            Err(_) => self.drafts.set_text(t(
                "Could not check local changes",
                "Не вдалося перевірити локальні зміни",
            )),
        }
    }

    fn show_path(&self, paths: Option<(&str, &str, &str)>) {
        match paths {
            Some(("", "", "")) => {
                self.label.set_text(t(
                    "Virtual drive is not mounted",
                    "Віртуальний диск не підключено",
                ));
                self.mount.set_sensitive(true);
                self.unmount.set_sensitive(false);
            }
            Some(("", recovery, _)) if !recovery.is_empty() => {
                self.label.set_text(&format!(
                    "{}: {recovery}. {}",
                    t(
                        "Previous mount point is busy",
                        "Попередня точка монтування зайнята"
                    ),
                    t(
                        "Inspect it and unmount with fusermount3 -u",
                        "Перевірте її та відключіть через fusermount3 -u"
                    )
                ));
                self.mount.set_sensitive(false);
                self.unmount.set_sensitive(false);
            }
            Some(("", "", configured)) => {
                self.label.set_text(&format!(
                    "{}: {configured}",
                    t(
                        "Virtual drive is not mounted. Configured folder",
                        "Віртуальний диск не підключено. Налаштована тека"
                    )
                ));
                self.mount.set_sensitive(true);
                self.unmount.set_sensitive(false);
            }
            Some((path, _, _)) => {
                self.label.set_text(&format!(
                    "{}: {path}",
                    t("Virtual drive", "Віртуальний диск")
                ));
                self.mount.set_sensitive(false);
                self.unmount.set_sensitive(true);
            }
            None => {
                self.label.set_text(t(
                    "Virtual drive: daemon unavailable",
                    "Віртуальний диск: фоновий процес недоступний",
                ));
                self.mount.set_sensitive(false);
                self.unmount.set_sensitive(false);
            }
        }
    }

    fn refresh(&self) {
        if self.busy.replace(true) {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send((
                virtual_mount_paths(),
                read_virtual_fuse_diagnostics(),
                read_virtual_drafts(),
                read_virtual_mutations(),
                read_virtual_directories(),
            ));
        });
        let controls = self.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return gtk::glib::ControlFlow::Continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    controls.show_path(None);
                    controls.busy.set(false);
                    return gtk::glib::ControlFlow::Break;
                }
            };
            controls.show_path(
                result
                    .0
                    .as_ref()
                    .ok()
                    .map(|(current, recovery, configured)| {
                        (current.as_str(), recovery.as_str(), configured.as_str())
                    }),
            );
            controls.show_diagnostics(result.1);
            controls.show_drafts(result.2, result.3, result.4);
            controls.busy.set(false);
            gtk::glib::ControlFlow::Break
        });
    }

    fn run(&self, mount_path: Option<String>, status: &gtk::Label) {
        if self.busy.replace(true) {
            return;
        }
        self.mount.set_sensitive(false);
        self.unmount.set_sensitive(false);
        status.set_text(if mount_path.is_some() {
            t("Mounting virtual drive…", "Підключення віртуального диска…")
        } else {
            t(
                "Unmounting virtual drive…",
                "Відключення віртуального диска…",
            )
        });
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = match mount_path {
                Some(path) => mount_virtual_drive(&path)
                    .map(|_| t("Virtual drive mounted", "Віртуальний диск підключено")),
                None => unmount_virtual_drive()
                    .map(|()| t("Virtual drive unmounted", "Віртуальний диск відключено")),
            };
            let path = virtual_mount_paths();
            let diagnostics = read_virtual_fuse_diagnostics();
            let drafts = read_virtual_drafts();
            let mutations = read_virtual_mutations();
            let directories = read_virtual_directories();
            let _ = sender.send((result, path, diagnostics, drafts, mutations, directories));
        });
        let controls = self.clone();
        let status = status.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return gtk::glib::ControlFlow::Continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    controls.show_path(None);
                    status.set_text(t(
                        "Could not complete virtual-drive operation",
                        "Не вдалося виконати операцію з віртуальним диском",
                    ));
                    controls.busy.set(false);
                    return gtk::glib::ControlFlow::Break;
                }
            };
            controls.show_path(
                result
                    .1
                    .as_ref()
                    .ok()
                    .map(|(current, recovery, configured)| {
                        (current.as_str(), recovery.as_str(), configured.as_str())
                    }),
            );
            controls.show_diagnostics(result.2);
            controls.show_drafts(result.3, result.4, result.5);
            match result.0 {
                Ok(message) => status.set_text(message),
                Err(error) => status.set_text(&format!(
                    "{}: {error}",
                    t("Virtual drive", "Віртуальний диск")
                )),
            }
            controls.busy.set(false);
            gtk::glib::ControlFlow::Break
        });
    }
}

fn read_sync_preview() -> Result<UiPreview, String> {
    let json: String = daemon_call("PreviewSync").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn read_review_operations() -> Result<Vec<UiReviewOperation>, String> {
    let json: String = daemon_call("ReviewOperations").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn read_operation_history() -> Result<UiOperationHistory, String> {
    let json: String = daemon_call("OperationHistory").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn queue_and_read_review() -> Result<(Vec<UiReviewOperation>, Option<String>), String> {
    let queue_result: zbus::Result<u64> = daemon_call("QueueReviewActions");
    let operations = read_review_operations()?;
    Ok((
        operations,
        queue_result.err().map(|error| error.to_string()),
    ))
}

fn decide_review_operation(id: i64, approve: bool) -> zbus::Result<()> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call(
        if approve {
            "ApproveOperation"
        } else {
            "DismissOperation"
        },
        &(id),
    )
}

fn review_title(operation: &UiReviewOperation) -> String {
    let action = match operation.kind.as_str() {
        "DeleteRemote" => "Удалить в OneDrive",
        "DeleteLocal" => "Убрать локальный файл",
        "RenameRemote" => "Переименовать в OneDrive",
        "Conflict" => "Конфликт",
        _ => "Требует рассмотрения",
    };
    let path = operation.from_path.as_ref().map_or_else(
        || operation.path.clone(),
        |source| format!("{source} → {}", operation.path),
    );
    format!("{action}: {path}")
}

fn review_details(operation: &UiReviewOperation) -> String {
    format!(
        "№{} · ID OneDrive: {} · eTag: {}",
        operation.id,
        operation.remote_id.as_deref().unwrap_or("—"),
        operation.expected_etag.as_deref().unwrap_or("—"),
    )
}

fn review_approvable(operation: &UiReviewOperation) -> bool {
    operation.can_approve
        && matches!(
            operation.kind.as_str(),
            "DeleteRemote" | "DeleteLocal" | "RenameRemote"
        )
}

fn history_text(history: UiOperationHistory) -> String {
    let mut lines = vec![format!(
        "Операций всего: {} · показано: {}",
        history.total,
        history.operations.len()
    )];
    for operation in history.operations {
        let state = match (operation.status.as_str(), operation.approved) {
            ("pending", false) => "ожидает подтверждения",
            ("pending", true) => "ожидает выполнения",
            ("running", _) => "выполняется",
            ("blocked", _) => "заблокирована",
            ("done", _) => "завершена",
            ("dismissed", _) => "отклонена",
            _ => operation.status.as_str(),
        };
        let action = match operation.kind.as_str() {
            "UploadNew" => "Загрузка нового файла",
            "UploadChanged" => "Загрузка изменений",
            "DownloadNew" => "Скачивание нового файла",
            "DownloadChanged" => "Скачивание изменений",
            "CreateRemoteFolder" => "Создание папки в OneDrive",
            "RenameRemote" => "Переименование в OneDrive",
            "RenameLocal" => "Локальное переименование",
            "DeleteRemote" => "Удаление в OneDrive",
            "DeleteLocal" => "Локальное удаление",
            "Conflict" => "Конфликт",
            _ => operation.kind.as_str(),
        };
        let path = operation.from_path.as_ref().map_or_else(
            || operation.path.clone(),
            |source| format!("{source} → {}", operation.path),
        );
        lines.push(format!(
            "[{state}] №{} · {action}: {path} · попыток: {}",
            operation.id, operation.attempts
        ));
        if let Some(error) = operation.last_error {
            lines.push(format!("    Причина: {error}"));
        }
    }
    lines.join("\n")
}

fn preview_text(preview: UiPreview) -> String {
    let mut lines = vec![format!(
        "Операций: {} · пропущено: {}",
        preview.operations.len(),
        preview.skipped.len()
    )];
    for operation in preview.operations {
        lines.push(format!(
            "{}{}  {}",
            if operation.requires_confirmation {
                "⚠ "
            } else {
                ""
            },
            operation.action,
            operation.from_path.map_or_else(
                || operation.path.clone(),
                |source| format!("{source} → {}", operation.path),
            )
        ));
    }
    for path in preview.skipped {
        lines.push(format!("Пропущено: {path}"));
    }
    lines.join("\n")
}

fn connect_review_decision(
    button: &gtk::Button,
    approve: bool,
    operation_id: i64,
    row: &gtk::Box,
    rows: &gtk::Box,
    status: &gtk::Label,
    controls: &[gtk::Button],
) {
    let row = row.clone();
    let rows = rows.clone();
    let status = status.clone();
    let controls = controls.to_vec();
    button.connect_clicked(move |_| {
        for control in &controls {
            control.set_sensitive(false);
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(decide_review_operation(operation_id, approve));
        });
        let row = row.clone();
        let rows = rows.clone();
        let status = status.clone();
        let controls = controls.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            match receiver.try_recv() {
                Ok(Ok(())) => {
                    rows.remove(&row);
                    status.set_text(if approve {
                        t(
                            "Operation approved and remains queued",
                            "Операцію підтверджено й залишено в черзі",
                        )
                    } else {
                        t("Operation dismissed", "Операцію відхилено")
                    });
                    if rows.first_child().is_none() {
                        rows.append(&gtk::Label::new(Some(t(
                            "No operations to review",
                            "Немає операцій для розгляду",
                        ))));
                    }
                    gtk::glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    status.set_text(&format!(
                        "{}: {error}",
                        t("Could not save decision", "Не вдалося зберегти рішення")
                    ));
                    for control in &controls {
                        control.set_sensitive(true);
                    }
                    gtk::glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    status.set_text(t("Could not save decision", "Не вдалося зберегти рішення"));
                    for control in &controls {
                        control.set_sensitive(true);
                    }
                    gtk::glib::ControlFlow::Break
                }
            }
        });
    });
}

fn present_review_dialog(
    parent: &adw::ApplicationWindow,
    status: &gtk::Label,
    operations: Vec<UiReviewOperation>,
) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let hint = gtk::Label::new(Some(t(
        "Approval saves the decision. Execution starts separately.",
        "Підтвердження зберігає рішення. Виконання запускається окремо.",
    )));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    content.append(&hint);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
    if operations.is_empty() {
        rows.append(&gtk::Label::new(Some(t(
            "No operations to review",
            "Немає операцій для розгляду",
        ))));
    }
    for operation in operations {
        let approvable = review_approvable(&operation);
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let title = gtk::Label::new(Some(&locale::summary(&review_title(&operation))));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_selectable(true);
        row.append(&title);
        let details = gtk::Label::new(Some(&locale::summary(&review_details(&operation))));
        details.set_xalign(0.0);
        details.set_wrap(true);
        details.set_selectable(true);
        row.append(&details);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let mut controls = Vec::new();
        if approvable {
            let approve = gtk::Button::with_label(t("Approve", "Підтвердити"));
            buttons.append(&approve);
            controls.push(approve);
        } else {
            let conflict = gtk::Label::new(Some(t(
                "Conflict requires a separate resolution",
                "Конфлікт потребує окремого вирішення",
            )));
            conflict.set_xalign(0.0);
            buttons.append(&conflict);
        }
        let dismiss = gtk::Button::with_label(if approvable {
            t("Dismiss", "Відхилити")
        } else {
            t("Hide", "Приховати")
        });
        buttons.append(&dismiss);
        controls.push(dismiss);
        row.append(&buttons);
        rows.append(&row);
        for (index, control) in controls.iter().enumerate() {
            connect_review_decision(
                control,
                approvable && index == 0,
                operation.id,
                &row,
                &rows,
                status,
                &controls,
            );
        }
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&rows)
        .vexpand(true)
        .hexpand(true)
        .build();
    content.append(&scroll);
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .title(t("Operations to review", "Операції для розгляду"))
        .default_width(700)
        .default_height(460)
        .child(&content)
        .build();
    dialog.present();
}

fn present_virtual_mutation_conflicts_dialog(
    parent: &adw::ApplicationWindow,
    controls: &VirtualMountControls,
    mutations: Vec<UiVirtualMutation>,
) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let hint = gtk::Label::new(Some(t(
        "Unmount the virtual drive before cancelling. Then refresh the catalog and remount to see the current OneDrive state.",
        "Відключіть віртуальний диск перед скасуванням. Потім оновіть каталог і підключіть диск знову, щоб побачити поточний стан OneDrive.",
    )));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    content.append(&hint);
    let result = gtk::Label::new(None);
    result.set_xalign(0.0);
    result.set_wrap(true);
    content.append(&result);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let mut count = 0;
    for mutation in mutations
        .into_iter()
        .filter(|item| item.status == "conflict")
    {
        count += 1;
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let action = if mutation.kind == "delete" {
            t("deletion", "видалення").to_owned()
        } else {
            let target = mutation.kind["rename"]["target_name"]
                .as_str()
                .unwrap_or("?");
            format!("{} {target}", t("rename to", "перейменування на"))
        };
        let title = gtk::Label::new(Some(&format!("{} — {}", mutation.name, action)));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_wrap(true);
        row.append(&title);
        let cancel = gtk::Button::with_label(t("Cancel operation", "Скасувати операцію"));
        row.append(&cancel);
        rows.append(&row);
        let drive_id = mutation.drive_id;
        let item_id = mutation.item_id;
        let row_for_result = row.clone();
        let rows_for_result = rows.clone();
        let result_for_result = result.clone();
        let controls_for_result = controls.clone();
        cancel.connect_clicked(move |button| {
            button.set_sensitive(false);
            let drive_id = drive_id.clone();
            let item_id = item_id.clone();
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(cancel_conflicted_virtual_mutation(&drive_id, &item_id));
            });
            let button = button.clone();
            let row = row_for_result.clone();
            let rows = rows_for_result.clone();
            let result = result_for_result.clone();
            let controls = controls_for_result.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok(())) => {
                        rows.remove(&row);
                        result.set_text(t("Conflicting operation cancelled. Refresh the catalog before remounting.", "Конфліктну операцію скасовано. Оновіть каталог перед повторним підключенням."));
                        controls.refresh();
                    }
                    Ok(Err(error)) => {
                        result.set_text(&format!("{}: {error}", t("Could not cancel operation", "Не вдалося скасувати операцію")));
                        button.set_sensitive(true);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        result.set_text(t("Daemon unavailable", "Фоновий процес недоступний"));
                        button.set_sensitive(true);
                    }
                }
                gtk::glib::ControlFlow::Break
            });
        });
    }
    if count == 0 {
        rows.append(&gtk::Label::new(Some(t(
            "No conflicting operations",
            "Немає конфліктних операцій",
        ))));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&rows)
        .vexpand(true)
        .hexpand(true)
        .build();
    content.append(&scroll);
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .title(t("Virtual-drive conflicts", "Конфлікти віртуального диска"))
        .default_width(700)
        .default_height(400)
        .child(&content)
        .build();
    dialog.present();
}

fn present_virtual_drafts_dialog(
    parent: &adw::ApplicationWindow,
    controls: &VirtualMountControls,
    drafts: Vec<UiVirtualDraft>,
    upload_enabled: bool,
) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let hint = gtk::Label::new(Some(if upload_enabled {
        t(
            "Export saves a copy without removing the draft. Discard removes only local edits; the cloud file remains. Closed files upload automatically while the drive is mounted. Files in new local folders wait for their folder to upload. The sync folder must be disabled; the daemon checks OneDrive again before upload.",
            "Експорт зберігає копію без видалення чернетки. Видалення чернетки стирає лише локальні зміни; хмарний файл залишається. Закриті файли надсилаються автоматично, коли диск підключено. Файли в нових локальних теках чекають на надсилання теки. Теку синхронізації потрібно вимкнути; перед надсиланням фоновий процес знову перевіряє OneDrive.",
        )
    } else {
        t(
            "Export saves a copy without removing the draft. Discard removes only local edits; the cloud file remains. Upload is currently unavailable.",
            "Експорт зберігає копію без видалення чернетки. Видалення чернетки стирає лише локальні зміни; хмарний файл залишається. Надсилання зараз недоступне.",
        )
    }));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    content.append(&hint);
    let result = gtk::Label::new(None);
    result.set_xalign(0.0);
    result.set_wrap(true);
    result.set_selectable(true);
    content.append(&result);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
    if drafts.is_empty() {
        rows.append(&gtk::Label::new(Some(t(
            "No local changes",
            "Немає локальних змін",
        ))));
    }
    for draft in drafts {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let name = draft.name.as_deref().unwrap_or(&draft.item_id);
        let title = gtk::Label::new(Some(&format!("{name} ({})", draft.drive_id)));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_selectable(true);
        row.append(&title);
        let state = gtk::Label::new(Some(&locale::summary(virtual_draft_status_label(
            &draft.status,
        ))));
        state.set_xalign(0.0);
        row.append(&state);
        if let Some(issue) = &draft.last_upload_issue {
            let http_status = draft
                .last_upload_http_status
                .map(|status| format!(" (HTTP {status})"))
                .unwrap_or_default();
            let warning = gtk::Label::new(Some(&locale::summary(&format!(
                "Предыдущая фоновая отправка: {}{http_status}. Черновик сохранён локально.",
                virtual_draft_upload_issue_label(issue),
            ))));
            warning.set_xalign(0.0);
            warning.set_wrap(true);
            row.append(&warning);
        }
        let export = gtk::Button::with_label(t("Save local copy…", "Зберегти локальну копію…"));
        row.append(&export);
        let export_drive_id = draft.drive_id.clone();
        let export_item_id = draft.item_id.clone();
        let export_name = format!("{name}.local-copy");
        let export_parent = parent.clone();
        let export_result = result.clone();
        export.connect_clicked(move |button| {
            button.set_sensitive(false);
            let dialog = gtk::FileDialog::new();
            dialog.set_title(t("Save local draft", "Зберегти локальну чернетку"));
            dialog.set_initial_name(Some(&export_name));
            let button = button.clone();
            let result = export_result.clone();
            let drive_id = export_drive_id.clone();
            let item_id = export_item_id.clone();
            dialog.save(
                Some(&export_parent),
                None::<&gio::Cancellable>,
                move |selection| {
                    let Ok(file) = selection else {
                        button.set_sensitive(true);
                        return;
                    };
                    let Some(path) = file.path() else {
                        result.set_text(t(
                            "Choose a local path for the copy",
                            "Виберіть локальний шлях для копії",
                        ));
                        button.set_sensitive(true);
                        return;
                    };
                    result.set_text(t("Saving local copy…", "Збереження локальної копії…"));
                    let (sender, receiver) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = sender.send(export_virtual_draft(
                            &drive_id,
                            &item_id,
                            &path.to_string_lossy(),
                        ));
                    });
                    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                        match receiver.try_recv() {
                            Ok(Ok(())) => result.set_text(t(
                                "Local copy saved. Draft unchanged.",
                                "Локальну копію збережено. Чернетку не змінено.",
                            )),
                            Ok(Err(error)) => result.set_text(&format!(
                                "{}: {error}. {}",
                                t("Could not save copy", "Не вдалося зберегти копію"),
                                t(
                                    "Existing files are not overwritten.",
                                    "Наявні файли не перезаписуються."
                                )
                            )),
                            Err(std::sync::mpsc::TryRecvError::Empty) => {
                                return gtk::glib::ControlFlow::Continue;
                            }
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => result.set_text(t(
                                "Could not get save result",
                                "Не вдалося отримати результат збереження",
                            )),
                        }
                        button.set_sensitive(true);
                        gtk::glib::ControlFlow::Break
                    });
                },
            );
        });
        if matches!(
            draft.status.as_str(),
            "remote-changed"
                | "uncertain-empty-existing"
                | "uncertain-existing"
                | "uncertain-empty-new"
                | "uncertain-new"
        ) && upload_enabled
        {
            let compare =
                gtk::Button::with_label(t("Compare with OneDrive", "Порівняти з OneDrive"));
            row.append(&compare);
            let item_id = draft.item_id.clone();
            let result = result.clone();
            let controls = controls.clone();
            let rows = rows.clone();
            let row = row.clone();
            let state = state.clone();
            compare.connect_clicked(move |button| {
                button.set_sensitive(false);
                result.set_text(t("Comparing cloud file with local draft…", "Порівняння хмарного файлу з локальною чернеткою…"));
                let (sender, receiver) = std::sync::mpsc::channel();
                let item_id = item_id.clone();
                std::thread::spawn(move || {
                    let _ = sender.send(reconcile_virtual_draft(&item_id));
                });
                let button = button.clone();
                let result = result.clone();
                let controls = controls.clone();
                let rows = rows.clone();
                let row = row.clone();
                let state = state.clone();
                gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                    match receiver.try_recv() {
                        Ok(Ok(outcome)) if outcome == "identical-acknowledged" => {
                            rows.remove(&row);
                            result.set_text(t("Contents match. Cloud copy confirmed; draft closed.", "Вміст збігається. Хмарну копію підтверджено; чернетку закрито."));
                            controls.refresh();
                        }
                        Ok(Ok(outcome)) => {
                            state.set_text(if outcome == "different" {
                                t("Cloud file differs. Draft retained; compare copies manually.", "Хмарний файл відрізняється. Чернетку збережено; порівняйте копії вручну.")
                            } else {
                                t("Local data differs from the uploaded snapshot or changed during comparison. Draft retained.", "Локальні дані відрізняються від надісланого знімка або змінилися під час перевірки. Чернетку збережено.")
                            });
                            result.set_text(t("Draft unchanged.", "Чернетку не змінено."));
                            button.set_sensitive(true);
                            controls.refresh();
                        }
                        Ok(Err(error)) => {
                            result.set_text(&format!("{}: {error}", t("Could not compare files", "Не вдалося порівняти файли")));
                            button.set_sensitive(true);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            result.set_text(t("Could not get comparison result", "Не вдалося отримати результат порівняння"));
                            button.set_sensitive(true);
                        }
                    }
                    gtk::glib::ControlFlow::Break
                });
            });
        }
        let discard = gtk::Button::with_label(t("Discard draft…", "Видалити чернетку…"));
        row.append(&discard);
        let discard_parent = parent.clone();
        let discard_name = name.to_owned();
        let discard_drive_id = draft.drive_id.clone();
        let discard_item_id = draft.item_id.clone();
        let discard_result = result.clone();
        let discard_rows = rows.clone();
        let discard_row = row.clone();
        let discard_controls = controls.clone();
        discard.connect_clicked(move |button| {
            let confirmation = gtk::AlertDialog::builder()
                .message(format!("{} “{discard_name}”?", t("Discard local draft", "Видалити локальну чернетку")))
                .detail(t("Local edits will be permanently lost. The OneDrive file is not deleted. If a previous upload outcome is uncertain, a cloud copy may already exist. Save a local copy first if needed.", "Локальні зміни буде безповоротно втрачено. Файл у OneDrive не видаляється. Якщо результат попереднього надсилання невідомий, хмарна копія вже може існувати. За потреби спочатку збережіть локальну копію."))
                .build();
            confirmation.set_buttons(&[t("Cancel", "Скасувати"), t("Discard draft", "Видалити чернетку")]);
            confirmation.set_cancel_button(0);
            confirmation.set_default_button(0);
            let button = button.clone();
            let result = discard_result.clone();
            let rows = discard_rows.clone();
            let row = discard_row.clone();
            let controls = discard_controls.clone();
            let drive_id = discard_drive_id.clone();
            let item_id = discard_item_id.clone();
            confirmation.choose(Some(&discard_parent), None::<&gio::Cancellable>, move |response| {
                if !matches!(response, Ok(1)) {
                    return;
                }
                button.set_sensitive(false);
                result.set_text(t("Discarding local draft…", "Видалення локальної чернетки…"));
                let (sender, receiver) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(discard_virtual_draft(&drive_id, &item_id));
                });
                gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                    match receiver.try_recv() {
                        Ok(Ok(())) => {
                            rows.remove(&row);
                            result.set_text(t("Local draft discarded. Cloud file unchanged.", "Локальну чернетку видалено. Хмарний файл не змінено."));
                            controls.refresh();
                        }
                        Ok(Err(error)) => {
                            result.set_text(&format!("{}: {error}", t("Could not discard draft", "Не вдалося видалити чернетку")));
                            button.set_sensitive(true);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            result.set_text(t("Could not get discard result", "Не вдалося отримати результат видалення"));
                            button.set_sensitive(true);
                        }
                    }
                    gtk::glib::ControlFlow::Break
                });
            });
        });
        if upload_enabled && matches!(draft.status.as_str(), "ready" | "ready-new") {
            let send = gtk::Button::with_label(t("Upload to OneDrive", "Надіслати до OneDrive"));
            row.append(&send);
            let item_id = draft.item_id;
            let result = result.clone();
            let controls = controls.clone();
            let rows = rows.clone();
            let row = row.clone();
            let state = state.clone();
            send.connect_clicked(move |button| {
                button.set_sensitive(false);
                result.set_text(t("Uploading draft…", "Надсилання чернетки…"));
                let (sender, receiver) = std::sync::mpsc::channel();
                let item_id = item_id.clone();
                std::thread::spawn(move || {
                    let _ = sender.send(upload_virtual_draft(&item_id));
                });
                let button = button.clone();
                let result = result.clone();
                let controls = controls.clone();
                let rows = rows.clone();
                let row = row.clone();
                let state = state.clone();
                gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                    match receiver.try_recv() {
                        Ok(Ok(outcome)) => {
                            if outcome == "uploaded" {
                                rows.remove(&row);
                                result.set_text(t("Draft uploaded.", "Чернетку надіслано."));
                            } else {
                                state.set_text(t("File uploaded, but new local edits appeared. Reopen the list to review.", "Файл надіслано, але з’явилися нові локальні зміни. Відкрийте список знову для перевірки."));
                                result.set_text(t("New local edits retained.", "Нові локальні зміни збережено."));
                            }
                            controls.refresh();
                        }
                        Ok(Err(error)) => {
                            result.set_text(&format!("{}: {error}", t("Could not upload draft", "Не вдалося надіслати чернетку")));
                            button.set_sensitive(true);
                            controls.refresh();
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            return gtk::glib::ControlFlow::Continue;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            result.set_text(t("Could not get upload result", "Не вдалося отримати результат надсилання"));
                            button.set_sensitive(true);
                        }
                    }
                    gtk::glib::ControlFlow::Break
                });
            });
        }
        rows.append(&row);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&rows)
        .vexpand(true)
        .hexpand(true)
        .build();
    content.append(&scroll);
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .title(t(
            "Virtual-drive local changes",
            "Локальні зміни віртуального диска",
        ))
        .default_width(700)
        .default_height(460)
        .child(&content)
        .build();
    dialog.present();
}

fn flatten_tree(items: Vec<UiItem>) -> Vec<(usize, UiItem)> {
    let active: HashMap<String, UiItem> = items
        .into_iter()
        .filter(|item| !item.deleted)
        .map(|item| (item.item_id.clone(), item))
        .collect();
    let mut children: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for item in active.values() {
        let parent = item
            .parent_id
            .as_ref()
            .filter(|parent| active.contains_key(*parent))
            .cloned();
        children
            .entry(parent)
            .or_default()
            .push(item.item_id.clone());
    }
    for ids in children.values_mut() {
        ids.sort_by(|a, b| {
            let first = &active[a];
            let second = &active[b];
            (first.kind != "folder", first.name.as_deref(), a).cmp(&(
                second.kind != "folder",
                second.name.as_deref(),
                b,
            ))
        });
    }
    let mut flattened = Vec::new();
    let mut visited = HashSet::new();
    fn visit(
        parent: Option<&str>,
        depth: usize,
        active: &HashMap<String, UiItem>,
        children: &HashMap<Option<String>, Vec<String>>,
        visited: &mut HashSet<String>,
        flattened: &mut Vec<(usize, UiItem)>,
    ) {
        if let Some(ids) = children.get(&parent.map(str::to_owned)) {
            for id in ids {
                if !visited.insert(id.clone()) {
                    continue;
                }
                let item = active[id].clone();
                flattened.push((depth, item));
                visit(Some(id), depth + 1, active, children, visited, flattened);
            }
        }
    }
    visit(None, 0, &active, &children, &mut visited, &mut flattened);
    // Malformed remote parent cycles remain visible as top-level entries.
    for id in active.keys() {
        if !visited.contains(id) {
            let item = active[id].clone();
            flattened.push((0, item));
        }
    }
    flattened
}

fn populate_list(list: &gtk::ListBox, items: Vec<UiItem>) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for (depth, item) in flatten_tree(items) {
        let row = gtk::ListBoxRow::new();
        row.set_widget_name(&item.item_id);
        row.set_tooltip_text(item.name.as_deref());
        row.set_selectable(item.kind == "file");
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.set_margin_start((12 + depth.min(20) * 18) as i32);
        line.set_margin_end(12);
        line.set_margin_top(5);
        line.set_margin_bottom(5);
        let name = gtk::Label::new(Some(&format!(
            "{} {}",
            if item.kind == "folder" {
                "📁"
            } else {
                "📄"
            },
            item.name.as_deref().unwrap_or("(без имени)")
        )));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        line.append(&name);
        let details = match (item.size, item.modified.as_deref()) {
            (Some(size), Some(modified)) => format!("{size} Б · {modified}"),
            (Some(size), None) => format!("{size} Б"),
            (None, Some(modified)) => modified.to_owned(),
            (None, None) => String::new(),
        };
        let details = gtk::Label::new(Some(&details));
        line.append(&details);
        row.set_child(Some(&line));
        list.append(&row);
    }
}

fn refresh_view(
    monitor: &StateMonitor,
    status: &gtk::Label,
    list: &gtk::ListBox,
    pause: &gtk::Button,
) {
    if !monitor.pending.get() {
        monitor.observe(read_status());
    }
    match read_sync_paused() {
        Ok(paused) => {
            update_pause_button(pause, paused);
            monitor.update_paused(Some(paused));
        }
        Err(_) => {
            pause.set_label("Пауза недоступна");
            pause.set_sensitive(false);
            monitor.update_paused(None);
        }
    }
    monitor.update_root_available(read_sync_root().ok().flatten().is_some());
    monitor.update_outstanding(read_outstanding_count().ok());
    match read_catalog() {
        Ok(items) => populate_list(list, items),
        Err(_) => status.set_text("Не удалось прочитать каталог"),
    }
}

fn add_button_action(app: &adw::Application, name: &str, button: &gtk::Button) {
    let action = gio::SimpleAction::new(name, None);
    action.set_enabled(button.is_sensitive());
    let target = button.clone();
    action.connect_activate(move |_, _| {
        if target.is_sensitive() {
            target.emit_clicked();
        }
    });
    let action_for_sensitivity = action.clone();
    button.connect_notify_local(Some("sensitive"), move |button, _| {
        action_for_sensitivity.set_enabled(button.is_sensitive());
    });
    app.add_action(&action);
}

fn offline_file_ids(items: &[UiItem], selected: &HashSet<String>) -> Vec<String> {
    let active: HashMap<&str, &UiItem> = items
        .iter()
        .filter(|item| !item.deleted)
        .map(|item| (item.item_id.as_str(), item))
        .collect();
    items
        .iter()
        .filter(|item| item.kind == "file" && !item.deleted)
        .filter(|item| {
            let mut current = Some(item.item_id.as_str());
            let mut visited = HashSet::new();
            while let Some(id) = current {
                if selected.contains(id) {
                    return true;
                }
                if !visited.insert(id) {
                    break;
                }
                current = active.get(id).and_then(|entry| entry.parent_id.as_deref());
            }
            false
        })
        .map(|item| item.item_id.clone())
        .collect()
}

fn run_offline_batch(
    items: &[UiItem],
    selected: &Rc<RefCell<HashSet<String>>>,
    pinned: bool,
    pin_button: &gtk::Button,
    unpin_button: &gtk::Button,
    result: &gtk::Label,
    cache: &VirtualCacheControls,
) {
    let ids = offline_file_ids(items, &selected.borrow());
    if ids.is_empty() {
        result.set_text(t(
            "Select at least one file or a folder containing files",
            "Виберіть принаймні один файл або теку з файлами",
        ));
        return;
    }
    pin_button.set_sensitive(false);
    unpin_button.set_sensitive(false);
    result.set_text(&format!(
        "{}: {}…",
        t("Processing files", "Обробка файлів"),
        ids.len()
    ));
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut succeeded = 0;
        let mut failed = 0;
        let mut first_error = None;
        for id in ids {
            match set_virtual_file_pinned(&id, pinned) {
                Ok(_) => succeeded += 1,
                Err(error) => {
                    failed += 1;
                    if first_error.is_none() {
                        first_error = Some(error.to_string());
                    }
                }
            }
        }
        let _ = sender.send((succeeded, failed, first_error));
    });
    let result = result.clone();
    let pin_button = pin_button.clone();
    let unpin_button = unpin_button.clone();
    let cache = cache.clone();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        match receiver.try_recv() {
            Ok((succeeded, failed, first_error)) => {
                let action = if pinned {
                    t("Pinned", "Закріплено")
                } else {
                    t("Unpinned", "Відкріплено")
                };
                let error =
                    first_error.map_or(String::new(), |error| format!(". Первая ошибка: {error}"));
                result.set_text(&format!(
                    "{action}: {succeeded}; {}: {failed}{error}",
                    t("errors", "помилок")
                ));
                pin_button.set_sensitive(true);
                unpin_button.set_sensitive(true);
                cache.refresh();
                gtk::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                result.set_text(t(
                    "Could not finish processing files",
                    "Не вдалося завершити обробку файлів",
                ));
                pin_button.set_sensitive(true);
                unpin_button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            }
        }
    });
}

fn present_offline_dialog(
    parent: &adw::ApplicationWindow,
    items: Vec<UiItem>,
    cache: &VirtualCacheControls,
) {
    let selected = Rc::new(RefCell::new(HashSet::<String>::new()));
    let tree = gtk::Box::new(gtk::Orientation::Vertical, 3);
    let mut parents = vec![tree.clone()];
    for (depth, item) in flatten_tree(items.clone()) {
        if item.kind != "file" && item.kind != "folder" {
            continue;
        }
        parents.truncate(depth + 1);
        let Some(container) = parents.last() else {
            continue;
        };
        let check = gtk::CheckButton::with_label(&format!(
            "{} {}",
            if item.kind == "folder" {
                "📁"
            } else {
                "📄"
            },
            item.name.as_deref().unwrap_or(&item.item_id)
        ));
        let id = item.item_id.clone();
        let selected_for_check = Rc::clone(&selected);
        check.connect_toggled(move |check| {
            if check.is_active() {
                selected_for_check.borrow_mut().insert(id.clone());
            } else {
                selected_for_check.borrow_mut().remove(&id);
            }
        });
        if item.kind == "folder" {
            let expander = gtk::Expander::new(None);
            expander.set_label_widget(Some(&check));
            expander.set_expanded(false);
            let children = gtk::Box::new(gtk::Orientation::Vertical, 3);
            children.set_margin_start(20);
            expander.set_child(Some(&children));
            container.append(&expander);
            parents.push(children);
        } else {
            container.append(&check);
        }
    }
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.set_margin_start(16);
    body.set_margin_end(16);
    body.set_margin_top(16);
    body.set_margin_bottom(16);
    body.append(&gtk::Label::builder().label(t("Select multiple files or folders. A selected folder includes all nested files. Use the arrow to expand the tree.", "Виберіть кілька файлів або тек. Вибрана тека включає всі вкладені файли. Розгортайте дерево стрілкою.")).xalign(0.0).wrap(true).build());
    body.append(
        &gtk::ScrolledWindow::builder()
            .child(&tree)
            .vexpand(true)
            .build(),
    );
    let result = gtk::Label::new(Some(t(
        "Selected files are cached for offline use",
        "Вибрані файли зберігаються в кеші для роботи без мережі",
    )));
    result.set_xalign(0.0);
    result.set_wrap(true);
    body.append(&result);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let pin = gtk::Button::with_label(t("Keep offline", "Зберегти офлайн"));
    let unpin = gtk::Button::with_label(t("Remove offline pin", "Зняти закріплення"));
    actions.append(&pin);
    actions.append(&unpin);
    body.append(&actions);
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(t("Offline file access", "Офлайн-доступ до файлів"))
        .default_width(620)
        .default_height(560)
        .child(&body)
        .build();
    for (button, pinned) in [(&pin, true), (&unpin, false)] {
        let items = items.clone();
        let selected = Rc::clone(&selected);
        let pin = pin.clone();
        let unpin = unpin.clone();
        let result = result.clone();
        let cache = cache.clone();
        button.connect_clicked(move |_| {
            run_offline_batch(&items, &selected, pinned, &pin, &unpin, &result, &cache);
        });
    }
    dialog.present();
}

fn main() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let show_action = gio::SimpleAction::new("show", None);
    let app_for_show = app.downgrade();
    show_action.connect_activate(move |_, _| {
        if let Some(app) = app_for_show.upgrade() {
            app.activate();
        }
    });
    app.add_action(&show_action);
    app.connect_activate(|app| {
        gtk::Window::set_default_icon_name(APP_ID);
        let development_icons = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/icons");
        if development_icons.join("hicolor/index.theme").is_file()
            && let Some(display) = gtk::gdk::Display::default()
        {
            gtk::IconTheme::for_display(&display).add_search_path(&development_icons);
        }
        if let Some(window) = app
            .windows()
            .into_iter()
            .find(|window| window.is::<adw::ApplicationWindow>())
        {
            window.present();
            return;
        }
        let engine_status = gtk::Label::new(None);
        engine_status.set_xalign(0.0);
        let pause = gtk::Button::with_label(t("Pause unavailable", "Пауза недоступна"));
        pause.set_sensitive(false);
        let monitor = StateMonitor::new(app, &engine_status, &pause, read_status());
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        let login = gtk::Button::with_label(t("Sign in with Microsoft", "Увійти через Microsoft"));
        let status_after_login = status.clone();
        login.connect_clicked(move |_| {
            let result: zbus::Result<String> = daemon_call("BeginLogin");
            match result {
                Ok(url) => {
                    if gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>)
                        .is_err()
                    {
                        status_after_login.set_text(t("Could not open browser", "Не вдалося відкрити браузер"));
                    }
                }
                Err(error) => status_after_login.set_text(&login_error_text(&error.to_string())),
            }
        });
        let scan = gtk::Button::with_label(t("Refresh OneDrive", "Оновити OneDrive"));
        let status_after_scan = status.clone();
        scan.connect_clicked(move |button| {
            button.set_sensitive(false);
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result: zbus::Result<()> = daemon_call("RefreshCatalog");
                let _ = sender.send(result);
            });
            let button = button.clone();
            let status = status_after_scan.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok(())) => status.set_text(t("Catalog refresh started", "Оновлення каталогу розпочато")),
                    Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        status.set_text(t("Could not start catalog refresh", "Не вдалося розпочати оновлення каталогу"));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                }
                button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            });
        });
        let reload = gtk::Button::with_label(t("Refresh list", "Оновити список"));
        let status_for_reload = status.clone();
        let list_for_reload = list.clone();
        let pause_for_reload = pause.clone();
        let monitor_for_reload = monitor.clone();
        reload.connect_clicked(move |_| {
            refresh_view(
                &monitor_for_reload,
                &status_for_reload,
                &list_for_reload,
                &pause_for_reload,
            )
        });
        let download = gtk::Button::with_label(t("Download selected file", "Завантажити вибраний файл"));
        let choose_sync = gtk::Button::with_label(t("Sync folder", "Тека синхронізації"));
        let preview_sync = gtk::Button::with_label(t("Sync preview", "Попередній перегляд синхронізації"));
        let review = gtk::Button::with_label(t("Review changes", "Переглянути зміни"));
        let history = gtk::Button::with_label(t("Queue and history", "Черга та історія"));
        let mount = gtk::Button::with_label(t("Mount virtual drive", "Підключити віртуальний диск"));
        let unmount = gtk::Button::with_label(t("Unmount virtual drive", "Відключити віртуальний диск"));
        let show_drafts = gtk::Button::with_label(t("All local changes…", "Усі локальні зміни…"));
        let show_mutation_conflicts = gtk::Button::with_label(t("Virtual-drive conflicts…", "Конфлікти віртуального диска…"));
        let confirm_deletes = gtk::Button::with_label(t("Confirm bulk deletion…", "Підтвердити масове видалення…"));
        mount.set_sensitive(false);
        unmount.set_sensitive(false);
        let mount_label = gtk::Label::new(Some(t("Virtual drive: checking status…", "Віртуальний диск: перевірка стану…")));
        mount_label.set_xalign(0.0);
        mount_label.set_wrap(true);
        mount_label.set_selectable(true);
        let fuse_label = gtk::Label::new(Some(t("FUSE: checking status…", "FUSE: перевірка стану…")));
        fuse_label.set_xalign(0.0);
        fuse_label.set_wrap(true);
        let drafts_label = gtk::Label::new(Some(t("Local changes: checking status…", "Локальні зміни: перевірка стану…")));
        drafts_label.set_xalign(0.0);
        drafts_label.set_wrap(true);
        let mount_controls =
            VirtualMountControls::new(&mount_label, &fuse_label, &drafts_label, &mount, &unmount);
        let cache_label = gtk::Label::new(Some(t("Cache: checking status…", "Кеш: перевірка стану…")));
        cache_label.set_xalign(0.0);
        let cache_size = gtk::SpinButton::with_range(0.25, 64.0, 0.25);
        cache_size.set_digits(2);
        cache_size.set_numeric(true);
        let cache_apply = gtk::Button::with_label(t("Save cache limit", "Зберегти обмеження кешу"));
        cache_apply.set_sensitive(false);
        let cache_controls = VirtualCacheControls::new(&cache_label, &cache_size, &cache_apply);
        let cache_clear = gtk::Button::with_label(t("Clear cache", "Очистити кеш"));
        cache_clear.set_tooltip_text(Some(t("Remove temporary copies; offline files and local drafts are kept", "Видалити тимчасові копії; офлайн-файли та локальні чернетки збережуться")));
        let exit = gtk::Button::with_label(t("Quit", "Вийти"));
        let account_menu = gio::Menu::new();
        account_menu.append(Some(t("Sign in with Microsoft", "Увійти через Microsoft")), Some("app.login"));
        account_menu.append(Some(t("Quit", "Вийти")), Some("app.quit"));
        let sync_menu = gio::Menu::new();
        for (label, action) in [
            (t("Refresh OneDrive", "Оновити OneDrive"), "app.refresh"),
            (t("OneDrive files…", "Файли OneDrive…"), "app.browse-files"),
            (t("Sync folder", "Тека синхронізації"), "app.choose-sync"),
            (t("Open sync folder", "Відкрити теку синхронізації"), "app.open-sync-folder"),
            (t("Sync preview", "Попередній перегляд синхронізації"), "app.preview-sync"),
            (t("Review changes", "Переглянути зміни"), "app.review"),
            (t("Queue and history", "Черга та історія"), "app.history"),
            (t("Pause / resume", "Призупинити / відновити"), "app.toggle-pause"),
        ] {
            sync_menu.append(Some(label), Some(action));
        }
        let virtual_menu = gio::Menu::new();
        for (label, action) in [
            (t("Mount drive", "Підключити диск"), "app.mount"),
            (t("Unmount drive", "Відключити диск"), "app.unmount"),
            (t("Local changes", "Локальні зміни"), "app.show-drafts"),
            (t("Conflicts", "Конфлікти"), "app.mutation-conflicts"),
            (t("Confirm bulk deletion", "Підтвердити масове видалення"), "app.confirm-deletes"),
            (t("Choose files for offline access", "Вибрати файли для офлайн-доступу"), "app.offline-files"),
            (t("Save cache limit", "Зберегти обмеження кешу"), "app.cache-limit"),
        ] {
            virtual_menu.append(Some(label), Some(action));
        }
        let menu = gio::Menu::new();
        menu.append_submenu(Some(t("Account", "Обліковий запис")), &account_menu);
        menu.append_submenu(Some(t("Sync", "Синхронізація")), &sync_menu);
        menu.append_submenu(Some(t("Virtual drive", "Віртуальний диск")), &virtual_menu);
        let language_menu = gio::Menu::new();
        language_menu.append(Some("English"), Some("app.language-en"));
        language_menu.append(Some("Українська"), Some("app.language-uk"));
        menu.append_submenu(Some(t("Language", "Мова")), &language_menu);
        let menu_bar = gtk::PopoverMenuBar::from_model(Some(&menu));
        menu_bar.set_margin_start(18);
        menu_bar.set_margin_end(18);
        menu_bar.set_margin_top(6);
        menu_bar.set_margin_bottom(6);
        let account_status = gtk::Box::new(gtk::Orientation::Vertical, 6);
        account_status.set_margin_start(12);
        account_status.set_margin_end(12);
        account_status.set_margin_top(12);
        account_status.set_margin_bottom(12);
        account_status.append(&gtk::Label::builder().label(t("OneDrive status", "Стан OneDrive")).xalign(0.0).css_classes(["heading"]).build());
        account_status.append(&engine_status);
        account_status.append(&gtk::Label::builder().label(t("Latest action", "Остання дія")).xalign(0.0).css_classes(["heading"]).build());
        account_status.append(&status);
        let account_frame = gtk::Frame::new(None);
        account_frame.set_child(Some(&account_status));
        let virtual_status = gtk::Box::new(gtk::Orientation::Vertical, 6);
        virtual_status.set_margin_start(12);
        virtual_status.set_margin_end(12);
        virtual_status.set_margin_top(12);
        virtual_status.set_margin_bottom(12);
        virtual_status.append(&gtk::Label::builder().label(t("Virtual drive", "Віртуальний диск")).xalign(0.0).css_classes(["heading"]).build());
        virtual_status.append(&mount_label);
        virtual_status.append(&fuse_label);
        virtual_status.append(&gtk::Label::builder().label(t("Local changes and operations", "Локальні зміни та операції")).xalign(0.0).css_classes(["heading"]).build());
        virtual_status.append(&drafts_label);
        let virtual_frame = gtk::Frame::new(None);
        virtual_frame.set_child(Some(&virtual_status));
        let cache_controls_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        cache_controls_box.set_margin_start(12);
        cache_controls_box.set_margin_end(12);
        cache_controls_box.set_margin_top(12);
        cache_controls_box.set_margin_bottom(12);
        cache_controls_box.append(&gtk::Label::builder().label(t("Offline access and cache", "Офлайн-доступ і кеш")).xalign(0.0).css_classes(["heading"]).build());
        cache_controls_box.append(&gtk::Label::builder().label(t("Pinned files remain available offline. Choose files and folders from the Virtual drive menu.", "Закріплені файли доступні без мережі. Вибір файлів і тек — у меню «Віртуальний диск».")).xalign(0.0).wrap(true).build());
        cache_controls_box.append(&cache_label);
        let cache_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        cache_actions.append(&gtk::Label::new(Some(t("Cache limit (GiB):", "Обмеження кешу (ГіБ):"))));
        cache_actions.append(&cache_size);
        cache_actions.append(&cache_clear);
        cache_controls_box.append(&cache_actions);
        let cache_frame = gtk::Frame::new(None);
        cache_frame.set_child(Some(&cache_controls_box));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.append(&account_frame);
        content.append(&virtual_frame);
        content.append(&cache_frame);
        let title = app_title();
        let header = adw::HeaderBar::new();
        let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let title_icon = gtk::Image::from_icon_name(APP_ID);
        title_icon.set_pixel_size(24);
        title_box.append(&title_icon);
        let title_label = gtk::Label::new(Some(&title));
        title_label.add_css_class("title");
        title_box.append(&title_label);
        header.set_title_widget(Some(&title_box));
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&header);
        layout.append(&menu_bar);
        layout.append(&gtk::ScrolledWindow::builder().child(&content).vexpand(true).build());
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(&title)
            .default_width(850)
            .default_height(540)
            .content(&layout)
            .build();
        for (code, language) in [
            ("language-en", locale::Language::English),
            ("language-uk", locale::Language::Ukrainian),
        ] {
            let action = gio::SimpleAction::new(code, None);
            let feedback = status.clone();
            action.connect_activate(move |_, _| {
                match locale::save(language) {
                    Ok(()) => feedback.set_text(t(
                        "Language saved. Restart the application to apply it.",
                        "Мову збережено. Перезапустіть програму, щоб застосувати її.",
                    )),
                    Err(error) => feedback.set_text(&format!("Could not save language: {error}")),
                }
            });
            app.add_action(&action);
        }
        let files_menu = gio::Menu::new();
        files_menu.append(Some(t("Refresh list", "Оновити список")), Some("app.reload"));
        files_menu.append(Some(t("Download selected file", "Завантажити вибраний файл")), Some("app.download"));
        let files_root_menu = gio::Menu::new();
        files_root_menu.append_submenu(Some(t("Files", "Файли")), &files_menu);
        let files_layout = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let files_menu_bar = gtk::PopoverMenuBar::from_model(Some(&files_root_menu));
        files_menu_bar.set_margin_start(12);
        files_menu_bar.set_margin_end(12);
        files_layout.append(&files_menu_bar);
        files_layout.append(&gtk::Label::builder()
            .label(t("OneDrive catalog. Select a file and use the Files menu to download it.", "Каталог OneDrive. Виберіть файл і скористайтеся меню «Файли» для завантаження."))
            .xalign(0.0).wrap(true).margin_start(12).margin_end(12).build());
        let files_feedback = gtk::Label::new(None);
        files_feedback.set_xalign(0.0);
        files_feedback.set_wrap(true);
        files_feedback.set_margin_start(12);
        files_feedback.set_margin_end(12);
        files_layout.append(&files_feedback);
        files_layout.append(&gtk::ScrolledWindow::builder().child(&list).vexpand(true).build());
        let files_window = gtk::Window::builder()
            .application(app)
            .transient_for(&window)
            .title(t("OneDrive files", "Файли OneDrive"))
            .default_width(700)
            .default_height(560)
            .child(&files_layout)
            .build();
        files_window.connect_close_request(|window| {
            window.set_visible(false);
            gtk::glib::Propagation::Stop
        });
        let hold = app.hold();
        window.connect_close_request(move |window| {
            let _keep_app_running = &hold;
            window.set_visible(false);
            gtk::glib::Propagation::Stop
        });
        let app_for_exit = app.downgrade();
        exit.connect_clicked(move |_| {
            if let Some(app) = app_for_exit.upgrade() {
                app.quit();
            }
        });
        let list_for_download = list.clone();
        let status_for_download = status.clone();
        let window_for_download = files_window.clone();
        let feedback_for_download = files_feedback.clone();
        download.connect_clicked(move |_| {
            let Some(row) = list_for_download.selected_row() else {
                feedback_for_download.set_text(t("Select a file first", "Спочатку виберіть файл"));
                return;
            };
            let item_id = row.widget_name().to_string();
            let dialog = gtk::FileDialog::new();
            dialog.set_title(t("Save OneDrive file", "Зберегти файл OneDrive"));
            dialog.set_initial_name(Some(row.tooltip_text().as_deref().unwrap_or("download")));
            let status_after_dialog = status_for_download.clone();
            let feedback_after_dialog = feedback_for_download.clone();
            dialog.save(
                Some(&window_for_download),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(file) = result else { return };
                    let Some(path) = file.path() else {
                        feedback_after_dialog.set_text(t("Choose a local path", "Виберіть локальний шлях"));
                        return;
                    };
                    let destination = path.to_string_lossy();
                    let message = if download_item(&item_id, &destination).is_ok() {
                        t("Download started", "Завантаження розпочато")
                    } else {
                        t("Could not start download", "Не вдалося розпочати завантаження")
                    };
                    feedback_after_dialog.set_text(message);
                    status_after_dialog.set_text(message);
                },
            );
        });
        let window_for_folder = window.clone();
        let status_for_folder = status.clone();
        let monitor_for_folder = monitor.clone();
        choose_sync.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title(t("Choose sync folder", "Виберіть теку синхронізації"));
            let status_after_dialog = status_for_folder.clone();
            let monitor_after_dialog = monitor_for_folder.clone();
            dialog.select_folder(
                Some(&window_for_folder),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(folder) = result else { return };
                    let Some(path) = folder.path() else {
                        status_after_dialog.set_text(t("Choose a local folder", "Виберіть локальну теку"));
                        return;
                    };
                    match set_sync_root(&path.to_string_lossy()) {
                        Ok(_) => {
                            monitor_after_dialog.update_root_available(true);
                            status_after_dialog
                                .set_text(t("Folder saved. Review the sync preview.", "Теку збережено. Перевірте попередній перегляд синхронізації."));
                        }
                        Err(_) => {
                            status_after_dialog.set_text(t("Could not save sync folder", "Не вдалося зберегти теку синхронізації"))
                        }
                    }
                },
            );
        });
        let window_for_mount = window.clone();
        let status_for_mount = status.clone();
        let controls_for_mount = mount_controls.clone();
        mount.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title(t("Choose an empty folder for the virtual drive", "Виберіть порожню теку для віртуального диска"));
            let status = status_for_mount.clone();
            let controls = controls_for_mount.clone();
            dialog.select_folder(
                Some(&window_for_mount),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(folder) = result else { return };
                    let Some(path) = folder.path() else {
                        status.set_text(t("Choose a local folder for the virtual drive", "Виберіть локальну теку для віртуального диска"));
                        return;
                    };
                    controls.run(Some(path.to_string_lossy().into_owned()), &status);
                },
            );
        });
        let status_for_unmount = status.clone();
        let controls_for_unmount = mount_controls.clone();
        unmount.connect_clicked(move |_| {
            controls_for_unmount.run(None, &status_for_unmount);
        });
        let window_for_drafts = window.clone();
        let status_for_drafts = status.clone();
        let controls_for_drafts = mount_controls.clone();
        show_drafts.connect_clicked(move |button| {
            button.set_sensitive(false);
            status_for_drafts.set_text(t("Loading local changes…", "Завантаження локальних змін…"));
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send((read_virtual_drafts(), read_virtual_draft_upload_enabled()));
            });
            let button = button.clone();
            let parent = window_for_drafts.clone();
            let status = status_for_drafts.clone();
            let controls = controls_for_drafts.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                let response = match receiver.try_recv() {
                    Ok(response) => response,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        status.set_text(t("Could not load local changes", "Не вдалося завантажити локальні зміни"));
                        button.set_sensitive(true);
                        return gtk::glib::ControlFlow::Break;
                    }
                };
                match response {
                    (Ok(drafts), Ok(enabled)) => {
                        present_virtual_drafts_dialog(&parent, &controls, drafts, enabled);
                        status.set_text(t("Local changes loaded", "Локальні зміни завантажено"));
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        status.set_text(&format!("{}: {error}", t("Could not load local changes", "Не вдалося завантажити локальні зміни")));
                    }
                }
                button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            });
        });
        let window_for_conflicts = window.clone();
        let status_for_conflicts = status.clone();
        let controls_for_conflicts = mount_controls.clone();
        show_mutation_conflicts.connect_clicked(move |button| {
            button.set_sensitive(false);
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(read_virtual_mutations());
            });
            let button = button.clone();
            let window = window_for_conflicts.clone();
            let status = status_for_conflicts.clone();
            let controls = controls_for_conflicts.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok(mutations)) => {
                        present_virtual_mutation_conflicts_dialog(&window, &controls, mutations);
                    }
                    Ok(Err(error)) => status.set_text(&format!("{}: {error}", t("Could not load conflicts", "Не вдалося завантажити конфлікти"))),
                    Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => status.set_text(t("Daemon unavailable", "Фоновий процес недоступний")),
                }
                button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            });
        });
        let window_for_deletes = window.clone();
        let status_for_deletes = status.clone();
        let controls_for_deletes = mount_controls.clone();
        confirm_deletes.connect_clicked(move |button| {
            button.set_sensitive(false);
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = read_virtual_mutations().map(|mutations| {
                    mutations.iter().filter(|item| item.status == "pending" && item.kind == "delete").count() as u64
                });
                let _ = sender.send(result);
            });
            let button = button.clone();
            let window = window_for_deletes.clone();
            let status = status_for_deletes.clone();
            let controls = controls_for_deletes.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                let result = match receiver.try_recv() {
                    Ok(result) => result,
                    Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("Демон недоступен".to_owned()),
                };
                button.set_sensitive(true);
                match result {
                    Ok(count) if count > 10 => {
                        let dialog = gtk::AlertDialog::builder()
                            .message(format!("{} {count} {}?", t("Delete", "Видалити"), t("items from OneDrive", "об’єктів із OneDrive")))
                            .detail(t("Only operations that pass another cloud version and location check will run.", "Виконуватимуться лише операції, які пройшли повторну перевірку версії та розташування в хмарі."))
                            .build();
                        dialog.set_buttons(&[t("Cancel", "Скасувати"), t("Confirm deletion", "Підтвердити видалення")]);
                        dialog.set_cancel_button(0);
                        dialog.set_default_button(0);
                        let status = status.clone();
                        let controls = controls.clone();
                        dialog.choose(Some(&window), None::<&gio::Cancellable>, move |response| {
                            if !matches!(response, Ok(1)) { return; }
                            let (sender, receiver) = std::sync::mpsc::channel();
                            std::thread::spawn(move || { let _ = sender.send(confirm_virtual_mass_delete(count)); });
                            let status = status.clone();
                            let controls = controls.clone();
                            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                                match receiver.try_recv() {
                                    Ok(Ok(())) => { status.set_text(t("Bulk deletion confirmed", "Масове видалення підтверджено")); controls.refresh(); },
                                    Ok(Err(error)) => status.set_text(&format!("{}: {error}", t("Could not confirm deletion", "Не вдалося підтвердити видалення"))),
                                    Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                                    Err(std::sync::mpsc::TryRecvError::Disconnected) => status.set_text(t("Could not confirm deletion", "Не вдалося підтвердити видалення")),
                                }
                                gtk::glib::ControlFlow::Break
                            });
                        });
                    }
                    Ok(_) => status.set_text(t("No bulk deletion pending", "Немає масового видалення")),
                    Err(error) => status.set_text(&format!("{}: {error}", t("Could not check queue", "Не вдалося перевірити чергу"))),
                }
                gtk::glib::ControlFlow::Break
            });
        });
        let status_for_cache = status.clone();
        let cache_for_apply = cache_controls.clone();
        cache_apply.connect_clicked(move |_| {
            cache_for_apply.apply(&status_for_cache);
        });
        let status_for_clear = status.clone();
        let cache_for_clear = cache_controls.clone();
        cache_clear.connect_clicked(move |button| {
            cache_for_clear.clear(button, &status_for_clear);
        });
        let window_for_preview = window.clone();
        let status_for_preview = status.clone();
        preview_sync.connect_clicked(move |_| {
            status_for_preview.set_text(t("Scanning local folder…", "Сканування локальної теки…"));
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(read_sync_preview());
            });
            let parent = window_for_preview.clone();
            let status = status_for_preview.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                let Ok(result) = receiver.try_recv() else {
                    return gtk::glib::ControlFlow::Continue;
                };
                match result {
                    Ok(preview) => {
                        status.set_text(t("Preview ready. Changes have not been applied.", "Попередній перегляд готовий. Зміни ще не виконуються."));
                        let text = gtk::TextView::new();
                        text.set_editable(false);
                        text.set_monospace(true);
                        text.buffer().set_text(&locale::summary(&preview_text(preview)));
                        let scroll = gtk::ScrolledWindow::builder()
                            .child(&text)
                            .vexpand(true)
                            .hexpand(true)
                            .build();
                        let dialog = gtk::Window::builder()
                            .transient_for(&parent)
                            .title(t("Sync preview", "Попередній перегляд синхронізації"))
                            .default_width(650)
                            .default_height(450)
                            .child(&scroll)
                            .build();
                        dialog.present();
                    }
                    Err(error) => status.set_text(&format!("{}: {error}", t("Could not build sync preview", "Не вдалося побудувати попередній перегляд"))),
                }
                gtk::glib::ControlFlow::Break
            });
        });
        let window_for_review = window.clone();
        let status_for_review = status.clone();
        review.connect_clicked(move |button| {
            button.set_sensitive(false);
            status_for_review.set_text(t("Checking changes…", "Перевірка змін…"));
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(queue_and_read_review());
            });
            let button = button.clone();
            let parent = window_for_review.clone();
            let status = status_for_review.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok((operations, queue_error))) => {
                        match queue_error {
                            Some(error) => status.set_text(&format!("{}: {error}", t("Could not refresh preview; showing saved queue", "Не вдалося оновити план; показано збережену чергу"))),
                            None => status.set_text(&format!("{}: {}", t("Operations to review", "Операцій для розгляду"), operations.len())),
                        }
                        present_review_dialog(&parent, &status, operations);
                    }
                    Ok(Err(error)) => {
                        status.set_text(&format!("{}: {error}", t("Could not read queue", "Не вдалося прочитати чергу")));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        status.set_text(t("Could not read queue", "Не вдалося прочитати чергу"));
                    }
                }
                button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            });
        });
        let window_for_history = window.clone();
        let status_for_history = status.clone();
        history.connect_clicked(move |button| {
            button.set_sensitive(false);
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(read_operation_history());
            });
            let button = button.clone();
            let parent = window_for_history.clone();
            let status = status_for_history.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok(history)) => {
                        let text = gtk::TextView::new();
                        text.set_editable(false);
                        text.set_monospace(true);
                        text.buffer().set_text(&locale::summary(&history_text(history)));
                        let scroll = gtk::ScrolledWindow::builder()
                            .child(&text)
                            .vexpand(true)
                            .hexpand(true)
                            .build();
                        let dialog = gtk::Window::builder()
                            .transient_for(&parent)
                            .title(t("Queue and history", "Черга та історія"))
                            .default_width(750)
                            .default_height(500)
                            .child(&scroll)
                            .build();
                        dialog.present();
                    }
                    Ok(Err(error)) => {
                        status.set_text(&format!("{}: {error}", t("Could not read queue", "Не вдалося прочитати чергу")));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        status.set_text(t("Could not read queue", "Не вдалося прочитати чергу"));
                    }
                }
                button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            });
        });
        let status_for_pause = status.clone();
        let monitor_for_pause = monitor.clone();
        pause.connect_clicked(move |button| {
            monitor_for_pause.pause_busy.set(true);
            button.set_sensitive(false);
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(toggle_sync_paused());
            });
            let button = button.clone();
            let status = status_for_pause.clone();
            let monitor = monitor_for_pause.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok(paused)) => {
                        update_pause_button(&button, paused);
                        monitor.pause_busy.set(false);
                        monitor.update_paused(Some(paused));
                        status.set_text(if paused {
                            t("Sync paused; the current operation may finish", "Синхронізацію призупинено; поточна операція може завершитися")
                        } else {
                            t("Sync resumed", "Синхронізацію відновлено")
                        });
                    }
                    Ok(Err(error)) => {
                        monitor.pause_busy.set(false);
                        button.set_sensitive(true);
                        status.set_text(&format!("{}: {error}", t("Could not change pause state", "Не вдалося змінити стан паузи")));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        monitor.pause_busy.set(false);
                        button.set_sensitive(true);
                        status.set_text(t("Could not change pause state", "Не вдалося змінити стан паузи"));
                    }
                }
                gtk::glib::ControlFlow::Break
            });
        });
        for (name, button) in [
            ("login", &login),
            ("refresh", &scan),
            ("reload", &reload),
            ("download", &download),
            ("choose-sync", &choose_sync),
            ("preview-sync", &preview_sync),
            ("review", &review),
            ("history", &history),
            ("toggle-pause", &pause),
            ("mount", &mount),
            ("unmount", &unmount),
            ("show-drafts", &show_drafts),
            ("mutation-conflicts", &show_mutation_conflicts),
            ("confirm-deletes", &confirm_deletes),
            ("cache-limit", &cache_apply),
        ] {
            add_button_action(app, name, button);
        }
        let browse_files_action = gio::SimpleAction::new("browse-files", None);
        let files_window_for_action = files_window.clone();
        browse_files_action.connect_activate(move |_, _| {
            files_window_for_action.present();
        });
        app.add_action(&browse_files_action);
        let open_folder_action = gio::SimpleAction::new("open-sync-folder", None);
        let window_for_open = window.downgrade();
        let status_for_open = status.downgrade();
        open_folder_action.connect_activate(move |_, _| {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(read_sync_root());
            });
            let window = window_for_open.clone();
            let status = status_for_open.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                let result = match receiver.try_recv() {
                    Ok(result) => result,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Err(t("Could not read sync folder", "Не вдалося прочитати теку синхронізації").to_owned())
                    }
                };
                let message = match result {
                    Ok(Some(root)) => {
                        let uri = gio::File::for_path(&root).uri();
                        gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)
                            .err()
                            .map(|_| t("Could not open sync folder", "Не вдалося відкрити теку синхронізації").to_owned())
                    }
                    Ok(None) => Some(t("Choose a sync folder first", "Спочатку виберіть теку синхронізації").to_owned()),
                    Err(error) => Some(error),
                };
                if let Some(message) = message {
                    if let Some(status) = status.upgrade() {
                        status.set_text(&message);
                    }
                    if let Some(window) = window.upgrade() {
                        window.present();
                    }
                }
                gtk::glib::ControlFlow::Break
            });
        });
        app.add_action(&open_folder_action);
        let offline_action = gio::SimpleAction::new("offline-files", None);
        let window_for_offline = window.downgrade();
        let status_for_offline = status.downgrade();
        let cache_for_offline = cache_controls.clone();
        offline_action.connect_activate(move |_, _| {
            if let Some(status) = status_for_offline.upgrade() {
                status.set_text(t("Loading offline file tree…", "Завантаження дерева файлів для офлайн-доступу…"));
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(read_catalog());
            });
            let window = window_for_offline.clone();
            let status = status_for_offline.clone();
            let cache = cache_for_offline.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                match receiver.try_recv() {
                    Ok(Ok(items)) => {
                        if let Some(window) = window.upgrade() {
                            present_offline_dialog(&window, items, &cache);
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Ok(Err(error)) => {
                        if let Some(status) = status.upgrade() {
                            status.set_text(&format!("{}: {error}", t("Could not open file list", "Не вдалося відкрити список файлів")));
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if let Some(status) = status.upgrade() {
                            status.set_text(t("Could not open file list", "Не вдалося відкрити список файлів"));
                        }
                        gtk::glib::ControlFlow::Break
                    }
                }
            });
        });
        app.add_action(&offline_action);
        let quit_action = gio::SimpleAction::new("quit", None);
        let app_for_quit = app.downgrade();
        quit_action.connect_activate(move |_, _| {
            if let Some(app) = app_for_quit.upgrade() {
                app.quit();
            }
        });
        app.add_action(&quit_action);
        refresh_view(&monitor, &status, &list, &pause);
        mount_controls.refresh();
        gtk::glib::timeout_add_local(Duration::from_secs(10), move || {
            mount_controls.refresh();
            gtk::glib::ControlFlow::Continue
        });
        cache_controls.refresh();
        gtk::glib::timeout_add_local(Duration::from_secs(30), move || {
            cache_controls.refresh();
            gtk::glib::ControlFlow::Continue
        });
        monitor.start();
        window.present();
    });
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_shows_workspace_version_and_login_error_explains_missing_configuration() {
        assert_eq!(app_title(), "OneDriveForLinux - 1.0.0");
        assert!(
            login_error_text(
                "org.freedesktop.DBus.Error.Failed: ONEDRIVE_CLIENT_ID is not configured"
            )
            .contains("~/.config/onedrive-linux/config.env")
        );
        assert!(login_error_text("daemon unavailable").contains("daemon unavailable"));
    }

    #[test]
    fn virtual_drafts_show_conflicts_and_manual_upload_summary() {
        let drafts: Vec<UiVirtualDraft> = serde_json::from_str(
            r#"[
              {"drive_id":"drive","item_id":"1","name":"note.txt","status":"ready","last_upload_issue":"transfer-failed","last_upload_http_status":400},
              {"drive_id":"drive","item_id":"2","name":null,"status":"remote-changed"}
            ]"#,
        )
        .unwrap();
        let text = virtual_drafts_text(&drafts);
        assert!(text.contains("Черновики: 2 · требуют внимания: 1"));
        assert!(text.contains("ошибки отправки: 1"));
        assert!(text.contains("Предыдущая фоновая отправка: ошибка передачи (HTTP 400)"));
        assert!(text.contains("note.txt (drive) — готов к отправке; версия в каталоге совпадает"));
        assert!(text.contains("2 (drive) — версия в каталоге изменилась"));
        assert!(text.contains("Откройте полный список для проверки"));
    }

    #[test]
    fn virtual_mutation_summary_flags_conflicts_and_mass_delete() {
        let mut mutations = (0..11)
            .map(|index| UiVirtualMutation {
                drive_id: "drive".into(),
                item_id: format!("item-{index}"),
                name: format!("file-{index}"),
                status: "pending".into(),
                kind: serde_json::json!("delete"),
            })
            .collect::<Vec<_>>();
        mutations.push(UiVirtualMutation {
            drive_id: "drive".into(),
            item_id: "changed".into(),
            name: "changed.txt".into(),
            status: "conflict".into(),
            kind: serde_json::json!({"rename": {"target_parent_id":"root", "target_name":"new.txt"}}),
        });
        let text = virtual_mutations_text(&mutations);
        assert!(text.contains("Массовое удаление приостановлено"));
        assert!(text.contains("Конфликт: changed.txt"));
    }

    #[test]
    fn virtual_directory_summary_names_first_conflict() {
        let directories = vec![
            UiVirtualDirectory {
                name: "parent".into(),
                status: "uploaded".into(),
            },
            UiVirtualDirectory {
                name: "child".into(),
                status: "waiting-parent".into(),
            },
            UiVirtualDirectory {
                name: "checking".into(),
                status: "verifying".into(),
            },
            UiVirtualDirectory {
                name: "duplicate".into(),
                status: "name-conflict".into(),
            },
        ];
        let text = virtual_directories_text(&directories);
        assert!(text.contains("1 ждут родителя"));
        assert!(text.contains("1 проверяются"));
        assert!(text.contains("1 созданы"));
        assert!(text.contains("Папка «duplicate»: имя занято"));
    }

    #[test]
    fn sync_folder_opening_requires_configured_canonical_directory() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(validated_sync_root("").unwrap(), None);
        assert_eq!(
            validated_sync_root(&folder.to_string_lossy()).unwrap(),
            Some(folder.clone())
        );
        assert!(validated_sync_root(&folder.join("missing").to_string_lossy()).is_err());
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&folder, &link).unwrap();
        assert!(validated_sync_root(&link.to_string_lossy()).is_err());
    }

    #[test]
    fn tree_uses_ids_for_parenting_and_skips_deleted_items() {
        let items: Vec<UiItem> = serde_json::from_str(
            r#"[
              {"item_id":"child","parent_id":"folder","name":"a.txt","kind":"file","size":4,"modified":null,"deleted":false},
              {"item_id":"folder","parent_id":null,"name":"Renamed","kind":"folder","size":null,"modified":null,"deleted":false},
              {"item_id":"gone","parent_id":null,"name":"old.txt","kind":"file","size":2,"modified":null,"deleted":true}
            ]"#,
        )
        .unwrap();
        let tree = flatten_tree(items);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].1.item_id, "folder");
        assert_eq!(tree[1].1.item_id, "child");
        assert_eq!(tree[1].0, 1);
    }

    #[test]
    fn offline_selection_expands_nested_folders_and_deduplicates_files() {
        let items: Vec<UiItem> = serde_json::from_str(
            r#"[
              {"item_id":"root","parent_id":null,"name":"Root","kind":"folder","size":null,"modified":null,"deleted":false},
              {"item_id":"nested","parent_id":"root","name":"Nested","kind":"folder","size":null,"modified":null,"deleted":false},
              {"item_id":"a","parent_id":"root","name":"a.txt","kind":"file","size":1,"modified":null,"deleted":false},
              {"item_id":"b","parent_id":"nested","name":"b.txt","kind":"file","size":1,"modified":null,"deleted":false},
              {"item_id":"gone","parent_id":"nested","name":"gone.txt","kind":"file","size":1,"modified":null,"deleted":true},
              {"item_id":"other","parent_id":null,"name":"other.txt","kind":"file","size":1,"modified":null,"deleted":false}
            ]"#,
        )
        .unwrap();
        let selected = HashSet::from(["root".to_owned(), "b".to_owned()]);
        assert_eq!(offline_file_ids(&items, &selected), vec!["a", "b"]);
        let selected = HashSet::from(["nested".to_owned(), "other".to_owned()]);
        assert_eq!(offline_file_ids(&items, &selected), vec!["b", "other"]);
    }

    #[test]
    fn review_queue_preserves_operation_identity_and_blocks_conflict_approval() {
        let operations: Vec<UiReviewOperation> = serde_json::from_str(
            r#"[
              {"id":12,"kind":"RenameRemote","path":"new.txt","from_path":"old.txt","remote_id":"remote-1","expected_etag":"etag-1","can_approve":true},
              {"id":13,"kind":"Conflict","path":"clash.txt","from_path":null,"remote_id":null,"expected_etag":null,"can_approve":false},
              {"id":14,"kind":"Conflict","path":"unsafe.txt","from_path":null,"remote_id":null,"expected_etag":null,"can_approve":true}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            review_title(&operations[0]),
            "Переименовать в OneDrive: old.txt → new.txt"
        );
        assert_eq!(
            review_details(&operations[0]),
            "№12 · ID OneDrive: remote-1 · eTag: etag-1"
        );
        assert!(review_approvable(&operations[0]));
        assert!(!review_approvable(&operations[1]));
        assert!(!review_approvable(&operations[2]));
    }

    #[test]
    fn operation_history_shows_blocked_reason_and_pending_review() {
        let history: UiOperationHistory = serde_json::from_str(
            r#"{"total":3,"operations":[
              {"id":8,"kind":"DownloadChanged","path":"new.txt","from_path":null,"status":"blocked","approved":true,"attempts":2,"last_error":"remote file changed"},
              {"id":7,"kind":"RenameRemote","path":"new-name.txt","from_path":"old-name.txt","status":"pending","approved":false,"attempts":0,"last_error":null}
            ]}"#,
        )
        .unwrap();
        let text = history_text(history);
        assert!(text.contains("Операций всего: 3 · показано: 2"));
        assert!(text.contains("[заблокирована] №8 · Скачивание изменений: new.txt · попыток: 2"));
        assert!(text.contains("Причина: remote file changed"));
        assert!(text.contains(
            "[ожидает подтверждения] №7 · Переименование в OneDrive: old-name.txt → new-name.txt"
        ));
    }

    #[test]
    fn notifications_only_follow_new_attention_states() {
        assert!(attention_notification(None, "error").is_none());
        assert!(attention_notification(Some("error"), "error").is_none());
        assert!(attention_notification(Some("connected"), "offline").is_some());
        assert!(attention_notification(Some("offline"), "connected").is_none());
        assert!(attention_notification(Some("connected"), "paused").is_none());
        assert!(attention_notification(Some("paused"), "reconciliation-needed").is_some());
        assert_eq!(
            state_label("daemon-unavailable"),
            "Background daemon unavailable"
        );
    }

    #[test]
    fn old_daemon_is_not_reported_as_current_after_appimage_rebuild() {
        assert_eq!(
            classify_daemon_status(Some("connected".to_owned()), Some(DAEMON_API_REVISION)),
            "connected"
        );
        assert_eq!(
            classify_daemon_status(Some("connected".to_owned()), None),
            "daemon-outdated"
        );
        assert_eq!(
            classify_daemon_status(Some("connected".to_owned()), Some(DAEMON_API_REVISION + 1)),
            "daemon-outdated"
        );
        assert_eq!(classify_daemon_status(None, None), "daemon-unavailable");
        assert!(state_label("daemon-outdated").contains("older daemon"));
    }

    #[test]
    fn fuse_diagnostics_identifies_missing_device_and_helper() {
        assert!(fuse_diagnostics_label("device-missing", true).contains("/dev/fuse"));
        assert!(fuse_diagnostics_label("device-permission", true).contains("нет доступа"));
        assert!(fuse_diagnostics_label("ready", false).contains("fusermount3"));
    }
}
