use crate::{
    capture::{self, Active, CaptureState, Control},
    config::{self, Paths, Settings, APP_ID},
    engine::{self, Event},
    portals::{Paste, Portal, Session},
    ptt::{Action, PushToTalk},
    worker::{self, Command},
};
use adw::prelude::*;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{atomic::Ordering, mpsc, Arc},
    time::{Duration, Instant},
};

pub fn run(paths: Paths, settings: Settings, background: bool, smoke: bool) -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    let show = gio::SimpleAction::new("show", None);
    let weak_app = app.downgrade();
    show.connect_activate(move |_, _| {
        if let Some(app) = weak_app.upgrade() {
            app.activate();
        }
    });
    app.add_action(&show);
    let current = Rc::new(RefCell::new(None::<Rc<Ui>>));
    let slot = current.clone();
    app.connect_activate(move |app| {
        if let Some(ui) = slot.borrow().as_ref() {
            ui.window.present();
            return;
        }
        let ui = Ui::new(app, paths.clone(), settings.clone());
        if !background {
            ui.window.present();
        }
        if smoke {
            ui.status.set_text("Smoke test: native window constructed");
            assert_eq!(ui.stack.pages().n_items(), 4);
            let app = app.clone();
            glib::timeout_add_local_once(Duration::from_millis(750), move || app.quit());
        } else {
            ui.load(false);
            if background || settings.permissions_configured {
                ui.connect();
            }
        }
        *slot.borrow_mut() = Some(ui);
    });
    app.connect_shutdown(move |_| {
        if let Some(ui) = current.borrow_mut().take() {
            ui.state.active.lock().unwrap().take();
            let _ = ui.capture.send(Control::Close);
            let _ = ui.model.send(Command::MicStop(None));
            let _ = ui.model.send(Command::Shutdown);
            ui.shortcuts.borrow_mut().take();
            ui.paste.borrow_mut().take();
            ui.hold.borrow_mut().take();
        }
    });
    app.run_with_args::<&str>(&[])
}

struct Ui {
    app: adw::Application,
    window: adw::ApplicationWindow,
    hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    paths: Paths,
    settings: RefCell<Settings>,
    stack: gtk::Stack,
    status: gtk::Label,
    permission_status: gtk::Label,
    progress: gtk::ProgressBar,
    output: gtk::TextView,
    preview: gtk::Picture,
    practice_preview: gtk::Picture,
    record_button: gtk::Button,
    download_button: gtk::Button,
    sentence: gtk::Label,
    practice_feedback: gtk::Label,
    history: gtk::Box,
    model: mpsc::Sender<Command>,
    capture: mpsc::Sender<Control>,
    state: Arc<CaptureState>,
    ready: Cell<bool>,
    busy: Cell<bool>,
    stopping: Cell<bool>,
    generation: Cell<u64>,
    pending: Cell<Option<u64>>,
    ptt: RefCell<PushToTalk>,
    portal: RefCell<Option<Portal>>,
    shortcuts: RefCell<Option<Session>>,
    paste: RefCell<Option<Rc<Paste>>>,
    connecting: Cell<bool>,
    background: Cell<bool>,
    camera_allowed: Cell<bool>,
    sentences: RefCell<Vec<String>>,
    practice_index: Cell<usize>,
    last_clip: RefCell<Option<String>>,
    last_paste: Cell<Option<Instant>>,
    setting_rows: RefCell<std::collections::HashMap<String, adw::SwitchRow>>,
    cleanup_row: RefCell<Option<adw::ComboRow>>,
    beam_row: RefCell<Option<adw::ComboRow>>,
}

fn page() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_margin_top(20);
    page.set_margin_bottom(20);
    page.set_margin_start(24);
    page.set_margin_end(24);
    page
}
fn label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .selectable(true)
        .xalign(0.0)
        .build()
}
fn button(text: &str, container: &gtk::Box) -> gtk::Button {
    let b = gtk::Button::with_label(text);
    container.append(&b);
    b
}
fn text_view(editable: bool) -> gtk::TextView {
    gtk::TextView::builder()
        .editable(editable)
        .wrap_mode(gtk::WrapMode::WordChar)
        .vexpand(true)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build()
}
fn scroll(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(child)
        .vexpand(true)
        .min_content_height(140)
        .build()
}
fn text(view: &gtk::TextView) -> String {
    let b = view.buffer();
    b.text(&b.start_iter(), &b.end_iter(), false).to_string()
}

impl Ui {
    fn new(app: &adw::Application, paths: Paths, settings: Settings) -> Rc<Self> {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Lipflow")
            .default_width(720)
            .default_height(760)
            .build();
        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let header = adw::HeaderBar::new();
        let quit = gtk::Button::with_label("Quit");
        header.pack_end(&quit);
        layout.append(&header);
        let stack = gtk::Stack::builder().vexpand(true).build();
        let switcher = gtk::StackSwitcher::builder()
            .stack(&stack)
            .halign(gtk::Align::Center)
            .build();
        layout.append(&switcher);
        let status = label("Loading models…");
        status.set_margin_start(24);
        status.set_margin_end(24);
        status.set_margin_top(12);
        layout.append(&status);
        let progress = gtk::ProgressBar::new();
        progress.set_visible(false);
        layout.append(&progress);
        layout.append(&stack);
        window.set_content(Some(&layout));

        let dictate = page();
        dictate.append(&label("Mouth words while holding your shortcut. Double-tap for hands-free recording; tap again to finish."));
        let preview = gtk::Picture::builder()
            .width_request(320)
            .height_request(200)
            .can_shrink(true)
            .build();
        dictate.append(&preview);
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let record_button = button("Record", &controls);
        record_button.add_css_class("suggested-action");
        let cancel = button("Cancel", &controls);
        let copy = button("Copy result", &controls);
        dictate.append(&controls);
        let output = text_view(false);
        dictate.append(&scroll(&output));
        let download_button = gtk::Button::with_label("Download models and load (~1.2 GB)");
        dictate.append(&download_button);
        stack.add_titled(&dictate, Some("dictate"), "Dictate");

        let practice = page();
        practice.append(&label("Practice 24 sentences, then train on your face. Six clips are held out; an adapted model is kept only if its word error improves."));
        let sentence = label("Start practice to choose your sentences.");
        sentence.add_css_class("title-2");
        practice.append(&sentence);
        let practice_preview = gtk::Picture::builder()
            .width_request(320)
            .height_request(200)
            .can_shrink(true)
            .build();
        practice.append(&practice_preview);
        let practice_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let start_practice = button("Start practice", &practice_controls);
        let next = button("Next / skip", &practice_controls);
        let redo = button("Redo last clip", &practice_controls);
        practice.append(&practice_controls);
        practice.append(&label("Use the Record button or global shortcut to record the displayed sentence. Keep your face and mouth in view."));
        let practice_record = gtk::Button::with_label("Record / finish sentence");
        practice.append(&practice_record);
        let practice_feedback = label("");
        practice.append(&practice_feedback);
        let train = gtk::Button::with_label("Train on my face and phrases");
        practice.append(&train);
        let finish_practice = gtk::Button::with_label("Return to dictation");
        practice.append(&finish_practice);
        stack.add_titled(&practice, Some("practice"), "Practice");

        let history_page = page();
        let refresh = gtk::Button::with_label("Refresh history");
        history_page.append(&refresh);
        let history = gtk::Box::new(gtk::Orientation::Vertical, 10);
        history_page.append(&scroll(&history));
        stack.add_titled(&history_page, Some("history"), "History");

        let prefs = page();
        let permission_status =
            label("Connect desktop permissions to enable the camera and shortcuts.");
        prefs.append(&permission_status);
        let connect = gtk::Button::with_label("Connect desktop permissions");
        prefs.append(&connect);
        let shortcuts = gtk::Button::with_label("Change shortcuts…");
        prefs.append(&shortcuts);
        let (model_tx, model_rx) = mpsc::channel();
        let (capture_tx, capture_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let state = Arc::new(CaptureState::default());
        state
            .preview_enabled
            .store(settings.preview, Ordering::Relaxed);
        let ui = Rc::new(Self {
            app: app.clone(),
            window,
            hold: RefCell::new(Some(app.hold())),
            paths: paths.clone(),
            settings: RefCell::new(settings.clone()),
            stack,
            status,
            permission_status,
            progress,
            output,
            preview,
            practice_preview,
            record_button,
            download_button,
            sentence,
            practice_feedback,
            history,
            model: model_tx.clone(),
            capture: capture_tx,
            state: state.clone(),
            ready: Cell::new(false),
            busy: Cell::new(false),
            stopping: Cell::new(false),
            generation: Cell::new(0),
            pending: Cell::new(None),
            ptt: RefCell::new(PushToTalk::default()),
            portal: RefCell::new(None),
            shortcuts: RefCell::new(None),
            paste: RefCell::new(None),
            connecting: Cell::new(false),
            background: Cell::new(false),
            camera_allowed: Cell::new(false),
            sentences: RefCell::new(Vec::new()),
            practice_index: Cell::new(0),
            last_clip: RefCell::new(None),
            last_paste: Cell::new(None),
            setting_rows: RefCell::new(std::collections::HashMap::new()),
            cleanup_row: RefCell::new(None),
            beam_row: RefCell::new(None),
        });
        ui.preferences(&prefs);
        ui.stack
            .add_titled(&scroll(&prefs), Some("preferences"), "Preferences");
        let worker_state = state.clone();
        let worker_paths = paths.clone();
        let worker_events = event_tx.clone();
        std::thread::spawn(move || {
            worker::run(
                worker_paths,
                settings,
                worker_state,
                model_rx,
                worker_events,
            )
        });
        std::thread::spawn(move || capture::run(paths, state, capture_rx, model_tx, event_tx));

        macro_rules! clicked {
            ($button:expr, $action:expr) => {{
                let weak = Rc::downgrade(&ui);
                $button.connect_clicked(move |_| {
                    if let Some(ui) = weak.upgrade() {
                        ($action)(ui);
                    }
                });
            }};
        }
        clicked!(quit, |ui: Rc<Self>| ui.app.quit());
        clicked!(record_button_ref(&ui), |ui: Rc<Self>| ui.toggle_record());
        clicked!(practice_record, |ui: Rc<Self>| ui.toggle_record());
        clicked!(cancel, |ui: Rc<Self>| {
            ui.ptt.borrow_mut().cancel();
            ui.cancel(false);
        });
        clicked!(copy, |ui: Rc<Self>| {
            ui.window.clipboard().set_text(&text(&ui.output));
            ui.status.set_text("Copied");
        });
        clicked!(&ui.download_button, |ui: Rc<Self>| ui.load(true));
        clicked!(connect, |ui: Rc<Self>| ui.connect());
        clicked!(shortcuts, |ui: Rc<Self>| {
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                // Hold the session while awaiting the portal, without borrowing the RefCell.
                let portal_path = ui
                    .shortcuts
                    .borrow()
                    .as_ref()
                    .map(|s| (s.portal.clone(), s.path.clone()));
                if let Some((portal, path)) = portal_path {
                    use glib::variant::ToVariant;
                    if let Err(e) = portal
                        .call(
                            "GlobalShortcuts",
                            "ConfigureShortcuts",
                            &(path, "", crate::portals::Dict::new()).to_variant(),
                        )
                        .await
                    {
                        ui.error(&format!("{e:#}"));
                    }
                } else {
                    ui.error("Connect desktop permissions first");
                }
            });
        });
        clicked!(start_practice, |ui: Rc<Self>| {
            if ui.can_work() {
                ui.busy.set(true);
                let _ = ui.model.send(Command::Practice);
            }
        });
        clicked!(next, |ui: Rc<Self>| {
            if ui.can_work() {
                ui.next_sentence();
            }
        });
        clicked!(redo, |ui: Rc<Self>| {
            if !ui.can_work() {
                return;
            }
            if let Some(path) = ui.last_clip.borrow_mut().take() {
                if std::path::Path::new(&path).parent()
                    == Some(ui.paths.data.join("clips/onboarding").as_path())
                {
                    if let Err(e) = std::fs::remove_file(path) {
                        ui.error(&e.to_string());
                        return;
                    }
                    ui.practice_feedback
                        .set_text("Clip removed. Record this sentence again.");
                }
            }
        });
        clicked!(train, |ui: Rc<Self>| {
            if ui.can_work() {
                ui.busy.set(true);
                ui.status.set_text("Training…");
                let _ = ui.model.send(Command::Train);
            }
        });
        clicked!(finish_practice, |ui: Rc<Self>| {
            if ui.can_work() {
                ui.state.practice.store(false, Ordering::Relaxed);
                ui.sentences.borrow_mut().clear();
                ui.stack.set_visible_child_name("dictate");
            }
        });
        clicked!(refresh, |ui: Rc<Self>| ui.refresh_history());
        let weak = Rc::downgrade(&ui);
        ui.window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                if ui.background.get() {
                    ui.state.practice.store(false, Ordering::Relaxed);
                    ui.window.set_visible(false);
                    ui.notify(
                        "Lipflow is running",
                        "Use your shortcut to dictate. Open Lipflow again to show its window.",
                    );
                } else {
                    ui.app.quit();
                }
            }
            glib::Propagation::Stop
        });
        let weak = Rc::downgrade(&ui);
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(ui) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            for event in event_rx.try_iter().take(30) {
                ui.event(event);
            }
            glib::ControlFlow::Continue
        });
        ui.refresh_history();
        ui
    }

    fn preferences(self: &Rc<Self>, page: &gtk::Box) {
        let group = adw::PreferencesGroup::new();
        page.append(&group);
        for (title, key, value) in [
            (
                "Automatically paste at the cursor",
                "paste",
                self.settings.borrow().paste,
            ),
            (
                "Live transcription preview",
                "preview",
                self.settings.borrow().preview,
            ),
            (
                "Whisper mode (microphone and lips)",
                "whisper",
                self.settings.borrow().whisper,
            ),
            (
                "Keep recent mouth clips for accuracy checks",
                "clips",
                self.settings.borrow().save_clips,
            ),
            (
                "Start when I log in",
                "autostart",
                self.settings.borrow().autostart,
            ),
        ] {
            let row = adw::SwitchRow::builder().title(title).active(value).build();
            group.add(&row);
            self.setting_rows
                .borrow_mut()
                .insert(key.into(), row.clone());
            let weak = Rc::downgrade(self);
            row.connect_active_notify(move |row| {
                if let Some(ui) = weak.upgrade() {
                    if !ui.can_change() {
                        // Avoid changing recording/model settings halfway through work.
                        let original = match key {
                            "paste" => ui.settings.borrow().paste,
                            "preview" => ui.settings.borrow().preview,
                            "whisper" => ui.settings.borrow().whisper,
                            "clips" => ui.settings.borrow().save_clips,
                            _ => ui.settings.borrow().autostart,
                        };
                        if row.is_active() != original {
                            row.set_active(original);
                        }
                        return;
                    }
                    let enabled = row.is_active();
                    {
                        let mut s = ui.settings.borrow_mut();
                        match key {
                            "paste" => s.paste = enabled,
                            "preview" => s.preview = enabled,
                            "whisper" => s.whisper = enabled,
                            "clips" => s.save_clips = enabled,
                            _ => s.autostart = enabled,
                        }
                    }
                    ui.state
                        .preview_enabled
                        .store(ui.settings.borrow().preview, Ordering::Relaxed);
                    ui.save();
                    if key == "autostart"
                        || (key == "paste" && enabled && ui.paste.borrow().is_none())
                    {
                        ui.connect();
                    }
                    ui.configure();
                }
            });
        }
        let cleanups = ["auto", "basic", "ollama", "claude"];
        let cleanup = adw::ComboRow::builder()
            .title("Text cleanup")
            .model(&gtk::StringList::new(&[
                "Automatic",
                "Offline formatting",
                "Local Ollama",
                "Claude",
            ]))
            .build();
        cleanup.set_selected(
            cleanups
                .iter()
                .position(|s| *s == self.settings.borrow().cleanup)
                .unwrap_or(0) as u32,
        );
        group.add(&cleanup);
        *self.cleanup_row.borrow_mut() = Some(cleanup.clone());
        let weak = Rc::downgrade(self);
        cleanup.connect_selected_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                if !ui.can_change() {
                    let original = cleanups
                        .iter()
                        .position(|c| *c == ui.settings.borrow().cleanup)
                        .unwrap_or(0) as u32;
                    if row.selected() != original {
                        row.set_selected(original);
                    }
                    return;
                }
                ui.settings.borrow_mut().cleanup = cleanups[row.selected() as usize].into();
                ui.save();
                ui.configure();
            }
        });
        let beam = adw::ComboRow::builder()
            .title("Decoding")
            .model(&gtk::StringList::new(&[
                "Faster (beam 4)",
                "More accurate (beam 10)",
            ]))
            .build();
        beam.set_selected(u32::from(self.settings.borrow().beam >= 10));
        group.add(&beam);
        *self.beam_row.borrow_mut() = Some(beam.clone());
        let weak = Rc::downgrade(self);
        beam.connect_selected_notify(move |row| {
            if let Some(ui) = weak.upgrade() {
                if !ui.can_change() {
                    let original = u32::from(ui.settings.borrow().beam >= 10);
                    if row.selected() != original {
                        row.set_selected(original);
                    }
                    return;
                }
                ui.settings.borrow_mut().beam = if row.selected() == 0 { 4 } else { 10 };
                ui.save();
                ui.configure();
            }
        });
        page.append(&label("Whisper mode downloads an additional ~1.9 GB model. Offline formatting keeps cleanup on this machine; Ollama uses your local server. Claude sends candidate text to Anthropic and needs an API key in the environment."));
        page.append(&label("Custom words (one name or term per line):"));
        let words = text_view(true);
        words.set_vexpand(false);
        words.buffer().set_text(
            &std::fs::read_to_string(self.paths.data.join("words.txt")).unwrap_or_default(),
        );
        page.append(&scroll(&words));
        let save_words = gtk::Button::with_label("Save custom words");
        page.append(&save_words);
        let weak = Rc::downgrade(self);
        save_words.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                match config::atomic_write(
                    &ui.paths.data.join("words.txt"),
                    text(&words).as_bytes(),
                ) {
                    Ok(()) => ui.status.set_text("Custom words saved"),
                    Err(e) => ui.error(&e.to_string()),
                }
            }
        });
        let import = gtk::Button::with_label("Import my phrases from a text file…");
        page.append(&import);
        let weak = Rc::downgrade(self);
        import.connect_clicked(move |_| {
            if let Some(ui) = weak.upgrade() {
                if !ui.can_work() {
                    return;
                }
                glib::MainContext::default().spawn_local(async move {
                    let dialog = gtk::FileDialog::builder().title("Import phrases").build();
                    if let Ok(file) = dialog.open_future(Some(&ui.window)).await {
                        if let Some(path) = file.path() {
                            ui.busy.set(true);
                            let _ = ui.model.send(Command::Import(path));
                        }
                    }
                });
            }
        });
    }

    fn save(&self) {
        if let Err(e) = self.settings.borrow().save(&self.paths.data) {
            self.error(&e.to_string());
        }
    }
    fn configure(&self) {
        if self.ready.get() {
            self.busy.set(true);
        }
        let _ = self
            .model
            .send(Command::Configure(self.settings.borrow().clone()));
    }
    fn notify(&self, title: &str, body: &str) {
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_default_action("app.show");
        self.app
            .send_notification(Some("lipflow-status"), &notification);
    }
    fn error(&self, message: &str) {
        self.status.set_text(message);
        if !self.window.is_visible() {
            self.notify("Lipflow", message);
        }
    }
    fn can_change(&self) -> bool {
        !self.busy.get() && self.state.active.lock().unwrap().is_none()
    }
    fn can_work(&self) -> bool {
        self.ready.get() && self.can_change()
    }
    fn load(&self, download: bool) {
        if !self.can_change() {
            return;
        }
        self.busy.set(true);
        self.ready.set(false);
        self.status.set_text(if download {
            "Downloading models…"
        } else {
            "Loading models…"
        });
        let _ = self.model.send(Command::Load(download));
    }

    fn connect(self: &Rc<Self>) {
        if self.connecting.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let result = async {
                let portal = Portal::new()?;
                *ui.portal.borrow_mut() = Some(portal.clone());
                let requested_autostart = ui.settings.borrow().autostart;
                let (background, autostart) = portal.background(requested_autostart).await?;
                ui.background.set(background);
                ui.settings.borrow_mut().autostart = autostart;
                let busy = ui.busy.replace(true);
                if let Some(row) = ui.setting_rows.borrow().get("autostart") {
                    row.set_active(autostart);
                }
                ui.busy.set(busy);
                ui.save();
                if !background && !ui.window.is_visible() {
                    ui.window.present();
                }
                portal.camera_permission().await?;
                ui.camera_allowed.set(true);
                ui.shortcuts.borrow_mut().take();
                ui.ptt.borrow_mut().cancel();
                ui.cancel(true);
                let weak = Rc::downgrade(&ui);
                let closed = Rc::downgrade(&ui);
                let trigger = ui.settings.borrow().shortcut.clone();
                let (session, description) = portal
                    .shortcuts(
                        &trigger,
                        move |pressed, id| {
                            if let Some(ui) = weak.upgrade() {
                                if id == "cancel" {
                                    if pressed {
                                        ui.ptt.borrow_mut().cancel();
                                        ui.cancel(false);
                                    }
                                    return;
                                }
                                let action = {
                                    let mut ptt = ui.ptt.borrow_mut();
                                    if pressed {
                                        ptt.press(Instant::now())
                                    } else {
                                        ptt.release(Instant::now())
                                    }
                                };
                                if let Some(action) = action {
                                    match action {
                                        Action::Start(hands_free) => ui.start(hands_free),
                                        Action::Stop => ui.stop(),
                                        Action::Cancel(silent) => ui.cancel(silent),
                                    }
                                }
                            }
                        },
                        move || {
                            if let Some(ui) = closed.upgrade() {
                                ui.ptt.borrow_mut().cancel();
                                ui.cancel(true);
                                ui.error(
                                    "Shortcut permission closed. Reconnect desktop permissions.",
                                );
                            }
                        },
                    )
                    .await?;
                *ui.shortcuts.borrow_mut() = Some(session);
                // Background clipboard ownership uses a RemoteDesktop session even in
                // copy-only mode. GNOME owns this consent; copy-only never injects keys.
                {
                    ui.paste.borrow_mut().take();
                    let weak = Rc::downgrade(&ui);
                    let restore = ui.settings.borrow().restore_token.clone();
                    let (paste, token) = portal
                        .remote_paste(&restore, move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.error("Paste permission closed. Reconnect desktop permissions.");
                            }
                        })
                        .await?;
                    *ui.paste.borrow_mut() = Some(Rc::new(paste));
                    ui.settings.borrow_mut().restore_token = token;
                }
                ui.settings.borrow_mut().permissions_configured = true;
                ui.save();
                ui.permission_status.set_text(&format!(
                    "Connected · {description} · cancel: Ctrl+Alt+Escape"
                ));
                Ok::<_, anyhow::Error>(())
            }
            .await;
            ui.connecting.set(false);
            if let Err(e) = result {
                ui.permission_status.set_text(&format!("{e:#}"));
                ui.error(&format!("Desktop permissions: {e:#}"));
                if !ui.window.is_visible() {
                    ui.window.present();
                }
            }
        });
    }

    fn toggle_record(self: &Rc<Self>) {
        if self.state.active.lock().unwrap().is_some() {
            self.stop();
        } else {
            self.start(true);
        }
    }
    fn start(self: &Rc<Self>, hands_free: bool) {
        if self.state.active.lock().unwrap().is_some() {
            if hands_free {
                self.status
                    .set_text("Hands-free recording · tap the shortcut to finish");
            }
            return;
        }
        if !self.can_work() {
            self.error("Wait for the model to finish loading or decoding");
            return;
        }
        if !self.camera_allowed.get() {
            self.error("Connect desktop permissions in Preferences first");
            return;
        }
        let record = match engine::new_record() {
            Ok(r) => r,
            Err(e) => {
                self.error(&format!("{e:#}"));
                return;
            }
        };
        let token = self.generation.get() + 1;
        self.generation.set(token);
        self.state.generation.store(token, Ordering::SeqCst);
        self.stopping.set(false);
        let practice = if self.state.practice.load(Ordering::Relaxed) {
            self.sentences
                .borrow()
                .get(self.practice_index.get())
                .cloned()
        } else {
            None
        };
        *self.state.active.lock().unwrap() = Some(Active {
            token,
            record,
            started: Instant::now(),
            practice: practice.clone(),
        });
        self.record_button.set_label("Finish");
        self.status.set_text(if hands_free {
            "Recording · tap the shortcut to finish"
        } else {
            "Recording · release the shortcut to finish"
        });
        if practice.is_none() && self.settings.borrow().paste && self.background.get() {
            self.window.set_visible(false);
        }
        if !self.window.is_visible() {
            self.notify(
                "Recording",
                "Mouth your words. Release the shortcut to finish; Ctrl+Alt+Escape cancels.",
            );
        }
        let ui = self.clone();
        let portal = self.portal.borrow().clone().unwrap();
        glib::MainContext::default().spawn_local(async move {
            let fd = portal.camera_fd().await;
            if !ui
                .state
                .active
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|a| a.token == token)
            {
                return;
            }
            match fd {
                Ok(fd) => {
                    if practice.is_none() && ui.settings.borrow().whisper {
                        let _ = ui.model.send(Command::MicStart);
                    }
                    let _ = ui
                        .capture
                        .send(Control::Open(fd, ui.settings.borrow().camera.clone()));
                }
                Err(e) => {
                    ui.cancel(true);
                    ui.error(&format!("Camera permission: {e:#}"));
                }
            }
        });
    }
    fn stop(self: &Rc<Self>) {
        if self.stopping.replace(true) {
            return;
        }
        let token = self.generation.get();
        let ui = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            ui.stopping.set(false);
            if token != ui.generation.get() {
                return;
            }
            let active = ui.state.active.lock().unwrap().take();
            if let Some(active) = active {
                ui.busy.set(true);
                ui.pending.set(Some(active.token));
                ui.status.set_text("Reading your lips…");
                ui.record_button.set_label("Record");
                let _ = ui.model.send(Command::MicStop(Some(active.record.clone())));
                let _ = ui.model.send(Command::Finish(
                    active.token,
                    active.record,
                    active.practice,
                ));
            }
        });
    }
    fn cancel(&self, silent: bool) {
        self.generation.set(self.generation.get() + 1);
        self.state
            .generation
            .store(self.generation.get(), Ordering::SeqCst);
        self.stopping.set(false);
        self.state.active.lock().unwrap().take();
        let _ = self.model.send(Command::MicStop(None));
        self.record_button.set_label("Record");
        if !silent {
            self.status.set_text("Cancelled");
        }
    }
    fn settle(&self, token: u64) -> bool {
        if self.pending.get() == Some(token) {
            self.pending.set(None);
            self.busy.set(false);
        }
        token == self.generation.get()
    }
    fn event(self: &Rc<Self>, event: Event) {
        match event {
            Event::SettingsRejected(settings, message) => {
                self.busy.set(true); // Restoring widgets must not schedule more model work.
                *self.settings.borrow_mut() = settings.clone();
                self.save();
                for (key, row) in self.setting_rows.borrow().iter() {
                    row.set_active(match key.as_str() {
                        "paste" => settings.paste,
                        "preview" => settings.preview,
                        "whisper" => settings.whisper,
                        "clips" => settings.save_clips,
                        _ => settings.autostart,
                    });
                }
                if let Some(row) = self.cleanup_row.borrow().as_ref() {
                    row.set_selected(
                        ["auto", "basic", "ollama", "claude"]
                            .iter()
                            .position(|c| *c == settings.cleanup)
                            .unwrap_or(0) as u32,
                    );
                }
                if let Some(row) = self.beam_row.borrow().as_ref() {
                    row.set_selected(u32::from(settings.beam >= 10));
                }
                self.state
                    .preview_enabled
                    .store(settings.preview, Ordering::Relaxed);
                self.busy.set(false);
                self.progress.set_visible(false);
                self.error(&format!("Settings restored: {message}"));
            }
            Event::Ready(description) => {
                self.ready.set(true);
                self.busy.set(false);
                self.progress.set_visible(false);
                self.download_button.set_visible(false);
                self.status.set_text(&description);
            }
            Event::Progress(message, percent) => {
                self.status.set_text(&message);
                self.progress.set_visible(true);
                self.progress
                    .set_fraction((percent / 100.0).clamp(0.0, 1.0));
            }
            Event::Error(message) => {
                self.cancel(true);
                self.busy.set(false);
                self.pending.set(None);
                self.progress.set_visible(false);
                self.error(&message);
            }
            Event::Failure(token, message) => {
                if self.settle(token) {
                    self.error(&message);
                }
            }
            Event::Preview(token, preview) => {
                if token == self.generation.get() {
                    self.output.buffer().set_text(&preview);
                }
            }
            Event::Frame(bytes, width, height) => {
                self.state.frame_pending.store(false, Ordering::Relaxed);
                let texture = gtk::gdk::MemoryTexture::new(
                    width as i32,
                    height as i32,
                    gtk::gdk::MemoryFormat::R8g8b8,
                    &glib::Bytes::from_owned(bytes),
                    width * 3,
                );
                self.preview.set_paintable(Some(&texture));
                self.practice_preview.set_paintable(Some(&texture));
            }
            Event::Result(token, result) => {
                if !self.settle(token) {
                    return;
                }
                self.output.buffer().set_text(&result);
                self.refresh_history();
                self.status.set_text("Result saved");
                if self.window.is_active() {
                    self.window.clipboard().set_text(&result);
                    self.status
                        .set_text("Copied while Lipflow is focused · result saved in history");
                    return;
                }
                if self.settings.borrow().paste {
                    if let Some(paste) = self.paste.borrow().clone() {
                        let ui = self.clone();
                        ui.busy.set(true);
                        let join = self
                            .last_paste
                            .get()
                            .is_some_and(|at| at.elapsed() < Duration::from_secs(45));
                        let payload = if join {
                            format!(" {result}")
                        } else {
                            result.clone()
                        };
                        glib::MainContext::default().spawn_local(async move {
                            let outcome = paste
                                .send_if(&payload, true, || token == ui.generation.get())
                                .await;
                            ui.busy.set(false);
                            if token != ui.generation.get() {
                                return;
                            }
                            match outcome {
                                Ok(()) => {
                                    ui.last_paste.set(Some(Instant::now()));
                                    ui.status.set_text("Pasted · result saved in history");
                                }
                                Err(e) => ui.error(&format!("Paste: {e:#}")),
                            }
                        });
                    } else {
                        self.error("Result saved. Connect paste permission in Preferences or use Copy result.");
                    }
                } else if self.window.is_visible() {
                    self.window.clipboard().set_text(&result);
                    self.status.set_text("Copied · result saved in history");
                } else if let Some(paste) = self.paste.borrow().clone() {
                    let ui = self.clone();
                    glib::MainContext::default().spawn_local(async move {
                        match paste.send(&result, false).await {
                            Ok(()) => ui.status.set_text("Copied · result saved in history"),
                            Err(e) => ui.error(&format!("Clipboard: {e:#}")),
                        }
                    });
                } else {
                    self.notify(
                        "Dictation complete",
                        "Your result is saved in history. Open Lipflow to copy it.",
                    );
                }
            }
            Event::Sentences(sentences) => {
                self.busy.set(false);
                *self.sentences.borrow_mut() = sentences;
                self.practice_index.set(0);
                self.state.practice.store(true, Ordering::Relaxed);
                self.show_sentence();
                self.stack.set_visible_child_name("practice");
            }
            Event::Practice(token, raw, path) => {
                if self.settle(token) {
                    *self.last_clip.borrow_mut() = Some(path);
                    self.practice_feedback.set_text(&format!(
                        "Saved. Model read: {raw}. Choose Next when ready."
                    ));
                }
            }
            Event::Trained(report) => {
                self.busy.set(false);
                self.progress.set_visible(false);
                self.status.set_text(&report);
                self.practice_feedback.set_text(&report);
            }
            Event::Imported(phrases, words) => {
                self.busy.set(false);
                self.status
                    .set_text(&format!("Imported {phrases} phrases ({words} words)"));
            }
            Event::Stopped(token) => {
                if token == self.generation.get() {
                    self.stop();
                }
            }
        }
    }
    fn show_sentence(&self) {
        let sentences = self.sentences.borrow();
        if let Some(sentence) = sentences.get(self.practice_index.get()) {
            self.sentence.set_text(&format!(
                "{} / {}: {sentence}",
                self.practice_index.get() + 1,
                sentences.len()
            ));
        } else {
            self.sentence.set_text(
                "Practice complete. Train on your face when you have at least 12 saved clips.",
            );
            self.state.practice.store(false, Ordering::Relaxed);
        }
        self.practice_feedback.set_text("");
        self.last_clip.borrow_mut().take();
    }
    fn next_sentence(&self) {
        if !self.sentences.borrow().is_empty() {
            self.practice_index.set(self.practice_index.get() + 1);
            self.show_sentence();
        }
    }
    fn refresh_history(&self) {
        while let Some(child) = self.history.first_child() {
            self.history.remove(&child);
        }
        let contents =
            std::fs::read_to_string(self.paths.data.join("history.jsonl")).unwrap_or_default();
        let mut count = 0;
        for line in contents.lines().rev().take(100) {
            let Ok(item) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(result) = item["text"].as_str() else {
                continue;
            };
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let content = label(&format!("{}\n{result}", item["at"].as_str().unwrap_or("")));
            content.set_hexpand(true);
            row.append(&content);
            let copy = button("Copy", &row);
            let result = result.to_owned();
            let window = self.window.clone();
            copy.connect_clicked(move |_| window.clipboard().set_text(&result));
            self.history.append(&row);
            count += 1;
        }
        if count == 0 {
            self.history
                .append(&label("Your completed dictations will appear here."));
        }
    }
}

fn record_button_ref(ui: &Ui) -> &gtk::Button {
    &ui.record_button
}
