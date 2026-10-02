//! Real D-Bus/Unix-FD integration tests against a private portal fixture.
use crate::portals::*;
use gio::prelude::*;
use glib::variant::{Handle, ObjectPath, ToVariant};
use std::{
    cell::{Cell, RefCell},
    io::{BufRead, Read},
    os::unix::net::UnixStream,
    process::{Child, Command, Stdio},
    rc::Rc,
    sync::{Arc, Mutex},
};

const BUS: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE: &str = "/org/freedesktop/portal/desktop/session/lipflow/remote";
const SHORTCUT: &str = "/org/freedesktop/portal/desktop/session/lipflow/shortcuts";
const XML: &str = r#"<node>
<interface name="org.freedesktop.portal.Camera">
 <property name="version" type="u" access="read"/>
 <method name="AccessCamera"><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
 <method name="OpenPipeWireRemote"><arg type="a{sv}" direction="in"/><arg type="h" direction="out"/></method>
</interface>
<interface name="org.freedesktop.portal.GlobalShortcuts">
 <property name="version" type="u" access="read"/>
 <method name="CreateSession"><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
 <method name="BindShortcuts"><arg type="o" direction="in"/><arg type="a(sa{sv})" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
 <signal name="Activated"><arg type="o"/><arg type="s"/><arg type="t"/><arg type="a{sv}"/></signal>
 <signal name="Deactivated"><arg type="o"/><arg type="s"/><arg type="t"/><arg type="a{sv}"/></signal>
</interface>
<interface name="org.freedesktop.portal.RemoteDesktop">
 <property name="version" type="u" access="read"/>
 <method name="CreateSession"><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
 <method name="SelectDevices"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
 <method name="Start"><arg type="o" direction="in"/><arg type="s" direction="in"/><arg type="a{sv}" direction="in"/><arg type="o" direction="out"/></method>
 <method name="NotifyKeyboardKeysym"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="in"/></method>
</interface>
<interface name="org.freedesktop.portal.Clipboard">
 <property name="version" type="u" access="read"/>
 <method name="RequestClipboard"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/></method>
 <method name="SetSelection"><arg type="o" direction="in"/><arg type="a{sv}" direction="in"/></method>
 <method name="SelectionWrite"><arg type="o" direction="in"/><arg type="u" direction="in"/><arg type="h" direction="out"/></method>
 <method name="SelectionWriteDone"><arg type="o" direction="in"/><arg type="u" direction="in"/><arg type="b" direction="in"/></method>
 <signal name="SelectionTransfer"><arg type="o"/><arg type="s"/><arg type="u"/></signal>
</interface>
<interface name="org.freedesktop.portal.Session">
 <method name="Close"/>
 <signal name="Closed"><arg type="a{sv}"/></signal>
</interface>
<interface name="org.freedesktop.portal.Request">
 <signal name="Response"><arg type="u"/><arg type="a{sv}"/></signal>
</interface>
</node>"#;

struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[derive(Default)]
struct State {
    denied: Cell<bool>,
    clipboard_requested: Cell<bool>,
    clipboard_granted: Cell<bool>,
    keys: RefCell<Vec<(i32, u32)>>,
    bytes: Arc<Mutex<Vec<u8>>>,
    ack: Cell<bool>,
    closed: Cell<u32>,
}
fn path(value: &str) -> ObjectPath {
    ObjectPath::try_from(value).unwrap()
}
fn response(
    connection: &gio::DBusConnection,
    sender: &str,
    opts: Dict,
    results: Dict,
    denied: bool,
    invocation: gio::DBusMethodInvocation,
) {
    let token = property::<String>(&opts, "handle_token").unwrap();
    let sender_path = sender.trim_start_matches(':').replace('.', "_");
    let handle = format!("{PATH}/request/{sender_path}/{token}");
    // Deliberately emit before the method reply; this is how restored permissions can race.
    connection
        .emit_signal(
            Some(sender),
            &handle,
            "org.freedesktop.portal.Request",
            "Response",
            Some(&(if denied { 1u32 } else { 0 }, results).to_variant()),
        )
        .unwrap();
    invocation.return_value(Some(&(path(&handle),).to_variant()));
}

#[test]
fn portal_requests_shortcuts_permissions_and_clipboard_fds_roundtrip() {
    let mut daemon = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("dbus-daemon is required for portal tests");
    let mut address = String::new();
    std::io::BufReader::new(daemon.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let _bus = Bus(daemon);
    assert!(!address.is_empty(), "Private session bus could not start");
    let flags = gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
        | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION;
    let service =
        gio::DBusConnection::for_address_sync(address.trim(), flags, None, gio::Cancellable::NONE)
            .unwrap();
    let client =
        gio::DBusConnection::for_address_sync(address.trim(), flags, None, gio::Cancellable::NONE)
            .unwrap();
    service
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            Some(&(BUS, 0u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            3000,
            gio::Cancellable::NONE,
        )
        .unwrap();
    let state = Rc::new(State::default());
    state.clipboard_granted.set(true);
    let context = glib::MainContext::default();
    context
        .with_thread_default(|| {
            let node = gio::DBusNodeInfo::for_xml(XML).unwrap();
            let mut registrations = Vec::new();
            for name in ["Camera", "GlobalShortcuts", "RemoteDesktop", "Clipboard"] {
                let iface = format!("org.freedesktop.portal.{name}");
                let info = node.lookup_interface(&iface).unwrap();
                let data = state.clone();
                registrations.push(
                    service
                        .register_object(PATH, &info)
                        .property(|_, _, _, _, _| 2u32.to_variant())
                        .method_call(
                            move |connection, sender, _, iface, method, params, invocation| {
                                let sender = sender.unwrap();
                                let iface = iface.unwrap();
                                let empty =
                                    || invocation.clone().return_value(Some(&().to_variant()));
                                match method {
                                    "AccessCamera" => response(
                                        &connection,
                                        sender,
                                        params.get::<(Dict,)>().unwrap().0,
                                        Dict::new(),
                                        data.denied.get(),
                                        invocation,
                                    ),
                                    "OpenPipeWireRemote" => {
                                        let file = std::fs::File::open("/dev/null").unwrap();
                                        let fds = gio::UnixFDList::new();
                                        let index = fds.append(file).unwrap();
                                        invocation.return_value_with_unix_fd_list(
                                            Some(&(Handle(index),).to_variant()),
                                            Some(&fds),
                                        );
                                    }
                                    "CreateSession" => {
                                        let session = if iface.ends_with("GlobalShortcuts") {
                                            SHORTCUT
                                        } else {
                                            REMOTE
                                        };
                                        response(
                                            &connection,
                                            sender,
                                            params.get::<(Dict,)>().unwrap().0,
                                            options([("session_handle", session.to_variant())]),
                                            false,
                                            invocation,
                                        );
                                    }
                                    "BindShortcuts" => {
                                        let (session, bindings, _, opts) = params
                                            .get::<(ObjectPath, Vec<(String, Dict)>, String, Dict)>(
                                            )
                                            .unwrap();
                                        assert_eq!(session.as_str(), SHORTCUT);
                                        assert_eq!(bindings.len(), 2);
                                        response(
                                            &connection,
                                            sender,
                                            opts,
                                            options([(
                                                "shortcuts",
                                                vec![(
                                                    "dictate",
                                                    options([(
                                                        "trigger_description",
                                                        "Ctrl+Alt+Space".to_variant(),
                                                    )]),
                                                )]
                                                .to_variant(),
                                            )]),
                                            false,
                                            invocation,
                                        );
                                    }
                                    "RequestClipboard" => {
                                        data.clipboard_requested.set(true);
                                        empty();
                                    }
                                    "SelectDevices" => {
                                        let (_, opts) = params.get::<(ObjectPath, Dict)>().unwrap();
                                        assert_eq!(property::<u32>(&opts, "types"), Some(1));
                                        assert_eq!(property::<u32>(&opts, "persist_mode"), Some(2));
                                        assert_eq!(
                                            property::<String>(&opts, "restore_token").as_deref(),
                                            Some("old-token")
                                        );
                                        response(
                                            &connection,
                                            sender,
                                            opts,
                                            Dict::new(),
                                            false,
                                            invocation,
                                        );
                                    }
                                    "Start" => {
                                        assert!(
                                            data.clipboard_requested.get(),
                                            "Clipboard must be requested before Start"
                                        );
                                        let (_, _, opts) =
                                            params.get::<(ObjectPath, String, Dict)>().unwrap();
                                        response(
                                            &connection,
                                            sender,
                                            opts,
                                            options([
                                                ("devices", 1u32.to_variant()),
                                                (
                                                    "clipboard_enabled",
                                                    data.clipboard_granted.get().to_variant(),
                                                ),
                                                ("restore_token", "replacement-token".to_variant()),
                                            ]),
                                            false,
                                            invocation,
                                        );
                                    }
                                    "SetSelection" => {
                                        let (_, opts) = params.get::<(ObjectPath, Dict)>().unwrap();
                                        assert!(property::<Vec<String>>(&opts, "mime_types")
                                            .unwrap()
                                            .contains(&"text/plain;charset=utf-8".into()));
                                        empty();
                                    }
                                    "NotifyKeyboardKeysym" => {
                                        let (_, _, key, pressed) =
                                            params.get::<(ObjectPath, Dict, i32, u32)>().unwrap();
                                        data.keys.borrow_mut().push((key, pressed));
                                        if key == 0x76 && pressed == 1 {
                                            connection
                                                .emit_signal(
                                                    Some(sender),
                                                    PATH,
                                                    "org.freedesktop.portal.Clipboard",
                                                    "SelectionTransfer",
                                                    Some(
                                                        &(
                                                            path(REMOTE),
                                                            "text/plain;charset=utf-8",
                                                            7u32,
                                                        )
                                                            .to_variant(),
                                                    ),
                                                )
                                                .unwrap();
                                        }
                                        empty();
                                    }
                                    "SelectionWrite" => {
                                        let (_, serial) =
                                            params.get::<(ObjectPath, u32)>().unwrap();
                                        assert_eq!(serial, 7);
                                        let (mut reader, writer) = UnixStream::pair().unwrap();
                                        let bytes = data.bytes.clone();
                                        std::thread::spawn(move || {
                                            let mut result = Vec::new();
                                            reader.read_to_end(&mut result).unwrap();
                                            *bytes.lock().unwrap() = result;
                                        });
                                        let fds = gio::UnixFDList::new();
                                        let index = fds.append(writer).unwrap();
                                        invocation.return_value_with_unix_fd_list(
                                            Some(&(Handle(index),).to_variant()),
                                            Some(&fds),
                                        );
                                    }
                                    "SelectionWriteDone" => {
                                        let (_, serial, success) =
                                            params.get::<(ObjectPath, u32, bool)>().unwrap();
                                        assert_eq!(serial, 7);
                                        data.ack.set(success);
                                        empty();
                                    }
                                    _ => panic!("Unexpected portal method: {method}"),
                                }
                            },
                        )
                        .build()
                        .unwrap(),
                );
            }
            let session_info = node
                .lookup_interface("org.freedesktop.portal.Session")
                .unwrap();
            for session in [REMOTE, SHORTCUT] {
                let data = state.clone();
                registrations.push(
                    service
                        .register_object(session, &session_info)
                        .method_call(move |_, _, _, _, _, _, invocation| {
                            data.closed.set(data.closed.get() + 1);
                            invocation.return_value(Some(&().to_variant()));
                        })
                        .build()
                        .unwrap(),
                );
            }
            let portal = Portal {
                connection: client.clone(),
            };
            context.block_on(async {
                portal.camera_permission().await.unwrap();
                state.denied.set(true);
                assert!(portal
                    .camera_permission()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("cancelled"));
                state.denied.set(false);
                let mut camera_fd = std::fs::File::from(portal.camera_fd().await.unwrap());
                assert_eq!(camera_fd.read(&mut [0; 1]).unwrap(), 0);
                let events = Rc::new(RefCell::new(Vec::new()));
                let captured = events.clone();
                let closed = Rc::new(Cell::new(false));
                let ended = closed.clone();
                let (shortcuts, description) = portal
                    .shortcuts(
                        "CTRL+ALT+space",
                        move |pressed, id| captured.borrow_mut().push((pressed, id)),
                        move || ended.set(true),
                    )
                    .await
                    .unwrap();
                assert_eq!(description, "Ctrl+Alt+Space");
                for signal in ["Activated", "Deactivated"] {
                    service
                        .emit_signal(
                            None,
                            PATH,
                            "org.freedesktop.portal.GlobalShortcuts",
                            signal,
                            Some(&(path(SHORTCUT), "dictate", 123u64, Dict::new()).to_variant()),
                        )
                        .unwrap();
                }
                glib::timeout_future(std::time::Duration::from_millis(30)).await;
                assert_eq!(
                    &*events.borrow(),
                    &[(true, "dictate".into()), (false, "dictate".into())]
                );
                state.clipboard_granted.set(false);
                assert!(portal.remote_paste("old-token", || {}).await.is_err());
                state.clipboard_granted.set(true);
                let (paste, token) = portal.remote_paste("old-token", || {}).await.unwrap();
                assert_eq!(token, "replacement-token");
                paste.send("Hello, GNOME — café!", true).await.unwrap();
                for _ in 0..20 {
                    if !state.bytes.lock().unwrap().is_empty() {
                        break;
                    }
                    glib::timeout_future(std::time::Duration::from_millis(10)).await;
                }
                assert_eq!(
                    &*state.bytes.lock().unwrap(),
                    "Hello, GNOME — café!".as_bytes()
                );
                assert!(state.ack.get());
                assert_eq!(
                    &*state.keys.borrow(),
                    &[(0xFFE3, 1), (0x76, 1), (0x76, 0), (0xFFE3, 0)]
                );
                paste.send("Copy without keyboard", false).await.unwrap();
                assert_eq!(state.keys.borrow().len(), 4);
                assert!(paste
                    .send_if("cancelled before injection", true, || false)
                    .await
                    .is_err());
                assert_eq!(state.keys.borrow().len(), 4);
                service
                    .emit_signal(
                        None,
                        SHORTCUT,
                        "org.freedesktop.portal.Session",
                        "Closed",
                        Some(&(Dict::new(),).to_variant()),
                    )
                    .unwrap();
                glib::timeout_future(std::time::Duration::from_millis(30)).await;
                assert!(closed.get());
                assert!(!shortcuts.alive.get());
                service.close_sync(gio::Cancellable::NONE).unwrap();
                glib::timeout_future(std::time::Duration::from_millis(30)).await;
                assert!(!paste.session.alive.get());
                assert!(paste
                    .send("must not inject after revocation", true)
                    .await
                    .is_err());
                drop(paste);
                drop(shortcuts);
            });
            for registration in registrations {
                let _ = service.unregister_object(registration);
            }
        })
        .unwrap();
    let _ = client.close_sync(gio::Cancellable::NONE);
}
