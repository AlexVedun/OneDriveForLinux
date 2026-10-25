mod tray;

use adw::{gio, gtk, prelude::*};
use onedrive_core::{BUS_NAME, DAEMON_API_REVISION, INTERFACE_NAME, OBJECT_PATH};
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

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
        "Не задан ONEDRIVE_CLIENT_ID. Добавьте его в ~/.config/onedrive-linux/config.env".to_owned()
    } else if error.contains("login is already in progress") {
        "Вход уже выполняется. Завершите его в браузере или подождите пять минут".to_owned()
    } else {
        format!("Не удалось начать вход: {error}")
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
        "needs-login" => "Требуется вход в Microsoft",
        "error" => "Ошибка синхронизации",
        "reconciliation-needed" => "Нужна сверка каталога",
        "offline" => "Нет соединения",
        "paused" => "Синхронизация приостановлена",
        "transferring" => "Передача файлов",
        "checking" => "Проверка изменений",
        "connected" => "Подключено",
        "synced" => "Синхронизировано",
        "daemon-unavailable" => "Фоновый процесс недоступен",
        "daemon-outdated" => {
            "Работает старый фоновый процесс. Завершите прежнее приложение и запустите AppImage заново"
        }
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
            "Требуется вход в OneDrive",
            "Откройте приложение и войдите в аккаунт Microsoft.",
        )),
        "error" => Some((
            "Ошибка синхронизации",
            "Откройте очередь и историю, чтобы увидеть причину.",
        )),
        "reconciliation-needed" => Some((
            "Нужна сверка OneDrive",
            "Откройте приложение и проверьте каталог OneDrive.",
        )),
        "offline" => Some(("OneDrive недоступен", "Проверьте соединение с сетью.")),
        "daemon-unavailable" => Some((
            "Фоновый процесс недоступен",
            "Запустите демон OneDrive для Linux.",
        )),
        "daemon-outdated" => Some((
            "Запущен старый демон OneDrive",
            "Завершите прежнее приложение и запустите новый AppImage заново.",
        )),
        _ => None,
    }
}

#[derive(Clone)]
struct StateMonitor {
    app: adw::Application,
    label: gtk::Label,
    last: Rc<RefCell<String>>,
    pending: Rc<Cell<bool>>,
    tray: Option<Rc<tray::TrayIndicator>>,
}

impl StateMonitor {
    fn new(app: &adw::Application, label: &gtk::Label, initial: String) -> Self {
        label.set_text(&format!("Состояние: {}", state_label(&initial)));
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
        self.label
            .set_text(&format!("Состояние: {}", state_label(&current)));
        self.last.replace(current);
    }

    fn update_paused(&self, paused: Option<bool>) {
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
        "Возобновить синхронизацию"
    } else {
        "Приостановить синхронизацию"
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
            "Кэш: {:.1} МиБ из {:.2} ГиБ",
            usage as f64 / (1024 * 1024) as f64,
            limit as f64 / GIB as f64
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
                    controls
                        .label
                        .set_text("Не удалось прочитать состояние кэша");
                    controls.apply.set_sensitive(false);
                    controls.busy.set(false);
                    return gtk::glib::ControlFlow::Break;
                }
            };
            match result {
                Ok((usage, limit)) => controls.show_stats(usage, limit),
                Err(error) => {
                    controls.label.set_text(&format!("Кэш недоступен: {error}"));
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
        status.set_text("Изменение лимита кэша…");
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
                    status.set_text("Не удалось изменить лимит кэша");
                    return gtk::glib::ControlFlow::Break;
                }
            };
            match result {
                Ok((usage, limit)) => {
                    controls.dirty.set(false);
                    controls.show_stats(usage, limit);
                    status.set_text("Лимит кэша сохранён");
                }
                Err(error) => {
                    controls.apply.set_sensitive(true);
                    status.set_text(&format!("Не удалось изменить лимит кэша: {error}"));
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
        status.set_text("Очистка кэша…");
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
                    status.set_text("Не удалось очистить кэш");
                    button.set_sensitive(true);
                    controls.busy.set(false);
                    return gtk::glib::ControlFlow::Break;
                }
            };
            match result {
                Ok((cleared, _)) => {
                    status.set_text(&format!(
                        "Кэш очищен: освобождено {:.1} МиБ. Офлайн-файлы сохранены.",
                        cleared as f64 / (1024 * 1024) as f64
                    ));
                    controls.busy.set(false);
                    controls.refresh();
                }
                Err(error) => {
                    status.set_text(&format!("Не удалось очистить кэш: {error}"));
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
                    .set_text(fuse_diagnostics_label(&device, helper));
                if device != "ready" {
                    self.mount.set_sensitive(false);
                }
            }
            Err(_) => {
                self.diagnostics
                    .set_text("FUSE: диагностика демона недоступна");
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
                self.drafts.set_text(&text);
            }
            Err(_) => self
                .drafts
                .set_text("Не удалось проверить локальные изменения"),
        }
    }

    fn show_path(&self, paths: Option<(&str, &str, &str)>) {
        match paths {
            Some(("", "", "")) => {
                self.label.set_text("Виртуальный диск не подключён");
                self.mount.set_sensitive(true);
                self.unmount.set_sensitive(false);
            }
            Some(("", recovery, _)) if !recovery.is_empty() => {
                self.label.set_text(&format!("Прежняя точка монтирования занята: {recovery}. Проверьте её и отключите через fusermount3 -u"));
                self.mount.set_sensitive(false);
                self.unmount.set_sensitive(false);
            }
            Some(("", "", configured)) => {
                self.label.set_text(&format!(
                    "Виртуальный диск не подключён. Настроена папка: {configured}"
                ));
                self.mount.set_sensitive(true);
                self.unmount.set_sensitive(false);
            }
            Some((path, _, _)) => {
                self.label.set_text(&format!("Виртуальный диск: {path}"));
                self.mount.set_sensitive(false);
                self.unmount.set_sensitive(true);
            }
            None => {
                self.label.set_text("Виртуальный диск: демон недоступен");
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
            "Подключение виртуального диска…"
        } else {
            "Отключение виртуального диска…"
        });
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = match mount_path {
                Some(path) => mount_virtual_drive(&path).map(|_| "Виртуальный диск подключён"),
                None => unmount_virtual_drive().map(|()| "Виртуальный диск отключён"),
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
                    status.set_text("Не удалось выполнить операцию с виртуальным диском");
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
                Err(error) => status.set_text(&format!("Виртуальный диск: {error}")),
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
                        "Операция подтверждена и остаётся в очереди"
                    } else {
                        "Операция отклонена"
                    });
                    if rows.first_child().is_none() {
                        rows.append(&gtk::Label::new(Some("Нет операций на рассмотрении")));
                    }
                    gtk::glib::ControlFlow::Break
                }
                Ok(Err(error)) => {
                    status.set_text(&format!("Не удалось сохранить решение: {error}"));
                    for control in &controls {
                        control.set_sensitive(true);
                    }
                    gtk::glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    status.set_text("Не удалось сохранить решение");
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
    let hint = gtk::Label::new(Some(
        "Подтверждение сохраняет решение. Выполнение операции запускается отдельно.",
    ));
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    content.append(&hint);
    let rows = gtk::Box::new(gtk::Orientation::Vertical, 12);
    if operations.is_empty() {
        rows.append(&gtk::Label::new(Some("Нет операций на рассмотрении")));
    }
    for operation in operations {
        let approvable = review_approvable(&operation);
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let title = gtk::Label::new(Some(&review_title(&operation)));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_selectable(true);
        row.append(&title);
        let details = gtk::Label::new(Some(&review_details(&operation)));
        details.set_xalign(0.0);
        details.set_wrap(true);
        details.set_selectable(true);
        row.append(&details);
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let mut controls = Vec::new();
        if approvable {
            let approve = gtk::Button::with_label("Подтвердить");
            buttons.append(&approve);
            controls.push(approve);
        } else {
            let conflict = gtk::Label::new(Some("Нужно отдельное разрешение конфликта"));
            conflict.set_xalign(0.0);
            buttons.append(&conflict);
        }
        let dismiss = gtk::Button::with_label(if approvable {
            "Отклонить"
        } else {
            "Скрыть"
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
        .title("Операции на рассмотрении")
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
    let hint = gtk::Label::new(Some(
        "Отключите виртуальный диск перед отменой. После отмены обновите каталог и подключите диск снова: будет показано текущее состояние OneDrive.",
    ));
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
            "удаление".to_owned()
        } else {
            let target = mutation.kind["rename"]["target_name"]
                .as_str()
                .unwrap_or("?");
            format!("переименование в {target}")
        };
        let title = gtk::Label::new(Some(&format!("{} — {}", mutation.name, action)));
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_wrap(true);
        row.append(&title);
        let cancel = gtk::Button::with_label("Отменить операцию");
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
                        result.set_text("Конфликтная операция отменена. Обновите каталог перед повторным подключением.");
                        controls.refresh();
                    }
                    Ok(Err(error)) => {
                        result.set_text(&format!("Не удалось отменить операцию: {error}"));
                        button.set_sensitive(true);
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        result.set_text("Демон недоступен");
                        button.set_sensitive(true);
                    }
                }
                gtk::glib::ControlFlow::Break
            });
        });
    }
    if count == 0 {
        rows.append(&gtk::Label::new(Some("Конфликтных операций нет")));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&rows)
        .vexpand(true)
        .hexpand(true)
        .build();
    content.append(&scroll);
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .title("Конфликты виртуального диска")
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
        "Экспорт сохраняет копию без удаления черновика. Удаление черновика стирает только локальные изменения; облачный файл остаётся. Демон автоматически отправляет закрытые файлы при подключённом диске, включая новые файлы в созданных в OneDrive папках. Файл в новой локальной папке ждёт отправки папки. Папка синхронизации должна быть отключена; демон повторно проверит данные в OneDrive."
    } else {
        "Экспорт сохраняет копию без удаления черновика. Удаление черновика стирает только локальные изменения; облачный файл остаётся. Демон пока не предоставляет отправку файлов."
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
        rows.append(&gtk::Label::new(Some("Локальных изменений нет")));
    }
    for draft in drafts {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let name = draft.name.as_deref().unwrap_or(&draft.item_id);
        let title = gtk::Label::new(Some(&format!("{name} ({})", draft.drive_id)));
        title.set_xalign(0.0);
        title.set_wrap(true);
        title.set_selectable(true);
        row.append(&title);
        let state = gtk::Label::new(Some(virtual_draft_status_label(&draft.status)));
        state.set_xalign(0.0);
        row.append(&state);
        if let Some(issue) = &draft.last_upload_issue {
            let http_status = draft
                .last_upload_http_status
                .map(|status| format!(" (HTTP {status})"))
                .unwrap_or_default();
            let warning = gtk::Label::new(Some(&format!(
                "Предыдущая фоновая отправка: {}{http_status}. Черновик сохранён локально.",
                virtual_draft_upload_issue_label(issue),
            )));
            warning.set_xalign(0.0);
            warning.set_wrap(true);
            row.append(&warning);
        }
        let export = gtk::Button::with_label("Сохранить локальную копию…");
        row.append(&export);
        let export_drive_id = draft.drive_id.clone();
        let export_item_id = draft.item_id.clone();
        let export_name = format!("{name}.local-copy");
        let export_parent = parent.clone();
        let export_result = result.clone();
        export.connect_clicked(move |button| {
            button.set_sensitive(false);
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Сохранить локальный черновик");
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
                        result.set_text("Выберите локальный путь для копии");
                        button.set_sensitive(true);
                        return;
                    };
                    result.set_text("Сохранение локальной копии…");
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
                            Ok(Ok(())) => result.set_text("Локальная копия сохранена. Черновик оставлен без изменений."),
                            Ok(Err(error)) => result.set_text(&format!("Не удалось сохранить копию: {error}. Существующий файл не перезаписывается.")),
                            Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => result.set_text("Не удалось получить результат сохранения"),
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
            let compare = gtk::Button::with_label("Проверить содержимое в OneDrive");
            row.append(&compare);
            let item_id = draft.item_id.clone();
            let result = result.clone();
            let controls = controls.clone();
            let rows = rows.clone();
            let row = row.clone();
            let state = state.clone();
            compare.connect_clicked(move |button| {
                button.set_sensitive(false);
                result.set_text("Сравнение облачного файла с локальным черновиком…");
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
                            result.set_text("Содержимое совпадает. Облачная копия подтверждена, черновик закрыт.");
                            controls.refresh();
                        }
                        Ok(Ok(outcome)) => {
                            state.set_text(if outcome == "different" {
                                "Облачный файл отличается. Черновик сохранён; сравните копии вручную."
                            } else {
                                "Локальные данные отличаются от отправленного снимка или изменились во время проверки. Черновик сохранён."
                            });
                            result.set_text("Черновик оставлен без изменений.");
                            button.set_sensitive(true);
                            controls.refresh();
                        }
                        Ok(Err(error)) => {
                            result.set_text(&format!("Не удалось сравнить файлы: {error}"));
                            button.set_sensitive(true);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            result.set_text("Не удалось получить результат сравнения");
                            button.set_sensitive(true);
                        }
                    }
                    gtk::glib::ControlFlow::Break
                });
            });
        }
        let discard = gtk::Button::with_label("Удалить черновик…");
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
                .message(format!("Удалить локальный черновик «{discard_name}»?"))
                .detail("Локальные изменения будут удалены без возможности восстановления. Файл в OneDrive не удаляется. Если результат прежней отправки неизвестен, облачная копия уже может существовать. При необходимости сначала сохраните локальную копию.")
                .build();
            confirmation.set_buttons(&["Отмена", "Удалить черновик"]);
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
                result.set_text("Удаление локального черновика…");
                let (sender, receiver) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(discard_virtual_draft(&drive_id, &item_id));
                });
                gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                    match receiver.try_recv() {
                        Ok(Ok(())) => {
                            rows.remove(&row);
                            result.set_text("Локальный черновик удалён. Облачный файл не изменён.");
                            controls.refresh();
                        }
                        Ok(Err(error)) => {
                            result.set_text(&format!("Не удалось удалить черновик: {error}"));
                            button.set_sensitive(true);
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            result.set_text("Не удалось получить результат удаления");
                            button.set_sensitive(true);
                        }
                    }
                    gtk::glib::ControlFlow::Break
                });
            });
        });
        if upload_enabled && matches!(draft.status.as_str(), "ready" | "ready-new") {
            let send = gtk::Button::with_label("Отправить в OneDrive");
            row.append(&send);
            let item_id = draft.item_id;
            let result = result.clone();
            let controls = controls.clone();
            let rows = rows.clone();
            let row = row.clone();
            let state = state.clone();
            send.connect_clicked(move |button| {
                button.set_sensitive(false);
                result.set_text("Отправка черновика…");
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
                                result.set_text("Черновик отправлен.");
                            } else {
                                state.set_text("Файл отправлен, но появились новые локальные правки. Откройте список заново для проверки.");
                                result.set_text("Новые локальные правки сохранены.");
                            }
                            controls.refresh();
                        }
                        Ok(Err(error)) => {
                            result.set_text(&format!("Не удалось отправить черновик: {error}"));
                            button.set_sensitive(true);
                            controls.refresh();
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => {
                            return gtk::glib::ControlFlow::Continue;
                        }
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            result.set_text("Не удалось получить результат отправки");
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
        .title("Локальные изменения виртуального диска")
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
        result.set_text("Выберите хотя бы один файл или папку с файлами");
        return;
    }
    pin_button.set_sensitive(false);
    unpin_button.set_sensitive(false);
    result.set_text(&format!("Обработка файлов: {}…", ids.len()));
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
                    "Закреплено"
                } else {
                    "Откреплено"
                };
                let error =
                    first_error.map_or(String::new(), |error| format!(". Первая ошибка: {error}"));
                result.set_text(&format!("{action}: {succeeded}; ошибок: {failed}{error}"));
                pin_button.set_sensitive(true);
                unpin_button.set_sensitive(true);
                cache.refresh();
                gtk::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                result.set_text("Не удалось завершить обработку файлов");
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
    body.append(&gtk::Label::builder().label("Отметьте несколько файлов или папок. Выбранная папка включает все вложенные файлы. Дерево раскрывается стрелкой.").xalign(0.0).wrap(true).build());
    body.append(
        &gtk::ScrolledWindow::builder()
            .child(&tree)
            .vexpand(true)
            .build(),
    );
    let result = gtk::Label::new(Some(
        "Выбранные файлы сохраняются в кэше для работы без сети",
    ));
    result.set_xalign(0.0);
    result.set_wrap(true);
    body.append(&result);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let pin = gtk::Button::with_label("Сохранить офлайн");
    let unpin = gtk::Button::with_label("Снять закрепление");
    actions.append(&pin);
    actions.append(&unpin);
    body.append(&actions);
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Офлайн-доступ к файлам")
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
    let app = adw::Application::builder()
        .application_id("io.github.onedrive_linux.Desktop")
        .build();
    let show_action = gio::SimpleAction::new("show", None);
    let app_for_show = app.downgrade();
    show_action.connect_activate(move |_, _| {
        if let Some(app) = app_for_show.upgrade() {
            app.activate();
        }
    });
    app.add_action(&show_action);
    app.connect_activate(|app| {
        if let Some(window) = app.windows().into_iter().next() {
            window.present();
            return;
        }
        let engine_status = gtk::Label::new(None);
        engine_status.set_xalign(0.0);
        let monitor = StateMonitor::new(app, &engine_status, read_status());
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        let login = gtk::Button::with_label("Войти через Microsoft");
        let status_after_login = status.clone();
        login.connect_clicked(move |_| {
            let result: zbus::Result<String> = daemon_call("BeginLogin");
            match result {
                Ok(url) => {
                    if gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>)
                        .is_err()
                    {
                        status_after_login.set_text("Не удалось открыть браузер");
                    }
                }
                Err(error) => status_after_login.set_text(&login_error_text(&error.to_string())),
            }
        });
        let scan = gtk::Button::with_label("Проверить OneDrive");
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
                    Ok(Ok(())) => status.set_text("Проверка каталога запущена"),
                    Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        status.set_text("Не удалось запустить проверку каталога");
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                }
                button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            });
        });
        let reload = gtk::Button::with_label("Обновить список");
        let pause = gtk::Button::with_label("Пауза недоступна");
        pause.set_sensitive(false);
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
        let download = gtk::Button::with_label("Скачать выбранный файл");
        let choose_sync = gtk::Button::with_label("Папка синхронизации");
        let preview_sync = gtk::Button::with_label("План синхронизации");
        let review = gtk::Button::with_label("Рассмотреть изменения");
        let history = gtk::Button::with_label("Очередь и история");
        let mount = gtk::Button::with_label("Подключить виртуальный диск");
        let unmount = gtk::Button::with_label("Отключить виртуальный диск");
        let show_drafts = gtk::Button::with_label("Все локальные изменения…");
        let show_mutation_conflicts = gtk::Button::with_label("Конфликты виртуального диска…");
        let confirm_deletes = gtk::Button::with_label("Подтвердить массовое удаление…");
        mount.set_sensitive(false);
        unmount.set_sensitive(false);
        let mount_label = gtk::Label::new(Some("Виртуальный диск: проверка состояния…"));
        mount_label.set_xalign(0.0);
        mount_label.set_wrap(true);
        mount_label.set_selectable(true);
        let fuse_label = gtk::Label::new(Some("FUSE: проверка состояния…"));
        fuse_label.set_xalign(0.0);
        fuse_label.set_wrap(true);
        let drafts_label = gtk::Label::new(Some("Локальные изменения: проверка состояния…"));
        drafts_label.set_xalign(0.0);
        drafts_label.set_wrap(true);
        let mount_controls =
            VirtualMountControls::new(&mount_label, &fuse_label, &drafts_label, &mount, &unmount);
        let cache_label = gtk::Label::new(Some("Кэш: проверка состояния…"));
        cache_label.set_xalign(0.0);
        let cache_size = gtk::SpinButton::with_range(0.25, 64.0, 0.25);
        cache_size.set_digits(2);
        cache_size.set_numeric(true);
        let cache_apply = gtk::Button::with_label("Сохранить лимит кэша");
        cache_apply.set_sensitive(false);
        let cache_controls = VirtualCacheControls::new(&cache_label, &cache_size, &cache_apply);
        let cache_clear = gtk::Button::with_label("Очистить кэш");
        cache_clear.set_tooltip_text(Some("Удалить временные копии; офлайн-файлы и локальные черновики сохранятся"));
        let exit = gtk::Button::with_label("Выход");
        let account_menu = gio::Menu::new();
        account_menu.append(Some("Войти через Microsoft"), Some("app.login"));
        account_menu.append(Some("Выход"), Some("app.quit"));
        let sync_menu = gio::Menu::new();
        for (label, action) in [
            ("Проверить OneDrive", "app.refresh"),
            ("Обновить список", "app.reload"),
            ("Скачать выбранный файл", "app.download"),
            ("Папка синхронизации", "app.choose-sync"),
            ("Открыть папку синхронизации", "app.open-sync-folder"),
            ("План синхронизации", "app.preview-sync"),
            ("Рассмотреть изменения", "app.review"),
            ("Очередь и история", "app.history"),
            ("Приостановить / возобновить", "app.toggle-pause"),
        ] {
            sync_menu.append(Some(label), Some(action));
        }
        let virtual_menu = gio::Menu::new();
        for (label, action) in [
            ("Подключить диск", "app.mount"),
            ("Отключить диск", "app.unmount"),
            ("Локальные изменения", "app.show-drafts"),
            ("Конфликты", "app.mutation-conflicts"),
            ("Подтвердить массовое удаление", "app.confirm-deletes"),
            ("Выбрать файлы для офлайн-доступа", "app.offline-files"),
            ("Сохранить лимит кэша", "app.cache-limit"),
        ] {
            virtual_menu.append(Some(label), Some(action));
        }
        let menu = gio::Menu::new();
        menu.append_submenu(Some("Аккаунт"), &account_menu);
        menu.append_submenu(Some("Синхронизация"), &sync_menu);
        menu.append_submenu(Some("Виртуальный диск"), &virtual_menu);
        let menu_bar = gtk::PopoverMenuBar::from_model(Some(&menu));
        let account_status = gtk::Box::new(gtk::Orientation::Vertical, 6);
        account_status.set_margin_start(12);
        account_status.set_margin_end(12);
        account_status.set_margin_top(12);
        account_status.set_margin_bottom(12);
        account_status.append(&gtk::Label::builder().label("Состояние OneDrive").xalign(0.0).css_classes(["heading"]).build());
        account_status.append(&engine_status);
        account_status.append(&gtk::Label::builder().label("Последнее действие").xalign(0.0).css_classes(["heading"]).build());
        account_status.append(&status);
        let account_frame = gtk::Frame::new(None);
        account_frame.set_child(Some(&account_status));
        let virtual_status = gtk::Box::new(gtk::Orientation::Vertical, 6);
        virtual_status.set_margin_start(12);
        virtual_status.set_margin_end(12);
        virtual_status.set_margin_top(12);
        virtual_status.set_margin_bottom(12);
        virtual_status.append(&gtk::Label::builder().label("Виртуальный диск").xalign(0.0).css_classes(["heading"]).build());
        virtual_status.append(&mount_label);
        virtual_status.append(&fuse_label);
        virtual_status.append(&gtk::Label::builder().label("Локальные изменения и операции").xalign(0.0).css_classes(["heading"]).build());
        virtual_status.append(&drafts_label);
        let virtual_frame = gtk::Frame::new(None);
        virtual_frame.set_child(Some(&virtual_status));
        let cache_controls_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        cache_controls_box.set_margin_start(12);
        cache_controls_box.set_margin_end(12);
        cache_controls_box.set_margin_top(12);
        cache_controls_box.set_margin_bottom(12);
        cache_controls_box.append(&gtk::Label::builder().label("Офлайн-доступ и кэш").xalign(0.0).css_classes(["heading"]).build());
        cache_controls_box.append(&gtk::Label::builder().label("Закреплённые файлы остаются доступными без сети. Выбор файлов и папок — в меню «Виртуальный диск».").xalign(0.0).wrap(true).build());
        cache_controls_box.append(&cache_label);
        let cache_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        cache_actions.append(&gtk::Label::new(Some("Лимит кэша (ГиБ):")));
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
        content.append(&menu_bar);
        content.append(&account_frame);
        content.append(&virtual_frame);
        content.append(&cache_frame);
        content.append(&gtk::Label::builder().label("Файлы OneDrive").xalign(0.0).css_classes(["heading"]).build());
        content.append(&list);
        let title = app_title();
        let header = adw::HeaderBar::new();
        let title_label = gtk::Label::new(Some(&title));
        title_label.add_css_class("title");
        header.set_title_widget(Some(&title_label));
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.append(&header);
        layout.append(&gtk::ScrolledWindow::builder().child(&content).vexpand(true).build());
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(&title)
            .default_width(850)
            .default_height(540)
            .content(&layout)
            .build();
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
        let window_for_download = window.clone();
        download.connect_clicked(move |_| {
            let Some(row) = list_for_download.selected_row() else {
                status_for_download.set_text("Сначала выберите файл");
                return;
            };
            let item_id = row.widget_name().to_string();
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Сохранить файл OneDrive");
            dialog.set_initial_name(Some(row.tooltip_text().as_deref().unwrap_or("download")));
            let status_after_dialog = status_for_download.clone();
            dialog.save(
                Some(&window_for_download),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(file) = result else { return };
                    let Some(path) = file.path() else {
                        status_after_dialog.set_text("Выберите локальный путь");
                        return;
                    };
                    let destination = path.to_string_lossy();
                    status_after_dialog.set_text(
                        if download_item(&item_id, &destination).is_ok() {
                            "Скачивание запущено"
                        } else {
                            "Не удалось начать скачивание"
                        },
                    );
                },
            );
        });
        let window_for_folder = window.clone();
        let status_for_folder = status.clone();
        let monitor_for_folder = monitor.clone();
        choose_sync.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Выберите папку синхронизации");
            let status_after_dialog = status_for_folder.clone();
            let monitor_after_dialog = monitor_for_folder.clone();
            dialog.select_folder(
                Some(&window_for_folder),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(folder) = result else { return };
                    let Some(path) = folder.path() else {
                        status_after_dialog.set_text("Нужна локальная папка");
                        return;
                    };
                    match set_sync_root(&path.to_string_lossy()) {
                        Ok(_) => {
                            monitor_after_dialog.update_root_available(true);
                            status_after_dialog
                                .set_text("Папка сохранена. Проверьте план синхронизации");
                        }
                        Err(_) => {
                            status_after_dialog.set_text("Не удалось сохранить папку синхронизации")
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
            dialog.set_title("Выберите пустую папку для виртуального диска");
            let status = status_for_mount.clone();
            let controls = controls_for_mount.clone();
            dialog.select_folder(
                Some(&window_for_mount),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(folder) = result else { return };
                    let Some(path) = folder.path() else {
                        status.set_text("Выберите локальную папку для виртуального диска");
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
            status_for_drafts.set_text("Загрузка локальных изменений…");
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
                        status.set_text("Не удалось загрузить локальные изменения");
                        button.set_sensitive(true);
                        return gtk::glib::ControlFlow::Break;
                    }
                };
                match response {
                    (Ok(drafts), Ok(enabled)) => {
                        present_virtual_drafts_dialog(&parent, &controls, drafts, enabled);
                        status.set_text("Локальные изменения загружены");
                    }
                    (Err(error), _) | (_, Err(error)) => {
                        status.set_text(&format!(
                            "Не удалось загрузить локальные изменения: {error}"
                        ));
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
                    Ok(Err(error)) => status.set_text(&format!("Не удалось загрузить конфликты: {error}")),
                    Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => status.set_text("Демон недоступен"),
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
                            .message(format!("Удалить {count} объектов из OneDrive?"))
                            .detail("Будут выполнены только операции, прошедшие повторную проверку версии и расположения в облаке.")
                            .build();
                        dialog.set_buttons(&["Отмена", "Подтвердить удаление"]);
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
                                    Ok(Ok(())) => { status.set_text("Массовое удаление подтверждено"); controls.refresh(); },
                                    Ok(Err(error)) => status.set_text(&format!("Не удалось подтвердить удаление: {error}")),
                                    Err(std::sync::mpsc::TryRecvError::Empty) => return gtk::glib::ControlFlow::Continue,
                                    Err(std::sync::mpsc::TryRecvError::Disconnected) => status.set_text("Не удалось подтвердить удаление"),
                                }
                                gtk::glib::ControlFlow::Break
                            });
                        });
                    }
                    Ok(_) => status.set_text("Массового удаления нет"),
                    Err(error) => status.set_text(&format!("Не удалось проверить очередь: {error}")),
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
            status_for_preview.set_text("Сканирование локальной папки…");
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
                        status.set_text("План готов. Изменения пока не выполняются");
                        let text = gtk::TextView::new();
                        text.set_editable(false);
                        text.set_monospace(true);
                        text.buffer().set_text(&preview_text(preview));
                        let scroll = gtk::ScrolledWindow::builder()
                            .child(&text)
                            .vexpand(true)
                            .hexpand(true)
                            .build();
                        let dialog = gtk::Window::builder()
                            .transient_for(&parent)
                            .title("Предварительный план синхронизации")
                            .default_width(650)
                            .default_height(450)
                            .child(&scroll)
                            .build();
                        dialog.present();
                    }
                    Err(error) => status.set_text(&format!("Не удалось построить план: {error}")),
                }
                gtk::glib::ControlFlow::Break
            });
        });
        let window_for_review = window.clone();
        let status_for_review = status.clone();
        review.connect_clicked(move |button| {
            button.set_sensitive(false);
            status_for_review.set_text("Проверка изменений…");
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
                            Some(error) => status.set_text(&format!(
                                "Не удалось обновить план; показана сохранённая очередь: {error}"
                            )),
                            None => status.set_text(&format!(
                                "Операций на рассмотрении: {}",
                                operations.len()
                            )),
                        }
                        present_review_dialog(&parent, &status, operations);
                    }
                    Ok(Err(error)) => {
                        status.set_text(&format!("Не удалось прочитать очередь: {error}"));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        status.set_text("Не удалось прочитать очередь");
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
                        text.buffer().set_text(&history_text(history));
                        let scroll = gtk::ScrolledWindow::builder()
                            .child(&text)
                            .vexpand(true)
                            .hexpand(true)
                            .build();
                        let dialog = gtk::Window::builder()
                            .transient_for(&parent)
                            .title("Очередь и история")
                            .default_width(750)
                            .default_height(500)
                            .child(&scroll)
                            .build();
                        dialog.present();
                    }
                    Ok(Err(error)) => {
                        status.set_text(&format!("Не удалось прочитать очередь: {error}"));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        status.set_text("Не удалось прочитать очередь");
                    }
                }
                button.set_sensitive(true);
                gtk::glib::ControlFlow::Break
            });
        });
        let status_for_pause = status.clone();
        let monitor_for_pause = monitor.clone();
        pause.connect_clicked(move |button| {
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
                        monitor.update_paused(Some(paused));
                        status.set_text(if paused {
                            "Синхронизация приостановлена; текущая операция может завершиться"
                        } else {
                            "Синхронизация возобновлена"
                        });
                    }
                    Ok(Err(error)) => {
                        button.set_sensitive(true);
                        status.set_text(&format!("Не удалось изменить паузу: {error}"));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        return gtk::glib::ControlFlow::Continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        button.set_sensitive(true);
                        status.set_text("Не удалось изменить паузу");
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
                        Err("Не удалось прочитать папку синхронизации".to_owned())
                    }
                };
                let message = match result {
                    Ok(Some(root)) => {
                        let uri = gio::File::for_path(&root).uri();
                        gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)
                            .err()
                            .map(|_| "Не удалось открыть папку синхронизации".to_owned())
                    }
                    Ok(None) => Some("Сначала выберите папку синхронизации".to_owned()),
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
                status.set_text("Загрузка дерева файлов для офлайн-доступа…");
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
                            status.set_text(&format!("Не удалось открыть список файлов: {error}"));
                        }
                        gtk::glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => gtk::glib::ControlFlow::Continue,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        if let Some(status) = status.upgrade() {
                            status.set_text("Не удалось открыть список файлов");
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
            "Фоновый процесс недоступен"
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
        assert!(state_label("daemon-outdated").contains("старый фоновый процесс"));
    }

    #[test]
    fn fuse_diagnostics_identifies_missing_device_and_helper() {
        assert!(fuse_diagnostics_label("device-missing", true).contains("/dev/fuse"));
        assert!(fuse_diagnostics_label("device-permission", true).contains("нет доступа"));
        assert!(fuse_diagnostics_label("ready", false).contains("fusermount3"));
    }
}
