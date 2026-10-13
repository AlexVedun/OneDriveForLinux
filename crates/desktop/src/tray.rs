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
    History,
    TogglePause,
    Refresh,
    Quit,
}

const MENU_ITEMS: [(i32, &str, MenuAction); 5] = [
    (1, "Открыть окно", MenuAction::Show),
    (2, "Очередь и история", MenuAction::History),
    (3, "Приостановить синхронизацию", MenuAction::TogglePause),
    (4, "Проверить OneDrive", MenuAction::Refresh),
    (5, "Выход", MenuAction::Quit),
];

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

fn menu_properties(
    id: i32,
    names: &[String],
    paused: Option<bool>,
) -> HashMap<String, glib::Variant> {
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
        let label = if id == 3 && paused == Some(true) {
            "Возобновить синхронизацию"
        } else {
            label
        };
        properties.insert("label".to_owned(), label.to_variant());
    }
    if id == 3
        && paused.is_none()
        && (names.is_empty() || names.iter().any(|name| name == "enabled"))
    {
        properties.insert("enabled".to_owned(), false.to_variant());
    }
    properties
}

fn menu_layout(id: i32, depth: i32, names: &[String], paused: Option<bool>) -> glib::Variant {
    let children: Vec<glib::Variant> = if id == 0 && depth != 0 {
        MENU_ITEMS
            .iter()
            .map(|(child_id, _, _)| menu_layout(*child_id, depth - 1, names, paused))
            .collect()
    } else {
        Vec::new()
    };
    glib::Variant::tuple_from_iter([
        id.to_variant(),
        menu_properties(id, names, paused).to_variant(),
        children.to_variant(),
    ])
}

fn register_menu(
    connection: &gio::DBusConnection,
    action: Rc<dyn Fn(MenuAction)>,
    paused: Rc<Cell<Option<bool>>>,
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
                        let layout = menu_layout(id, depth, &names, paused.get());
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
                            .map(|id| (id, menu_properties(id, &names, paused.get())))
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
                        if let Some(value) = menu_properties(id, &[], paused.get()).get(&name) {
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
    icon: &'static str,
}

fn presentation(state: &str) -> Presentation {
    match state {
        "needs-login" | "error" | "reconciliation-needed" | "daemon-unavailable" => Presentation {
            status: "NeedsAttention",
            icon: "dialog-warning-symbolic",
        },
        "offline" => Presentation {
            status: "NeedsAttention",
            icon: "network-offline-symbolic",
        },
        "paused" => Presentation {
            status: "Active",
            icon: "media-playback-pause-symbolic",
        },
        "transferring" | "checking" => Presentation {
            status: "Active",
            icon: "view-refresh-symbolic",
        },
        "synced" => Presentation {
            status: "Active",
            icon: "object-select-symbolic",
        },
        _ => Presentation {
            status: "Active",
            icon: "network-idle-symbolic",
        },
    }
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
            <property name="AttentionIconName" type="s" access="read"/>
            <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
            <property name="ItemIsMenu" type="b" access="read"/>
            <property name="Menu" type="o" access="read"/>
            <signal name="NewIcon"/>
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
        "IconName" => presentation(state).icon.to_variant(),
        "AttentionIconName" => "dialog-warning-symbolic".to_variant(),
        "ToolTip" => (
            presentation(state).icon,
            Vec::<(i32, i32, Vec<u8>)>::new(),
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
    paused: Rc<Cell<Option<bool>>>,
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
            paused: Rc::new(Cell::new(None)),
        };
        tray.registrations.push(register_menu(
            &tray.connection,
            action,
            tray.paused.clone(),
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
            let _ = self
                .connection
                .emit_signal(None, OBJECT_PATH, interface, "NewToolTip", None);
        }
    }

    pub fn update_paused(&self, paused: Option<bool>) {
        if self.paused.replace(paused) == paused {
            return;
        }
        let updated = vec![(3_i32, menu_properties(3, &[], paused))];
        let removed: Vec<(i32, Vec<String>)> = if paused.is_some() {
            vec![(3, vec!["enabled".to_owned()])]
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
        assert_eq!(presentation("offline").icon, "network-offline-symbolic");
        assert_eq!(presentation("paused").icon, "media-playback-pause-symbolic");
        assert_eq!(presentation("transferring").status, "Active");
        assert_eq!(presentation("connected").icon, "network-idle-symbolic");
        assert_eq!(presentation("synced").icon, "object-select-symbolic");
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
        let layout = menu_layout(0, -1, &[], Some(false));
        assert_eq!(layout.type_().as_str(), "(ia{sv}av)");
        assert_eq!(layout.child_value(2).n_children(), MENU_ITEMS.len());
        assert_eq!(menu_action(3), Some(MenuAction::TogglePause));
        assert_eq!(menu_action(99), None);
        let paused = menu_properties(3, &[], Some(true));
        assert_eq!(paused["label"].str(), Some("Возобновить синхронизацию"));
        let unavailable = menu_properties(3, &[], None);
        assert_eq!(unavailable["enabled"].get::<bool>(), Some(false));
    }
}
