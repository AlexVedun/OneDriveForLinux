use adw::{gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

const OBJECT_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/Menu";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const INTERFACES: [(&str, &str); 2] = [
    (
        "org.kde.StatusNotifierItem",
        "org.kde.StatusNotifierWatcher",
    ),
    (
        "org.freedesktop.StatusNotifierItem",
        "org.freedesktop.StatusNotifierWatcher",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Show,
    OpenFolder,
    History,
    TogglePause,
    Refresh,
    Quit,
}

const MENU_ITEMS: [(i32, &str, MenuAction); 6] = [
    (1, "Открыть окно", MenuAction::Show),
    (6, "Открыть папку синхронизации", MenuAction::OpenFolder),
    (2, "Очередь и история", MenuAction::History),
    (3, "Приостановить синхронизацию", MenuAction::TogglePause),
    (4, "Проверить OneDrive", MenuAction::Refresh),
    (5, "Выход", MenuAction::Quit),
];

#[derive(Clone, Copy, PartialEq, Eq)]
struct MenuState {
    paused: Option<bool>,
    root_available: bool,
    outstanding: Option<u64>,
}

const MENU_XML: &str = r#"<node><interface name="com.canonical.dbusmenu">
    <method name="GetLayout">
        <arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/>
        <arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
        <arg type="ai" direction="in"/><arg type="as" direction="in"/>
        <arg type="a(ia{sv})" direction="out"/>
    </method>
    <method name="GetProperty">
        <arg type="i" direction="in"/><arg type="s" direction="in"/>
        <arg type="v" direction="out"/>
    </method>
    <method name="Event">
        <arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/>
    </method>
    <method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
    <property name="Version" type="u" access="read"/>
    <property name="Status" type="s" access="read"/>
    <signal name="ItemsPropertiesUpdated"><arg type="a(ia{sv})"/><arg type="a(ias)"/></signal>
    <signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal>
</interface></node>"#;

fn menu_action(id: i32) -> Option<MenuAction> {
    MENU_ITEMS
        .iter()
        .find(|item| item.0 == id)
        .map(|item| item.2)
}

fn menu_properties(id: i32, names: &[String], state: MenuState) -> HashMap<String, glib::Variant> {
    let mut properties = HashMap::new();
    let label = if id == 0 {
        None
    } else {
        MENU_ITEMS
            .iter()
            .find(|item| item.0 == id)
            .map(|item| item.1)
    };
    if id == 0 && (names.is_empty() || names.iter().any(|name| name == "children-display")) {
        properties.insert("children-display".to_owned(), "submenu".to_variant());
    }
    if let Some(label) = label
        && (names.is_empty() || names.iter().any(|name| name == "label"))
    {
        let label = if id == 3 && state.paused == Some(true) {
            "Возобновить синхронизацию"
        } else {
            label
        };
        let label = if id == 2 {
            match state.outstanding {
                Some(count) if count > 0 => format!("{label} ({count})"),
                _ => label.to_owned(),
            }
        } else {
            label.to_owned()
        };
        properties.insert("label".to_owned(), label.to_variant());
    }
    if ((id == 3 && state.paused.is_none()) || (id == 6 && !state.root_available))
        && (names.is_empty() || names.iter().any(|name| name == "enabled"))
    {
        properties.insert("enabled".to_owned(), false.to_variant());
    }
    properties
}

fn menu_layout(id: i32, depth: i32, names: &[String], state: MenuState) -> glib::Variant {
    let children: Vec<glib::Variant> = if id == 0 && depth != 0 {
        MENU_ITEMS
            .iter()
            .map(|(child_id, _, _)| menu_layout(*child_id, depth - 1, names, state))
            .collect()
    } else {
        Vec::new()
    };
    glib::Variant::tuple_from_iter([
        id.to_variant(),
        menu_properties(id, names, state).to_variant(),
        children.to_variant(),
    ])
}

fn register_menu(
    connection: &gio::DBusConnection,
    action: Rc<dyn Fn(MenuAction)>,
    state: Rc<Cell<MenuState>>,
) -> Result<gio::RegistrationId, String> {
    let node = gio::DBusNodeInfo::for_xml(MENU_XML).map_err(|error| error.to_string())?;
    let info = node
        .lookup_interface("com.canonical.dbusmenu")
        .ok_or_else(|| "missing DBusMenu interface".to_owned())?;
    connection
        .register_object(MENU_PATH, &info)
        .method_call(
            move |_, _, _, _, method, parameters, invocation| match method {
                "GetLayout" => {
                    if let Some((id, depth, names)) = parameters.get::<(i32, i32, Vec<String>)>()
                        && (id == 0 || menu_action(id).is_some())
                    {
                        let layout = menu_layout(id, depth, &names, state.get());
                        invocation.return_value(Some(&glib::Variant::tuple_from_iter([
                            1_u32.to_variant(),
                            layout,
                        ])));
                    } else {
                        invocation.return_dbus_error(
                            "com.canonical.dbusmenu.Error.InvalidItem",
                            "unknown menu item",
                        );
                    }
                }
                "GetGroupProperties" => {
                    if let Some((ids, names)) = parameters.get::<(Vec<i32>, Vec<String>)>() {
                        let ids = if ids.is_empty() {
                            std::iter::once(0)
                                .chain(MENU_ITEMS.iter().map(|item| item.0))
                                .collect::<Vec<_>>()
                        } else {
                            ids
                        };
                        let properties: Vec<(i32, HashMap<String, glib::Variant>)> = ids
                            .into_iter()
                            .filter(|id| *id == 0 || menu_action(*id).is_some())
                            .map(|id| (id, menu_properties(id, &names, state.get())))
                            .collect();
                        invocation.return_value(Some(&(properties,).to_variant()));
                    } else {
                        invocation.return_dbus_error(
                            "com.canonical.dbusmenu.Error.InvalidArgs",
                            "invalid property request",
                        );
                    }
                }
                "GetProperty" => {
                    if let Some((id, name)) = parameters.get::<(i32, String)>() {
                        if let Some(value) = menu_properties(id, &[], state.get()).get(&name) {
                            invocation.return_value(Some(&(value.clone(),).to_variant()));
                        } else {
                            invocation.return_dbus_error(
                                "com.canonical.dbusmenu.Error.InvalidProperty",
                                "unknown menu property",
                            );
                        }
                    } else {
                        invocation.return_dbus_error(
                            "com.canonical.dbusmenu.Error.InvalidArgs",
                            "invalid property request",
                        );
                    }
                }
                "Event" => {
                    if let Some((id, event, _, _)) =
                        parameters.get::<(i32, String, glib::Variant, u32)>()
                        && event == "clicked"
                        && let Some(item) = menu_action(id)
                    {
                        action(item);
                    }
                    invocation.return_value(Some(&().to_variant()));
                }
                "AboutToShow" => invocation.return_value(Some(&(false,).to_variant())),
                _ => invocation.return_dbus_error(
                    "com.canonical.dbusmenu.Error.UnknownMethod",
                    "unknown menu method",
                ),
            },
        )
        .property(|_, _, _, _, name| match name {
            "Version" => 2_u32.to_variant(),
            "Status" => "normal".to_variant(),
            _ => String::new().to_variant(),
        })
        .build()
        .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Presentation {
    status: &'static str,
    icon: IconKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconKind {
    Attention,
    Offline,
    Paused,
    Transferring,
    Checking,
    Synced,
    Idle,
}

fn presentation(state: &str) -> Presentation {
    match state {
        "needs-login" | "error" | "reconciliation-needed" | "daemon-unavailable" => Presentation {
            status: "NeedsAttention",
            icon: IconKind::Attention,
        },
        "offline" => Presentation {
            status: "NeedsAttention",
            icon: IconKind::Offline,
        },
        "paused" => Presentation {
            status: "Active",
            icon: IconKind::Paused,
        },
        "transferring" => Presentation {
            status: "Active",
            icon: IconKind::Transferring,
        },
        "checking" => Presentation {
            status: "Active",
            icon: IconKind::Checking,
        },
        "synced" => Presentation {
            status: "Active",
            icon: IconKind::Synced,
        },
        _ => Presentation {
            status: "Active",
            icon: IconKind::Idle,
        },
    }
}

fn icon_pixmap(kind: IconKind) -> Vec<(i32, i32, Vec<u8>)> {
    [22, 32]
        .into_iter()
        .map(|size| (size, size, render_icon(kind, size)))
        .collect()
}

fn render_icon(kind: IconKind, size: i32) -> Vec<u8> {
    // Four samples per axis keep the cloud and badge legible in a 22 px panel.
    // StatusNotifierItem expects straight ARGB32 bytes in network byte order.
    let mut pixels = vec![0; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut alpha = 0_u32;
            let mut rgb = [0_u32; 3];
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = (x as f32 + (sx as f32 + 0.5) / 4.0) * 32.0 / size as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / 4.0) * 32.0 / size as f32;
                    if let Some(color) = icon_sample(kind, px, py) {
                        alpha += 1;
                        for channel in 0..3 {
                            rgb[channel] += u32::from(color[channel]);
                        }
                    }
                }
            }
            let offset = ((y * size + x) * 4) as usize;
            pixels[offset] = (alpha * 255 / 16) as u8;
            for channel in 0..3 {
                pixels[offset + channel + 1] = rgb[channel].checked_div(alpha).unwrap_or(0) as u8;
            }
        }
    }
    pixels
}

fn circle(x: f32, y: f32, cx: f32, cy: f32, radius: f32) -> bool {
    (x - cx).powi(2) + (y - cy).powi(2) <= radius.powi(2)
}

fn line(x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32, width: f32) -> bool {
    let dx = bx - ax;
    let dy = by - ay;
    let projection = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    circle(
        x,
        y,
        ax + projection * dx,
        ay + projection * dy,
        width / 2.0,
    )
}

fn badge_symbol(kind: IconKind, x: f32, y: f32) -> bool {
    match kind {
        IconKind::Attention => {
            (23.1..24.9).contains(&x) && (19.6..25.1).contains(&y) || circle(x, y, 24.0, 27.6, 1.0)
        }
        IconKind::Offline => {
            line(x, y, 21.0, 21.0, 27.0, 27.0, 2.0) || line(x, y, 27.0, 21.0, 21.0, 27.0, 2.0)
        }
        IconKind::Paused => {
            ((21.0..23.0).contains(&x) || (25.0..27.0).contains(&x)) && (20.3..27.7).contains(&y)
        }
        IconKind::Transferring => {
            line(x, y, 22.0, 26.8, 22.0, 21.0, 1.7)
                || line(x, y, 22.0, 21.0, 20.1, 23.0, 1.7)
                || line(x, y, 22.0, 21.0, 23.9, 23.0, 1.7)
                || line(x, y, 26.0, 21.2, 26.0, 27.0, 1.7)
                || line(x, y, 26.0, 27.0, 24.1, 25.0, 1.7)
                || line(x, y, 26.0, 27.0, 27.9, 25.0, 1.7)
        }
        IconKind::Checking => {
            let distance = ((x - 23.0).powi(2) + (y - 23.0).powi(2)).sqrt();
            (2.2..3.8).contains(&distance) || line(x, y, 25.2, 25.2, 28.0, 28.0, 1.8)
        }
        IconKind::Synced => {
            line(x, y, 20.5, 24.0, 23.0, 26.5, 2.2) || line(x, y, 23.0, 26.5, 27.7, 21.2, 2.2)
        }
        IconKind::Idle => false,
    }
}

fn icon_sample(kind: IconKind, x: f32, y: f32) -> Option<[u8; 3]> {
    // Rounded blue tile and white cloud follow the packaged application icon.
    let bx = x.clamp(6.0, 26.0);
    let by = y.clamp(6.0, 26.0);
    let mut color = circle(x, y, bx, by, 5.0).then_some([0x17, 0x64, 0xc2]);
    let cloud = (circle(x, y, 11.0, 18.2, 5.0)
        || circle(x, y, 18.0, 14.3, 6.5)
        || circle(x, y, 24.0, 17.8, 4.7)
        || (8.0..27.8).contains(&x) && (17.7..23.0).contains(&y))
        && (8.0..27.8).contains(&x)
        && y < 23.0;
    if cloud {
        color = Some([255, 255, 255]);
    }
    if kind != IconKind::Idle {
        if circle(x, y, 24.0, 24.0, 8.5) {
            color = Some([255, 255, 255]);
        }
        if circle(x, y, 24.0, 24.0, 7.0) {
            color = Some(match kind {
                IconKind::Attention => [0xcc, 0x35, 0x35],
                IconKind::Offline => [0x67, 0x6f, 0x78],
                IconKind::Paused => [0xad, 0x67, 0x09],
                IconKind::Synced => [0x1b, 0x83, 0x4b],
                IconKind::Transferring | IconKind::Checking => [0x17, 0x64, 0xc2],
                IconKind::Idle => unreachable!(),
            });
        }
        if badge_symbol(kind, x, y) {
            color = Some([255, 255, 255]);
        }
    }
    color
}

fn interface_xml(name: &str) -> String {
    format!(
        r#"<node><interface name="{name}">
            <method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
            <method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
            <method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
            <method name="Scroll"><arg name="delta" type="i" direction="in"/><arg name="orientation" type="s" direction="in"/></method>
            <property name="Category" type="s" access="read"/>
            <property name="Id" type="s" access="read"/>
            <property name="Title" type="s" access="read"/>
            <property name="Status" type="s" access="read"/>
            <property name="WindowId" type="u" access="read"/>
            <property name="IconName" type="s" access="read"/>
            <property name="IconPixmap" type="a(iiay)" access="read"/>
            <property name="AttentionIconName" type="s" access="read"/>
            <property name="AttentionIconPixmap" type="a(iiay)" access="read"/>
            <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
            <property name="ItemIsMenu" type="b" access="read"/>
            <property name="Menu" type="o" access="read"/>
            <signal name="NewIcon"/>
            <signal name="NewAttentionIcon"/>
            <signal name="NewToolTip"/>
            <signal name="NewStatus"><arg name="status" type="s"/></signal>
        </interface></node>"#
    )
}

fn property_value(name: &str, state: &str, label: &str) -> glib::Variant {
    match name {
        "Category" => "ApplicationStatus".to_variant(),
        "Id" => "onedrive-linux".to_variant(),
        "Title" => "OneDrive для Linux".to_variant(),
        "Status" => presentation(state).status.to_variant(),
        "WindowId" => 0_u32.to_variant(),
        "IconName" | "AttentionIconName" => "".to_variant(),
        "IconPixmap" => icon_pixmap(presentation(state).icon).to_variant(),
        "AttentionIconPixmap" => icon_pixmap(presentation(state).icon).to_variant(),
        "ToolTip" => (
            "",
            icon_pixmap(presentation(state).icon),
            "OneDrive для Linux",
            label,
        )
            .to_variant(),
        "ItemIsMenu" => false.to_variant(),
        "Menu" => glib::variant::ObjectPath::try_from(MENU_PATH)
            .expect("valid menu path")
            .to_variant(),
        _ => String::new().to_variant(),
    }
}

fn register_with_watcher(connection: &gio::DBusConnection, watcher: &str, service: &str) {
    connection.call(
        Some(watcher),
        WATCHER_PATH,
        watcher,
        "RegisterStatusNotifierItem",
        Some(&(service,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        5_000,
        None::<&gio::Cancellable>,
        |_| {},
    );
}

pub struct TrayIndicator {
    connection: gio::DBusConnection,
    registrations: Vec<gio::RegistrationId>,
    owners: Vec<gio::OwnerId>,
    watcher_cleanup: Vec<Box<dyn FnOnce()>>,
    state: Rc<RefCell<String>>,
    menu_state: Rc<Cell<MenuState>>,
}

impl TrayIndicator {
    pub fn start(
        app: &adw::Application,
        initial: &str,
        action: Rc<dyn Fn(MenuAction)>,
    ) -> Result<Self, String> {
        let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
            .map_err(|error| error.to_string())?;
        let mut tray = Self {
            connection,
            registrations: Vec::new(),
            owners: Vec::new(),
            watcher_cleanup: Vec::new(),
            state: Rc::new(RefCell::new(initial.to_owned())),
            menu_state: Rc::new(Cell::new(MenuState {
                paused: None,
                root_available: false,
                outstanding: None,
            })),
        };
        tray.registrations.push(register_menu(
            &tray.connection,
            action,
            tray.menu_state.clone(),
        )?);
        for (interface, watcher) in INTERFACES {
            tray.register_interface(app, interface, watcher)?;
        }
        Ok(tray)
    }

    fn register_interface(
        &mut self,
        app: &adw::Application,
        interface: &str,
        watcher: &str,
    ) -> Result<(), String> {
        let node = gio::DBusNodeInfo::for_xml(&interface_xml(interface))
            .map_err(|error| error.to_string())?;
        let info = node
            .lookup_interface(interface)
            .ok_or_else(|| "missing status notifier interface".to_owned())?;
        let app_for_activation = app.downgrade();
        let state_for_property = self.state.clone();
        let registration = self
            .connection
            .register_object(OBJECT_PATH, &info)
            .method_call(move |_, _, _, _, method, _, invocation| {
                if matches!(method, "Activate" | "SecondaryActivate" | "ContextMenu")
                    && let Some(app) = app_for_activation.upgrade()
                {
                    app.activate();
                }
                invocation.return_value(Some(&().to_variant()));
            })
            .property(move |_, _, _, _, name| {
                let state = state_for_property.borrow();
                property_value(name, &state, super::state_label(&state))
            })
            .build()
            .map_err(|error| error.to_string())?;
        self.registrations.push(registration);

        let service = format!("{interface}-{}-1", std::process::id());
        let connection = self.connection.clone();
        let watcher_for_acquired = watcher.to_owned();
        let service_for_acquired = service.clone();
        let owner = gio::bus_own_name_on_connection(
            &connection,
            &service,
            gio::BusNameOwnerFlags::NONE,
            move |connection, _| {
                register_with_watcher(&connection, &watcher_for_acquired, &service_for_acquired);
            },
            |_, _| {},
        );
        self.owners.push(owner);
        let service_for_watcher = service.clone();
        let watcher_id = gio::bus_watch_name_on_connection(
            &connection,
            watcher,
            gio::BusNameWatcherFlags::NONE,
            move |connection, watcher, _| {
                register_with_watcher(&connection, watcher, &service_for_watcher);
            },
            |_, _| {},
        );
        self.watcher_cleanup
            .push(Box::new(move || gio::bus_unwatch_name(watcher_id)));
        Ok(())
    }

    pub fn update(&self, current: &str) {
        if self.state.borrow().as_str() == current {
            return;
        }
        self.state.replace(current.to_owned());
        for (interface, _) in INTERFACES {
            let status = presentation(current).status;
            let _ = self.connection.emit_signal(
                None,
                OBJECT_PATH,
                interface,
                "NewStatus",
                Some(&(status,).to_variant()),
            );
            let _ = self
                .connection
                .emit_signal(None, OBJECT_PATH, interface, "NewIcon", None);
            let _ =
                self.connection
                    .emit_signal(None, OBJECT_PATH, interface, "NewAttentionIcon", None);
            let _ = self
                .connection
                .emit_signal(None, OBJECT_PATH, interface, "NewToolTip", None);
        }
    }

    pub fn update_paused(&self, paused: Option<bool>) {
        let mut state = self.menu_state.get();
        if state.paused == paused {
            return;
        }
        state.paused = paused;
        self.menu_state.set(state);
        self.emit_item_properties(3, paused.is_some());
    }

    pub fn update_root_available(&self, available: bool) {
        let mut state = self.menu_state.get();
        if state.root_available == available {
            return;
        }
        state.root_available = available;
        self.menu_state.set(state);
        self.emit_item_properties(6, available);
    }

    pub fn update_outstanding(&self, count: Option<u64>) {
        let mut state = self.menu_state.get();
        if state.outstanding == count {
            return;
        }
        state.outstanding = count;
        self.menu_state.set(state);
        self.emit_item_properties(2, false);
    }

    fn emit_item_properties(&self, id: i32, enabled: bool) {
        let updated = vec![(id, menu_properties(id, &[], self.menu_state.get()))];
        let removed: Vec<(i32, Vec<String>)> = if enabled {
            vec![(id, vec!["enabled".to_owned()])]
        } else {
            Vec::new()
        };
        let _ = self.connection.emit_signal(
            None,
            MENU_PATH,
            "com.canonical.dbusmenu",
            "ItemsPropertiesUpdated",
            Some(&(updated, removed).to_variant()),
        );
    }
}

impl Drop for TrayIndicator {
    fn drop(&mut self) {
        for cleanup in self.watcher_cleanup.drain(..) {
            cleanup();
        }
        for owner in self.owners.drain(..) {
            gio::bus_unown_name(owner);
        }
        for registration in self.registrations.drain(..) {
            let _ = self.connection.unregister_object(registration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_icons_follow_attention_and_transfer_priority() {
        assert_eq!(presentation("error").status, "NeedsAttention");
        assert_eq!(presentation("offline").icon, IconKind::Offline);
        assert_eq!(presentation("paused").icon, IconKind::Paused);
        assert_eq!(presentation("transferring").status, "Active");
        assert_eq!(presentation("checking").icon, IconKind::Checking);
        assert_eq!(presentation("connected").icon, IconKind::Idle);
        assert_eq!(presentation("synced").icon, IconKind::Synced);
    }

    #[test]
    fn status_pixmaps_are_complete_argb_images() {
        let mut images = Vec::new();
        for kind in [
            IconKind::Attention,
            IconKind::Offline,
            IconKind::Paused,
            IconKind::Transferring,
            IconKind::Checking,
            IconKind::Synced,
            IconKind::Idle,
        ] {
            let pixmaps = icon_pixmap(kind);
            assert_eq!(pixmaps.len(), 2);
            for (size, (width, height, pixels)) in [22, 32].into_iter().zip(&pixmaps) {
                assert_eq!((*width, *height), (size, size));
                assert_eq!(pixels.len(), (size * size * 4) as usize);
                assert_eq!(&pixels[..4], &[0, 0, 0, 0]);
                assert_eq!(pixels[((size / 2 * size + size / 2) * 4) as usize], 255);
            }
            images.push(pixmaps[1].2.clone());
        }
        for (index, image) in images.iter().enumerate() {
            assert!(images.iter().skip(index + 1).all(|other| other != image));
        }
        assert_eq!(
            property_value("IconPixmap", "error", "error")
                .type_()
                .as_str(),
            "a(iiay)"
        );
        assert_eq!(
            property_value("AttentionIconPixmap", "error", "error")
                .type_()
                .as_str(),
            "a(iiay)"
        );
        assert_ne!(
            property_value("AttentionIconPixmap", "offline", "offline"),
            property_value("AttentionIconPixmap", "error", "error")
        );
    }

    #[test]
    fn cloud_and_status_symbols_have_expected_colors() {
        assert_eq!(
            icon_sample(IconKind::Idle, 16.0, 13.0),
            Some([255, 255, 255])
        );
        assert_eq!(
            icon_sample(IconKind::Idle, 4.0, 4.0),
            Some([0x17, 0x64, 0xc2])
        );
        assert_eq!(icon_sample(IconKind::Idle, 0.0, 0.0), None);
        for kind in [
            IconKind::Attention,
            IconKind::Offline,
            IconKind::Paused,
            IconKind::Transferring,
            IconKind::Checking,
            IconKind::Synced,
        ] {
            assert_ne!(
                icon_sample(kind, 24.0, 19.0),
                icon_sample(IconKind::Idle, 24.0, 19.0)
            );
            assert!((19..29).any(|y| (19..29).any(|x| badge_symbol(kind, x as f32, y as f32))));
        }
    }

    #[test]
    fn both_status_notifier_interfaces_have_valid_introspection() {
        for (interface, _) in INTERFACES {
            let node = gio::DBusNodeInfo::for_xml(&interface_xml(interface)).unwrap();
            assert!(node.lookup_interface(interface).is_some());
        }
    }

    #[test]
    fn dbus_menu_layout_and_actions_match() {
        let node = gio::DBusNodeInfo::for_xml(MENU_XML).unwrap();
        assert!(node.lookup_interface("com.canonical.dbusmenu").is_some());
        let state = MenuState {
            paused: Some(false),
            root_available: false,
            outstanding: None,
        };
        let layout = menu_layout(0, -1, &[], state);
        assert_eq!(layout.type_().as_str(), "(ia{sv}av)");
        assert_eq!(layout.child_value(2).n_children(), MENU_ITEMS.len());
        assert_eq!(menu_action(3), Some(MenuAction::TogglePause));
        assert_eq!(menu_action(6), Some(MenuAction::OpenFolder));
        assert_eq!(menu_action(99), None);
        let paused = menu_properties(
            3,
            &[],
            MenuState {
                paused: Some(true),
                ..state
            },
        );
        assert_eq!(paused["label"].str(), Some("Возобновить синхронизацию"));
        let unavailable = menu_properties(
            3,
            &[],
            MenuState {
                paused: None,
                ..state
            },
        );
        assert_eq!(unavailable["enabled"].get::<bool>(), Some(false));
        let missing_root = menu_properties(6, &[], state);
        assert_eq!(missing_root["enabled"].get::<bool>(), Some(false));
        let configured_root = menu_properties(
            6,
            &[],
            MenuState {
                root_available: true,
                ..state
            },
        );
        assert!(!configured_root.contains_key("enabled"));
        let counted = menu_properties(
            2,
            &[],
            MenuState {
                outstanding: Some(42),
                ..state
            },
        );
        assert_eq!(counted["label"].str(), Some("Очередь и история (42)"));
        let empty = menu_properties(
            2,
            &[],
            MenuState {
                outstanding: Some(0),
                ..state
            },
        );
        assert_eq!(empty["label"].str(), Some("Очередь и история"));
    }
}
