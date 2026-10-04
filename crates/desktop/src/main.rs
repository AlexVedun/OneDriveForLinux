use adw::{gio, gtk, prelude::*};
use onedrive_core::{BUS_NAME, INTERFACE_NAME, OBJECT_PATH};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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

fn read_sync_preview() -> Result<UiPreview, String> {
    let json: String = daemon_call("PreviewSync").map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

fn read_review_operations() -> Result<Vec<UiReviewOperation>, String> {
    let json: String = daemon_call("ReviewOperations").map_err(|error| error.to_string())?;
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

fn refresh_view(status: &gtk::Label, list: &gtk::ListBox) {
    status.set_text(&format!("Состояние: {}", read_status()));
    match read_catalog() {
        Ok(items) => populate_list(list, items),
        Err(_) => status.set_text("Не удалось прочитать каталог"),
    }
}

fn main() {
    let app = adw::Application::builder()
        .application_id("io.github.onedrive_linux.Desktop")
        .build();
    app.connect_activate(|app| {
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
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
        scan.connect_clicked(move |_| {
            let result: zbus::Result<()> = daemon_call("RefreshCatalog");
            status_after_scan.set_text(if result.is_ok() {
                "Проверка каталога запущена"
            } else {
                "Не удалось запустить проверку каталога"
            });
        });
        let reload = gtk::Button::with_label("Обновить список");
        let status_for_reload = status.clone();
        let list_for_reload = list.clone();
        reload.connect_clicked(move |_| refresh_view(&status_for_reload, &list_for_reload));
        let download = gtk::Button::with_label("Скачать выбранный файл");
        let choose_sync = gtk::Button::with_label("Папка синхронизации");
        let preview_sync = gtk::Button::with_label("План синхронизации");
        let review = gtk::Button::with_label("Рассмотреть изменения");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        controls.append(&login);
        controls.append(&scan);
        controls.append(&reload);
        controls.append(&download);
        let sync_controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sync_controls.append(&choose_sync);
        sync_controls.append(&preview_sync);
        sync_controls.append(&review);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.append(&status);
        content.append(&controls);
        content.append(&sync_controls);
        content.append(&scroll);
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("OneDrive для Linux")
            .default_width(850)
            .default_height(540)
            .content(&content)
            .build();
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
        choose_sync.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::new();
            dialog.set_title("Выберите папку синхронизации");
            let status_after_dialog = status_for_folder.clone();
            dialog.select_folder(
                Some(&window_for_folder),
                None::<&gio::Cancellable>,
                move |result| {
                    let Ok(folder) = result else { return };
                    let Some(path) = folder.path() else {
                        status_after_dialog.set_text("Нужна локальная папка");
                        return;
                    };
                    status_after_dialog.set_text(
                        if set_sync_root(&path.to_string_lossy()).is_ok() {
                            "Папка сохранена. Проверьте план синхронизации"
                        } else {
                            "Не удалось сохранить папку синхронизации"
                        },
                    );
                },
            );
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
        refresh_view(&status, &list);
        window.present();
    });
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
