use adw::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

const OBJECT_PATH: &str = "/StatusNotifierItem";
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
        "Menu" => glib::variant::ObjectPath::try_from("/")
            .expect("valid empty menu path")
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
}

impl TrayIndicator {
    pub fn start(app: &adw::Application, initial: &str) -> Result<Self, String> {
        let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
            .map_err(|error| error.to_string())?;
        let mut tray = Self {
            connection,
            registrations: Vec::new(),
            owners: Vec::new(),
            watcher_cleanup: Vec::new(),
            state: Rc::new(RefCell::new(initial.to_owned())),
        };
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
}
