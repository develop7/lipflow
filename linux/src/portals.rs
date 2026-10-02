//! Typed D-Bus portal calls. Requests are subscribed before calling to avoid restored-session races.
use anyhow::{anyhow, bail, Context, Result};
use futures_channel::oneshot;
use gio::prelude::*;
use glib::{
    variant::{Handle, ObjectPath},
    Variant,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::Write,
    os::fd::OwnedFd,
    rc::Rc,
};

const BUS: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
pub type Dict = HashMap<String, Variant>;

#[derive(Clone)]
pub struct Portal {
    pub connection: gio::DBusConnection,
}

fn interface(name: &str) -> String {
    format!("org.freedesktop.portal.{name}")
}
pub fn options<const N: usize>(entries: [(&str, Variant); N]) -> Dict {
    entries
        .into_iter()
        .map(|(key, val)| (key.into(), val))
        .collect()
}
pub fn property<T: glib::variant::FromVariant>(dict: &Dict, key: &str) -> Option<T> {
    dict.get(key)?.get()
}
fn token() -> String {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "lipflow_{}_{}",
        std::process::id(),
        SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

impl Portal {
    pub fn new() -> Result<Self> {
        Ok(Self {
            connection: gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?,
        })
    }

    pub async fn call(&self, iface: &str, method: &str, params: &Variant) -> Result<Variant> {
        Ok(self
            .connection
            .call_future(
                Some(BUS),
                PATH,
                &interface(iface),
                method,
                Some(params),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await?)
    }

    pub async fn version(&self, iface: &str) -> u32 {
        self.connection
            .call_future(
                Some(BUS),
                PATH,
                "org.freedesktop.DBus.Properties",
                "Get",
                Some(&(interface(iface), "version").to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                3000,
            )
            .await
            .ok()
            .and_then(|v| v.get::<(Variant,)>().and_then(|(v,)| v.get()))
            .unwrap_or(0)
    }

    pub async fn request(
        &self,
        iface: &str,
        method: &str,
        mut opts: Dict,
        params: impl FnOnce(Dict) -> Variant,
    ) -> Result<Dict> {
        let handle_token = token();
        opts.insert("handle_token".into(), handle_token.to_variant());
        let sender = self
            .connection
            .unique_name()
            .context("D-Bus connection has no unique name")?
            .trim_start_matches(':')
            .replace('.', "_");
        let path = format!("{PATH}/request/{sender}/{handle_token}");
        let mut request = PendingRequest {
            portal: self.clone(),
            path: path.clone(),
            complete: false,
        };
        let (tx, rx) = oneshot::channel();
        let tx = RefCell::new(Some(tx));
        let subscription = self.connection.subscribe_to_signal(
            Some(BUS),
            Some(&interface("Request")),
            Some("Response"),
            Some(&path),
            None,
            gio::DBusSignalFlags::NONE,
            move |signal| {
                if let Some(tx) = tx.borrow_mut().take() {
                    let _ = tx.send(signal.parameters.clone());
                }
            },
        );
        let reply = self.call(iface, method, &params(opts)).await?;
        let (actual,) = reply
            .get::<(ObjectPath,)>()
            .context("Invalid portal request reply")?;
        if actual.as_str() != path {
            request.path = actual.as_str().into();
            bail!("Portal returned an unexpected request handle");
        }
        let response = rx.await.context("Portal request was interrupted")?;
        drop(subscription);
        request.complete = true;
        let (code, results) = response
            .get::<(u32, Dict)>()
            .context("Invalid portal response")?;
        match code {
            0 => Ok(results),
            1 => bail!("Permission request cancelled"),
            _ => bail!("{iface} permission was not granted"),
        }
    }

    pub async fn fd(&self, iface: &str, method: &str, params: &Variant) -> Result<OwnedFd> {
        let (reply, descriptors) = self
            .connection
            .call_with_unix_fd_list_future(
                Some(BUS),
                PATH,
                &interface(iface),
                method,
                Some(params),
                None,
                gio::DBusCallFlags::NONE,
                5000,
                None::<&gio::UnixFDList>,
            )
            .await?;
        let (Handle(index),) = reply
            .get::<(Handle,)>()
            .context("Invalid file descriptor reply")?;
        Ok(descriptors
            .context("Portal did not send a file descriptor")?
            .get(index)?)
    }

    async fn create_session(&self, iface: &str) -> Result<Session> {
        let results = self
            .request(
                iface,
                "CreateSession",
                options([("session_handle_token", token().to_variant())]),
                |opts| (opts,).to_variant(),
            )
            .await?;
        let path = ObjectPath::try_from(
            property::<String>(&results, "session_handle").context("No session handle")?,
        )?;
        let mut session = Session {
            portal: self.clone(),
            path,
            subscriptions: Vec::new(),
            alive: Rc::new(Cell::new(true)),
        };
        session.watch_closed(|| {});
        Ok(session)
    }

    pub async fn shortcuts(
        &self,
        trigger: &str,
        event: impl Fn(bool, String) + 'static,
        closed: impl Fn() + 'static,
    ) -> Result<(Session, String)> {
        if self.version("GlobalShortcuts").await == 0 {
            bail!("GlobalShortcuts portal is unavailable. Install xdg-desktop-portal-gnome.");
        }
        let mut session = self.create_session("GlobalShortcuts").await?;
        let event = Rc::new(event);
        for (signal, pressed) in [("Activated", true), ("Deactivated", false)] {
            let path = session.path.clone();
            let event = event.clone();
            session
                .subscriptions
                .push(self.connection.subscribe_to_signal(
                    Some(BUS),
                    Some(&interface("GlobalShortcuts")),
                    Some(signal),
                    Some(PATH),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |signal| {
                        if let Some((handle, id, _timestamp, _options)) =
                            signal.parameters.get::<(ObjectPath, String, u64, Dict)>()
                        {
                            if handle == path {
                                event(pressed, id);
                            }
                        }
                    },
                ));
        }
        session.watch_closed(closed);
        let shortcuts = vec![
            (
                "dictate",
                options([
                    (
                        "description",
                        "Hold to dictate; double-tap for hands-free".to_variant(),
                    ),
                    ("preferred_trigger", trigger.to_variant()),
                ]),
            ),
            (
                "cancel",
                options([
                    ("description", "Cancel Lipflow dictation".to_variant()),
                    ("preferred_trigger", "CTRL+ALT+Escape".to_variant()),
                ]),
            ),
        ];
        let path = session.path.clone();
        let result = self
            .request("GlobalShortcuts", "BindShortcuts", Dict::new(), |opts| {
                (path, shortcuts, "", opts).to_variant()
            })
            .await?;
        let bindings = property::<Vec<(String, Dict)>>(&result, "shortcuts").unwrap_or_default();
        let (_, binding) = bindings
            .iter()
            .find(|(id, _)| id == "dictate")
            .context("No dictation shortcut was granted")?;
        let description =
            property::<String>(binding, "trigger_description").unwrap_or_else(|| trigger.into());
        Ok((session, description))
    }

    pub async fn camera_permission(&self) -> Result<()> {
        self.request("Camera", "AccessCamera", Dict::new(), |opts| {
            (opts,).to_variant()
        })
        .await?;
        Ok(())
    }
    pub async fn camera_fd(&self) -> Result<OwnedFd> {
        self.fd("Camera", "OpenPipeWireRemote", &(Dict::new(),).to_variant())
            .await
    }

    pub async fn background(&self, autostart: bool) -> Result<(bool, bool)> {
        let result = self
            .request(
                "Background",
                "RequestBackground",
                options([
                    (
                        "reason",
                        "Keep Lipflow ready for your dictation shortcut".to_variant(),
                    ),
                    ("autostart", autostart.to_variant()),
                    (
                        "commandline",
                        vec!["lipflow", "run", "--background"].to_variant(),
                    ),
                ]),
                |opts| ("", opts).to_variant(),
            )
            .await?;
        Ok((
            property(&result, "background").unwrap_or(false),
            property(&result, "autostart").unwrap_or(false),
        ))
    }

    pub async fn remote_paste(
        &self,
        restore_token: &str,
        closed: impl Fn() + 'static,
    ) -> Result<(Paste, String)> {
        if self.version("Clipboard").await == 0 {
            bail!("Automatic background paste requires the Clipboard portal (GNOME 50).");
        }
        let mut session = self.create_session("RemoteDesktop").await?;
        self.call(
            "Clipboard",
            "RequestClipboard",
            &(session.path.clone(), Dict::new()).to_variant(),
        )
        .await?;
        let mut opts = options([("types", 1u32.to_variant())]); // Keyboard only. No screen or pointer.
        if self.version("RemoteDesktop").await >= 2 {
            opts.insert("persist_mode".into(), 2u32.to_variant());
            if !restore_token.is_empty() {
                opts.insert("restore_token".into(), restore_token.to_variant());
            }
        }
        let path = session.path.clone();
        self.request("RemoteDesktop", "SelectDevices", opts, |opts| {
            (path, opts).to_variant()
        })
        .await?;
        let path = session.path.clone();
        let result = self
            .request("RemoteDesktop", "Start", Dict::new(), |opts| {
                (path, "", opts).to_variant()
            })
            .await?;
        if property::<u32>(&result, "devices").unwrap_or(0) & 1 == 0
            || !property::<bool>(&result, "clipboard_enabled").unwrap_or(false)
        {
            bail!("Keyboard and clipboard permission are required for automatic paste");
        }
        let restore_token = property::<String>(&result, "restore_token").unwrap_or_default();
        if !session.alive.get() {
            bail!("Paste permission closed during setup");
        }
        let transfer = Rc::new(Transfer::default());
        let pending = transfer.clone();
        session.watch_closed(move || {
            if let Some(tx) = pending.completion.borrow_mut().take() {
                let _ = tx.send(Err(anyhow!("Paste permission was revoked")));
            }
            closed();
        });
        let path = session.path.clone();
        let portal = self.clone();
        let data = transfer.clone();
        session
            .subscriptions
            .push(self.connection.subscribe_to_signal(
                Some(BUS),
                Some(&interface("Clipboard")),
                Some("SelectionTransfer"),
                Some(PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some((handle, mime, serial)) =
                        signal.parameters.get::<(ObjectPath, String, u32)>()
                    else {
                        return;
                    };
                    if handle != path {
                        return;
                    }
                    let portal = portal.clone();
                    let data = data.clone();
                    let bytes = data.bytes.borrow().clone();
                    let generation = data.generation.get();
                    glib::MainContext::default().spawn_local(async move {
                        let result = async {
                            if !["text/plain;charset=utf-8", "text/plain"].contains(&mime.as_str())
                            {
                                bail!("Unsupported clipboard format");
                            }
                            let descriptor = portal
                                .fd(
                                    "Clipboard",
                                    "SelectionWrite",
                                    &(handle.clone(), serial).to_variant(),
                                )
                                .await?;
                            let (tx, rx) = oneshot::channel();
                            std::thread::spawn(move || {
                                let _ = tx.send(std::fs::File::from(descriptor).write_all(&bytes));
                            });
                            rx.await??;
                            Ok::<_, anyhow::Error>(())
                        }
                        .await;
                        let ack = portal
                            .call(
                                "Clipboard",
                                "SelectionWriteDone",
                                &(handle, serial, result.is_ok()).to_variant(),
                            )
                            .await;
                        if data.generation.get() == generation {
                            if let Some(tx) = data.completion.borrow_mut().take() {
                                let _ = tx.send(result.and(ack.map(|_| ())));
                            }
                        }
                    });
                },
            ));
        Ok((Paste { session, transfer }, restore_token))
    }
}

struct PendingRequest {
    portal: Portal,
    path: String,
    complete: bool,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        if !self.complete {
            self.portal.connection.call(
                Some(BUS),
                &self.path,
                &interface("Request"),
                "Close",
                None,
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
                |_| {},
            );
        }
    }
}

pub struct Session {
    pub portal: Portal,
    pub path: ObjectPath,
    subscriptions: Vec<gio::SignalSubscription>,
    pub alive: Rc<Cell<bool>>,
}
impl Session {
    fn watch_closed(&mut self, callback: impl Fn() + 'static) {
        let callback = Rc::new(callback);
        let alive = self.alive.clone();
        let closed = callback.clone();
        self.subscriptions
            .push(self.portal.connection.subscribe_to_signal(
                Some(BUS),
                Some(&interface("Session")),
                Some("Closed"),
                Some(self.path.as_str()),
                None,
                gio::DBusSignalFlags::NONE,
                move |_| {
                    alive.set(false);
                    closed();
                },
            ));
        let alive = self.alive.clone();
        self.subscriptions
            .push(self.portal.connection.subscribe_to_signal(
                Some("org.freedesktop.DBus"),
                Some("org.freedesktop.DBus"),
                Some("NameOwnerChanged"),
                Some("/org/freedesktop/DBus"),
                Some(BUS),
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    if let Some((_name, old, new)) =
                        signal.parameters.get::<(String, String, String)>()
                    {
                        if !old.is_empty() && old != new {
                            alive.set(false);
                            callback();
                        }
                    }
                },
            ));
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.subscriptions.clear();
        self.alive.set(false);
        self.portal.connection.call(
            Some(BUS),
            self.path.as_str(),
            &interface("Session"),
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}

#[derive(Default)]
struct Transfer {
    bytes: RefCell<Vec<u8>>,
    completion: RefCell<Option<oneshot::Sender<Result<()>>>>,
    generation: Cell<u64>,
}
pub struct Paste {
    pub session: Session,
    transfer: Rc<Transfer>,
}
impl Paste {
    pub async fn send(&self, text: &str, paste: bool) -> Result<()> {
        self.send_if(text, paste, || true).await
    }

    pub async fn send_if(
        &self,
        text: &str,
        paste: bool,
        should_continue: impl Fn() -> bool,
    ) -> Result<()> {
        if !self.session.alive.get() {
            bail!("Paste session is closed. Reconnect desktop permissions.");
        }
        if !should_continue() {
            bail!("Paste cancelled");
        }
        if self.transfer.completion.borrow().is_some() {
            bail!("Another clipboard transfer is still running");
        }
        self.transfer
            .generation
            .set(self.transfer.generation.get() + 1);
        *self.transfer.bytes.borrow_mut() = text.as_bytes().to_vec();
        let (tx, rx) = oneshot::channel();
        if paste {
            *self.transfer.completion.borrow_mut() = Some(tx);
        }
        let result = async {
            if !should_continue() { bail!("Paste cancelled"); }
            self.session.portal.call("Clipboard", "SetSelection", &(self.session.path.clone(),
                options([("mime_types", vec!["text/plain;charset=utf-8", "text/plain"].to_variant())])).to_variant()).await?;
            if paste {
                let keyboard = [(0xFFE3, 1), (0x76, 1), (0x76, 0), (0xFFE3, 0)];
                for (key, state) in keyboard {
                    if state == 1 && !should_continue() {
                        for key in [0x76i32, 0xFFE3] {
                            let _ = self.session.portal.call("RemoteDesktop", "NotifyKeyboardKeysym",
                                &(self.session.path.clone(), Dict::new(), key, 0u32).to_variant()).await;
                        }
                        bail!("Paste cancelled");
                    }
                    let result = self.session.portal.call("RemoteDesktop", "NotifyKeyboardKeysym",
                        &(self.session.path.clone(), Dict::new(), key as i32, state as u32).to_variant()).await;
                    if let Err(error) = result {
                        for key in [0x76i32, 0xFFE3] {
                            let _ = self.session.portal.call("RemoteDesktop", "NotifyKeyboardKeysym",
                                &(self.session.path.clone(), Dict::new(), key, 0u32).to_variant()).await;
                        }
                        return Err(error);
                    }
                }
                use futures_util::{future::{select, Either}, FutureExt};
                match select(rx.boxed_local(), glib::timeout_future_seconds(10).boxed_local()).await {
                    Either::Left((result, _)) => result.context("Clipboard transfer was interrupted")??,
                    Either::Right(_) => bail!("The focused application did not accept the paste. The result is saved in history."),
                }
            }
            Ok(())
        }.await;
        self.transfer.completion.borrow_mut().take();
        result
    }
}
