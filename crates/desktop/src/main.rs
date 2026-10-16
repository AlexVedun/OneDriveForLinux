mod tray;

use adw::{gio, gtk, prelude::*};
use onedrive_core::{BUS_NAME, INTERFACE_NAME, OBJECT_PATH};
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

fn read_status() -> String {
    daemon_call("Status").unwrap_or_else(|_| "daemon-unavailable".to_owned())
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

fn export_virtual_draft(drive_id: &str, item_id: &str, destination: &str) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)
        .map_err(|error| error.to_string())?;
    proxy
        .call("ExportVirtualDraft", &(drive_id, item_id, destination))
        .map_err(|error| error.to_string())
}

fn virtual_draft_status_label(status: &str) -> &'static str {
    match status {
        "ready" => "версия в каталоге совпадает",
        "other-drive" => "другая учётная запись или диск",
        "catalog-incomplete" => "каталог ещё не сверён",
        "remote-missing" => "файл отсутствует в каталоге",
        "remote-not-file" => "объект больше не является файлом",
        "remote-changed" => "версия в каталоге изменилась",
        _ => "неизвестное состояние",
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
    let mut lines = vec![format!(
        "Локально сохранено: {} · требуют внимания: {attention}. Автоматическая отправка в OneDrive ещё не включена.",
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
            "{name} ({}) — {}",
            draft.drive_id,
            virtual_draft_status_label(&draft.status)
        ));
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

fn virtual_mount_paths() -> zbus::Result<(String, String)> {
    let current = read_virtual_mount_path()?;
    let recovery = read_virtual_mount_recovery_path()?;
    Ok((current, recovery))
}

fn mount_virtual_drive(path: &str) -> zbus::Result<String> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("MountVirtualDrive", &(path))
}

fn unmount_virtual_drive() -> zbus::Result<()> {
    daemon_call("UnmountVirtualDrive")
}

fn read_virtual_file_cache_status(item_id: &str) -> zbus::Result<String> {
    let connection = zbus::blocking::Connection::session()?;
    let proxy = zbus::blocking::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE_NAME)?;
    proxy.call("VirtualFileCacheStatus", &(item_id))
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
}

#[derive(Clone)]
struct OfflineControls {
    label: gtk::Label,
    button: gtk::Button,
    selected: Rc<RefCell<Option<String>>>,
    state: Rc<RefCell<Option<String>>>,
    generation: Rc<Cell<u64>>,
    busy: Rc<Cell<bool>>,
}

impl OfflineControls {
    fn new(label: &gtk::Label, button: &gtk::Button) -> Self {
        Self {
            label: label.clone(),
            button: button.clone(),
            selected: Rc::new(RefCell::new(None)),
            state: Rc::new(RefCell::new(None)),
            generation: Rc::new(Cell::new(0)),
            busy: Rc::new(Cell::new(false)),
        }
    }

    fn show_state(&self, state: &str) {
        self.state.replace(Some(state.to_owned()));
        self.label.set_text(match state {
            "missing" => "Выбранный файл: ещё не сохранён локально",
            "cached" => "Выбранный файл: в кэше, может быть удалён при очистке",
            "pinned" => "Выбранный файл: доступен офлайн",
            _ => "Не удалось определить состояние файла",
        });
        self.button.set_label(if state == "pinned" {
            "Снять закрепление"
        } else {
            "Сохранить выбранный файл офлайн"
        });
        self.button
            .set_sensitive(matches!(state, "missing" | "cached" | "pinned"));
    }

    fn set_selected(&self, item_id: Option<String>) {
        self.selected.replace(item_id);
        self.state.replace(None);
        self.generation.set(self.generation.get().wrapping_add(1));
        self.button.set_sensitive(false);
        self.label.set_text(if self.selected.borrow().is_some() {
            "Выбранный файл: проверка кэша…"
        } else {
            "Выберите файл для доступа офлайн"
        });
        self.refresh();
    }

    fn refresh(&self) {
        if self.busy.get() {
            return;
        }
        let Some(item_id) = self.selected.borrow().clone() else {
            return;
        };
        let generation = self.generation.get();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(read_virtual_file_cache_status(&item_id));
        });
        let controls = self.clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    return gtk::glib::ControlFlow::Continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if controls.generation.get() == generation {
                        controls.label.set_text("Не удалось проверить кэш");
                    }
                    return gtk::glib::ControlFlow::Break;
                }
            };
            if controls.generation.get() == generation {
                match result {
                    Ok(state) => controls.show_state(&state),
                    Err(error) => controls.label.set_text(&format!("Кэш недоступен: {error}")),
                }
            }
            gtk::glib::ControlFlow::Break
        });
    }

    fn toggle(&self, status: &gtk::Label) {
        if self.busy.replace(true) {
            return;
        }
        let Some(item_id) = self.selected.borrow().clone() else {
            self.busy.set(false);
            return;
        };
        let pinned = self.state.borrow().as_deref() != Some("pinned");
        self.generation.set(self.generation.get().wrapping_add(1));
        self.button.set_sensitive(false);
        status.set_text(if pinned {
            "Сохранение файла для доступа офлайн…"
        } else {
            "Снятие закрепления файла…"
        });
        let selected_id = item_id.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(set_virtual_file_pinned(&item_id, pinned));
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
                    controls.busy.set(false);
                    controls.refresh();
                    status.set_text("Не удалось изменить доступность файла офлайн");
                    return gtk::glib::ControlFlow::Break;
                }
            };
            controls.busy.set(false);
            match result {
                Ok(state) => {
                    if controls.selected.borrow().as_deref() == Some(selected_id.as_str()) {
                        controls.show_state(&state);
                    } else {
                        controls.refresh();
                    }
                    status.set_text(if pinned {
                        "Файл сохранён для доступа офлайн"
                    } else {
                        "Закрепление файла снято"
                    });
                }
                Err(error) => {
                    controls.refresh();
                    status.set_text(&format!("Не удалось изменить доступность офлайн: {error}"));
                }
            }
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

    fn show_drafts(&self, result: Result<Vec<UiVirtualDraft>, String>) {
        match result {
            Ok(drafts) => self.drafts.set_text(&virtual_drafts_text(&drafts)),
            Err(_) => self
                .drafts
                .set_text("Не удалось проверить локальные изменения"),
        }
    }

    fn show_path(&self, paths: Option<(&str, &str)>) {
        match paths {
            Some(("", "")) => {
                self.label.set_text("Виртуальный диск не подключён");
                self.mount.set_sensitive(true);
                self.unmount.set_sensitive(false);
            }
            Some(("", recovery)) => {
                self.label.set_text(&format!("Прежняя точка монтирования занята: {recovery}. Проверьте её и отключите через fusermount3 -u"));
                self.mount.set_sensitive(false);
                self.unmount.set_sensitive(false);
            }
            Some((path, _)) => {
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
                    .map(|(current, recovery)| (current.as_str(), recovery.as_str())),
            );
            controls.show_diagnostics(result.1);
            controls.show_drafts(result.2);
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
            let _ = sender.send((result, path, diagnostics, drafts));
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
                    .map(|(current, recovery)| (current.as_str(), recovery.as_str())),
            );
            controls.show_diagnostics(result.2);
            controls.show_drafts(result.3);
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
        "Экспорт сохраняет копию без удаления черновика. Отправка экспериментальная: отключите виртуальный диск и папку синхронизации. Перед отправкой демон повторно проверит версию файла в OneDrive."
    } else {
        "Экспорт сохраняет копию без удаления черновика. Ручная отправка доступна после включения ONEDRIVE_EXPERIMENTAL_VIRTUAL_UPLOAD=1 для демона."
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
        if upload_enabled && draft.status == "ready" {
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
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .build();
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
                Err(_) => status_after_login.set_text("Не удалось начать вход"),
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
        let offline_label = gtk::Label::new(Some("Выберите файл для доступа офлайн"));
        offline_label.set_xalign(0.0);
        offline_label.set_wrap(true);
        let offline_button = gtk::Button::with_label("Сохранить выбранный файл офлайн");
        offline_button.set_sensitive(false);
        let offline_controls = OfflineControls::new(&offline_label, &offline_button);
        let offline_for_selection = offline_controls.clone();
        list.connect_row_selected(move |_, row| {
            offline_for_selection.set_selected(row.map(|row| row.widget_name().to_string()));
        });
        let cache_label = gtk::Label::new(Some("Кэш: проверка состояния…"));
        cache_label.set_xalign(0.0);
        let cache_size = gtk::SpinButton::with_range(0.25, 64.0, 0.25);
        cache_size.set_digits(2);
        cache_size.set_numeric(true);
        let cache_apply = gtk::Button::with_label("Сохранить лимит кэша");
        cache_apply.set_sensitive(false);
        let cache_controls = VirtualCacheControls::new(&cache_label, &cache_size, &cache_apply);
        let exit = gtk::Button::with_label("Выход");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.append(&login);
        controls.append(&scan);
        controls.append(&reload);
        controls.append(&download);
        let sync_controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let sync_setup = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sync_setup.append(&choose_sync);
        sync_setup.append(&preview_sync);
        sync_setup.append(&exit);
        let sync_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sync_actions.append(&review);
        sync_actions.append(&history);
        sync_actions.append(&pause);
        sync_controls.append(&sync_setup);
        sync_controls.append(&sync_actions);
        let virtual_controls = gtk::Box::new(gtk::Orientation::Vertical, 6);
        virtual_controls.append(&mount_label);
        virtual_controls.append(&fuse_label);
        virtual_controls.append(&drafts_label);
        let virtual_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        virtual_actions.append(&mount);
        virtual_actions.append(&unmount);
        virtual_actions.append(&show_drafts);
        virtual_controls.append(&virtual_actions);
        virtual_controls.append(&offline_label);
        virtual_controls.append(&offline_button);
        virtual_controls.append(&cache_label);
        let cache_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        cache_actions.append(&gtk::Label::new(Some("Лимит кэша (ГиБ):")));
        cache_actions.append(&cache_size);
        cache_actions.append(&cache_apply);
        virtual_controls.append(&cache_actions);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.append(&engine_status);
        content.append(&status);
        content.append(&controls);
        content.append(&sync_controls);
        content.append(&virtual_controls);
        content.append(&scroll);
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("OneDrive для Linux")
            .default_width(850)
            .default_height(540)
            .content(&content)
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
        let status_for_offline = status.clone();
        offline_button.connect_clicked(move |_| {
            offline_controls.toggle(&status_for_offline);
        });
        let status_for_cache = status.clone();
        let cache_for_apply = cache_controls.clone();
        cache_apply.connect_clicked(move |_| {
            cache_for_apply.apply(&status_for_cache);
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
            ("history", &history),
            ("toggle-pause", &pause),
            ("refresh", &scan),
        ] {
            let action = gio::SimpleAction::new(name, None);
            let button = button.downgrade();
            action.connect_activate(move |_, _| {
                if let Some(button) = button.upgrade() {
                    button.emit_clicked();
                }
            });
            app.add_action(&action);
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
    fn virtual_drafts_show_conflicts_and_manual_upload_summary() {
        let drafts: Vec<UiVirtualDraft> = serde_json::from_str(
            r#"[
              {"drive_id":"drive","item_id":"1","name":"note.txt","status":"ready"},
              {"drive_id":"drive","item_id":"2","name":null,"status":"remote-changed"}
            ]"#,
        )
        .unwrap();
        let text = virtual_drafts_text(&drafts);
        assert!(text.contains("Локально сохранено: 2 · требуют внимания: 1"));
        assert!(text.contains("note.txt (drive) — версия в каталоге совпадает"));
        assert!(text.contains("2 (drive) — версия в каталоге изменилась"));
        assert!(text.contains("Автоматическая отправка в OneDrive ещё не включена"));
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
    fn fuse_diagnostics_identifies_missing_device_and_helper() {
        assert!(fuse_diagnostics_label("device-missing", true).contains("/dev/fuse"));
        assert!(fuse_diagnostics_label("device-permission", true).contains("нет доступа"));
        assert!(fuse_diagnostics_label("ready", false).contains("fusermount3"));
    }
}
