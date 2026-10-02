use crate::{
    capture::CaptureState,
    config::{Paths, Settings},
    engine::{self, Engine, Event, Record},
    models,
};
use anyhow::{bail, Result};
use std::{
    path::PathBuf,
    sync::{
        atomic::Ordering,
        mpsc::{Receiver, Sender},
        Arc,
    },
};

pub enum Command {
    Load(bool),
    Configure(Settings),
    MicStart,
    MicStop(Option<Record>),
    Preview(u64, Record),
    Finish(u64, Record, Option<String>),
    Practice,
    Train,
    Import(PathBuf),
    Shutdown,
}

pub fn run(
    paths: Paths,
    mut settings: Settings,
    state: Arc<CaptureState>,
    commands: Receiver<Command>,
    events: Sender<Event>,
) {
    let mut engine: Option<Engine> = None;
    while let Ok(command) = commands.recv() {
        if matches!(command, Command::Shutdown) {
            return;
        }
        let result = (|| -> Result<()> {
            match command {
                Command::Load(download) => {
                    if download {
                        models::download(&paths, settings.whisper, false, |name, percent| {
                            let _ = events
                                .send(Event::Progress(format!("Downloading {name}"), percent));
                        })?;
                    }
                    let missing = models::missing(&paths.models, settings.whisper);
                    if !missing.is_empty() {
                        bail!("Download the lip-reading models to get started (~1.2 GB, research-use weights).");
                    }
                    engine::initialize(&paths, true)?;
                    let (mut loaded, description) = Engine::load(&settings)?;
                    if settings.whisper {
                        loaded.load_whisper(settings.beam)?;
                    }
                    engine = Some(loaded);
                    let _ = events.send(Event::Ready(description));
                }
                Command::Configure(new) => {
                    let reload = new.beam != settings.beam;
                    let whisper = engine.is_some() && new.whisper && (!settings.whisper || reload);
                    let configured = (|| -> Result<()> {
                        if whisper {
                            models::download(&paths, true, false, |name, percent| {
                                let _ = events
                                    .send(Event::Progress(format!("Downloading {name}"), percent));
                            })?;
                        }
                        if let Some(engine) = &mut engine {
                            if reload {
                                let mut replacement = Engine::load(&new)?.0;
                                if new.whisper {
                                    replacement.load_whisper(new.beam)?;
                                }
                                *engine = replacement;
                            } else {
                                if whisper {
                                    engine.load_whisper(new.beam)?;
                                }
                                engine.configure(&new)?;
                                if !new.whisper {
                                    engine.disable_whisper();
                                }
                            }
                        }
                        Ok(())
                    })();
                    if let Err(error) = configured {
                        let _ = events.send(Event::SettingsRejected(
                            settings.clone(),
                            format!("{error:#}"),
                        ));
                        return Ok(());
                    }
                    settings = new;
                    if engine.is_some() {
                        let _ = events.send(Event::Ready("Settings updated".into()));
                    }
                }
                Command::MicStart => {
                    if settings.whisper {
                        engine
                            .as_ref()
                            .ok_or_else(|| anyhow::anyhow!("Still loading"))?
                            .mic_start()?;
                    }
                }
                Command::MicStop(rec) => {
                    if let Some(engine) = &engine {
                        engine.mic_stop(rec.as_ref())?;
                    }
                }
                Command::Preview(token, rec) => {
                    let active = state
                        .active
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|a| a.token == token);
                    if active {
                        if let Some(engine) = &engine {
                            if let Ok(text) = engine.preview(&rec) {
                                let _ = events.send(Event::Preview(token, text.to_lowercase()));
                            }
                        }
                    }
                    state.preview_pending.store(false, Ordering::Relaxed);
                }
                Command::Finish(token, rec, practice) => {
                    if state.generation.load(Ordering::SeqCst) != token {
                        let _ = events.send(Event::Failure(token, "Cancelled".into()));
                        return Ok(());
                    }
                    if let Some(engine) = &mut engine {
                        match engine.finish(
                            token,
                            &rec,
                            &settings,
                            practice.as_deref(),
                            &state.generation,
                        ) {
                            Ok(event) => {
                                let _ = events.send(event);
                            }
                            Err(error) => {
                                let _ = events.send(Event::Failure(token, format!("{error:#}")));
                            }
                        }
                    }
                }
                Command::Practice => {
                    let _ = events.send(Event::Sentences(engine::sentences()?));
                }
                Command::Train => {
                    let message = engine
                        .as_mut()
                        .ok_or_else(|| anyhow::anyhow!("Load the model first"))?
                        .train(&settings, &events)?;
                    if settings.whisper {
                        engine.as_mut().unwrap().load_whisper(settings.beam)?;
                    }
                    let _ = events.send(Event::Trained(message));
                }
                Command::Import(path) => {
                    let (phrases, words) = engine::import_text(&path)?;
                    if let Some(engine) = &mut engine {
                        engine.configure(&settings)?;
                    }
                    let _ = events.send(Event::Imported(phrases, words));
                }
                Command::Shutdown => unreachable!(),
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = events.send(Event::Error(format!("{error:#}")));
        }
    }
}
