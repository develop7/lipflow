//! The existing ML engine is reused through PyO3; no new Python implementation is shipped.
use crate::config::{Paths, Settings};
use anyhow::{Context, Result};
use pyo3::{
    prelude::*,
    types::{PyBytes, PyDict, PyModule, PyTuple},
};
use std::{
    sync::{mpsc::Sender, Arc},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub type Record = Arc<Py<PyAny>>;

#[derive(Debug)]
pub enum Event {
    Ready(String),
    Progress(String, f64),
    Error(String),
    SettingsRejected(Settings, String),
    Failure(u64, String),
    Preview(u64, String),
    Frame(Vec<u8>, usize, usize),
    Result(u64, String),
    Practice(u64, String, String),
    Sentences(Vec<String>),
    Trained(String),
    Imported(usize, usize),
    Stopped(u64),
}

pub fn initialize(paths: &Paths, log: bool) -> Result<()> {
    Python::attach(|py| -> PyResult<()> {
        let sys = PyModule::import(py, "sys")?;
        let search = sys.getattr("path")?;
        if let Some(site) = &paths.python_site {
            search.call_method1("insert", (0, site.to_string_lossy().as_ref()))?;
        }
        search.call_method1("insert", (0, paths.engine.to_string_lossy().as_ref()))?;
        // The original source locates models beside the package. Redirect its runtime
        // constants/default to writable storage without changing its implementation.
        PyModule::import(py, "lipflow.vsr")?
            .setattr("MODELS", paths.models.to_string_lossy().as_ref())?;
        let face = PyModule::import(py, "lipflow.face")?;
        let model = paths
            .models
            .join("face_landmarker.task")
            .to_string_lossy()
            .into_owned();
        face.setattr("DEFAULT_MODEL", &model)?;
        face.getattr("FaceTracker")?
            .getattr("__init__")?
            .setattr("__defaults__", PyTuple::new(py, [&model])?)?;
        if log {
            let file = PyModule::import(py, "builtins")?.getattr("open")?.call1((
                paths.data.join("Lipflow.log").to_string_lossy().as_ref(),
                "a",
                1,
            ))?;
            sys.setattr("stdout", &file)?;
            sys.setattr("stderr", file)?;
        }
        Ok(())
    })
    .context(
        "Could not initialize the Python ML engine; install the locked Python 3.12 dependencies",
    )
}

pub fn new_record() -> Result<Record> {
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64();
    Ok(Arc::new(Python::attach(|py| -> PyResult<_> {
        Ok(PyModule::import(py, "lipflow.camera")?
            .getattr("Recording")?
            .call1((started,))?
            .unbind())
    })?))
}

pub struct Tracker {
    tracker: Py<PyAny>,
}
impl Tracker {
    pub fn new(paths: &Paths) -> Result<Self> {
        Ok(Self {
            tracker: Python::attach(|py| -> PyResult<_> {
                Ok(PyModule::import(py, "lipflow.face")?
                    .getattr("FaceTracker")?
                    .call1((paths
                        .models
                        .join("face_landmarker.task")
                        .to_string_lossy()
                        .as_ref(),))?
                    .unbind())
            })?,
        })
    }
    pub fn frame(
        &self,
        bgr: &[u8],
        width: usize,
        height: usize,
        rec: Option<&Record>,
        thumbnail: bool,
    ) -> Result<Option<Vec<u8>>> {
        Python::attach(|py| -> PyResult<_> {
            let np = PyModule::import(py, "numpy")?;
            let cv = PyModule::import(py, "cv2")?;
            let data = PyBytes::new(py, bgr);
            let frame = np
                .getattr("frombuffer")?
                .call1((data, np.getattr("uint8")?))?
                .call_method1("reshape", ((height, width, 3),))?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();
            let obs = self
                .tracker
                .bind(py)
                .call_method1("detect", (&frame, (now * 1000.0) as u64))?;
            let camera = PyModule::import(py, "lipflow.camera")?;
            if let Some(rec) = rec {
                let record = rec.bind(py);
                let gray = cv
                    .getattr("cvtColor")?
                    .call1((&frame, cv.getattr("COLOR_BGR2GRAY")?))?;
                let crop = camera.getattr("face_crop")?.call1((gray, &obs, record))?;
                record.getattr("ts")?.call_method1("append", (now,))?;
                record.getattr("grays")?.call_method1("append", (crop,))?;
                record.getattr("anchors")?.call_method1(
                    "append",
                    (if obs.is_none() {
                        py.None()
                    } else {
                        obs.getattr("anchors")?.unbind()
                    },),
                )?;
                record.getattr("mouth_open")?.call_method1(
                    "append",
                    (if obs.is_none() {
                        0.0
                    } else {
                        obs.getattr("mouth_open")?.extract::<f64>()?
                    },),
                )?;
            }
            if thumbnail {
                let view = camera
                    .getattr("mouth_view")?
                    .call1((frame, obs, 320, 200))?;
                let rgb = cv
                    .getattr("cvtColor")?
                    .call1((view, cv.getattr("COLOR_BGR2RGB")?))?;
                return Ok(Some(rgb.call_method0("tobytes")?.extract::<Vec<u8>>()?));
            }
            Ok(None)
        })
        .map_err(Into::into)
    }
}
impl Drop for Tracker {
    fn drop(&mut self) {
        Python::attach(|py| {
            let _ = self.tracker.bind(py).call_method0("close");
        });
    }
}

#[pyclass]
struct Progress {
    events: Sender<Event>,
}
#[pymethods]
impl Progress {
    fn __call__(&self, percent: f64, text: String) {
        let _ = self.events.send(Event::Progress(text, percent));
    }
}

pub struct Engine {
    pub reader: Py<PyAny>,
    cleaner: Py<PyAny>,
    av: Option<Py<PyAny>>,
    mic: Py<PyAny>,
    context: Vec<String>,
}

impl Engine {
    pub fn disable_whisper(&mut self) {
        self.av = None;
    }
    pub fn load(settings: &Settings) -> Result<(Self, String)> {
        Python::attach(|py| -> PyResult<_> {
            let kwargs = PyDict::new(py);
            kwargs.set_item("beam_size", settings.beam)?;
            let reader = PyModule::import(py, "lipflow.vsr")?
                .getattr("LipReader")?
                .call((), Some(&kwargs))?;
            reader.call_method0("warmup")?;
            let backend = reader.getattr("enc_device")?.str()?.to_string();
            let cleaner = PyModule::import(py, "lipflow.cleanup")?
                .getattr("Cleaner")?
                .call1((&settings.cleanup,))?;
            let description = cleaner.call_method0("describe")?.extract::<String>()?;
            let mic = PyModule::import(py, "lipflow.mic")?
                .getattr("Mic")?
                .call0()?;
            Ok((
                Self {
                    reader: reader.unbind(),
                    cleaner: cleaner.unbind(),
                    av: None,
                    mic: mic.unbind(),
                    context: Vec::new(),
                },
                format!("Ready · {backend} · {description}"),
            ))
        })
        .map_err(Into::into)
    }
    pub fn configure(&mut self, settings: &Settings) -> Result<()> {
        self.cleaner = Python::attach(|py| -> PyResult<_> {
            Ok(PyModule::import(py, "lipflow.cleanup")?
                .getattr("Cleaner")?
                .call1((&settings.cleanup,))?
                .unbind())
        })?;
        Ok(())
    }
    pub fn load_whisper(&mut self, beam: usize) -> Result<()> {
        self.av = Some(Python::attach(|py| -> PyResult<_> {
            let kwargs = PyDict::new(py);
            kwargs.set_item("beam_size", beam)?;
            let reader = PyModule::import(py, "lipflow.av")?
                .getattr("AVReader")?
                .call((), Some(&kwargs))?;
            reader.call_method0("warmup_av")?;
            Ok(reader.unbind())
        })?);
        Ok(())
    }
    pub fn mic_start(&self) -> Result<()> {
        Python::attach(|py| -> PyResult<()> {
            self.mic.bind(py).call_method0("start")?;
            if let Some(error) = self
                .mic
                .bind(py)
                .getattr("error")?
                .extract::<Option<String>>()?
            {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(error));
            }
            Ok(())
        })
        .map_err(Into::into)
    }
    pub fn mic_stop(&self, rec: Option<&Record>) -> Result<()> {
        Python::attach(|py| -> PyResult<()> {
            let audio = self.mic.bind(py).call_method0("stop")?;
            if let Some(rec) = rec {
                rec.bind(py).setattr("audio", audio)?;
            }
            Ok(())
        })
        .map_err(Into::into)
    }
    pub fn preview(&self, record: &Record) -> Result<String> {
        Python::attach(|py| -> PyResult<_> {
            let rois = PyModule::import(py, "lipflow.dictation")?
                .getattr("rois_for")?
                .call1((record.bind(py),))?;
            if rois.is_none() {
                return Ok("Looking for your face…".into());
            }
            let enc = self.reader.bind(py).call_method1("encode", (rois,))?;
            self.reader
                .bind(py)
                .call_method1("greedy", (enc,))?
                .extract()
        })
        .map_err(Into::into)
    }
    pub fn finish(
        &mut self,
        token: u64,
        record: &Record,
        settings: &Settings,
        practice: Option<&str>,
        generation: &std::sync::atomic::AtomicU64,
    ) -> Result<Event> {
        let started = Instant::now();
        Python::attach(|py| -> PyResult<_> {
            let dictation = PyModule::import(py, "lipflow.dictation")?;
            let rec = record.bind(py);
            if let Some((title, advice)) = dictation
                .getattr("clip_problem")?
                .call1((rec,))?
                .extract::<Option<(String, String)>>()?
            {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                    "{title}. {advice}"
                )));
            }
            let rois = dictation.getattr("rois_for")?.call1((rec,))?;
            let enc = self.reader.bind(py).call_method1("encode", (&rois,))?;
            if let Some(sentence) = practice {
                let raw = self
                    .reader
                    .bind(py)
                    .call_method1("greedy", (enc,))?
                    .extract::<String>()?;
                if generation.load(std::sync::atomic::Ordering::SeqCst) != token {
                    return Err(pyo3::exceptions::PyRuntimeError::new_err("Cancelled"));
                }
                let path = PyModule::import(py, "lipflow.practice")?
                    .getattr("save_clip")?
                    .call1((&rois, sentence, &raw))?
                    .extract()?;
                return Ok(Event::Practice(token, raw, path));
            }
            let nbest = PyDict::new(py);
            nbest.set_item("nbest", 5)?;
            let mut candidates: Option<Vec<String>> = None;
            if settings.whisper && self.av.is_some() {
                if let Ok(audio) = rec.getattr("audio") {
                    let ts = rec.getattr("ts")?.get_item(0)?;
                    let wave = PyModule::import(py, "lipflow.mic")?
                        .getattr("segment")?
                        .call1((audio, ts, rois.len()?))?;
                    if !wave.is_none() {
                        let av = self.av.as_ref().unwrap().bind(py);
                        let encoded = av.call_method1("encode_av", (&rois, wave))?;
                        candidates = Some(
                            av.call_method("beam_search", (encoded,), Some(&nbest))?
                                .extract()?,
                        );
                    }
                }
            }
            let candidates = match candidates.filter(|c| c.first().is_some_and(|t| !t.is_empty())) {
                Some(c) => c,
                None => self
                    .reader
                    .bind(py)
                    .call_method("beam_search", (enc,), Some(&nbest))?
                    .extract()?,
            };
            let kwargs = PyDict::new(py);
            kwargs.set_item(
                "context",
                self.context
                    .iter()
                    .rev()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" "),
            )?;
            if generation.load(std::sync::atomic::Ordering::SeqCst) != token {
                return Err(pyo3::exceptions::PyRuntimeError::new_err("Cancelled"));
            }
            let text = self
                .cleaner
                .bind(py)
                .call((candidates.clone(),), Some(&kwargs))?
                .extract::<String>()?;
            if text.is_empty() {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "Could not read that; try again a little slower",
                ));
            }
            if generation.load(std::sync::atomic::Ordering::SeqCst) != token {
                return Err(pyo3::exceptions::PyRuntimeError::new_err("Cancelled"));
            }
            self.context.push(text.clone());
            if self.context.len() > 3 {
                self.context.remove(0);
            }
            let cleanup = self.cleaner.bind(py).call_method0("describe")?;
            dictation.getattr("log_history")?.call1((
                rec,
                candidates.clone(),
                &text,
                started.elapsed().as_secs_f64(),
                cleanup,
            ))?;
            let settings_dict = PyDict::new(py);
            settings_dict.set_item("save_clips", settings.save_clips)?;
            dictation
                .getattr("keep_clip")?
                .call1((rois, candidates, &text, settings_dict))?;
            Ok(Event::Result(token, text))
        })
        .map_err(Into::into)
    }
    pub fn train(&mut self, settings: &Settings, events: &Sender<Event>) -> Result<String> {
        let result = Python::attach(|py| -> PyResult<_> {
            let callback = Py::new(
                py,
                Progress {
                    events: events.clone(),
                },
            )?;
            PyModule::import(py, "lipflow.dictation")?
                .getattr("train_on_face")?
                .call1((settings.beam, callback))?
                .unbind()
                .extract::<serde_json_bridge::Training>(py)
        })?;
        if let Some(after) = result.after {
            let (engine, _) = Self::load(settings)?;
            *self = engine;
            Ok(format!(
                "Held-out word error: {:.1}% → {:.1}%. {} {}",
                100.0 * result.before,
                100.0 * after,
                if result.kept {
                    "Improved model kept."
                } else {
                    "Standard model kept."
                },
                result.note
            ))
        } else {
            Ok(result.note)
        }
    }
}

mod serde_json_bridge {
    use pyo3::prelude::*;
    #[derive(FromPyObject)]
    pub struct Training {
        #[pyo3(item)]
        pub before: f64,
        #[pyo3(item)]
        pub after: Option<f64>,
        #[pyo3(item)]
        pub kept: bool,
        #[pyo3(item)]
        pub note: String,
    }
}

pub fn sentences() -> Result<Vec<String>> {
    Ok(Python::attach(|py| -> PyResult<_> {
        PyModule::import(py, "lipflow.practice")?
            .getattr("practice_sentences")?
            .call1((24,))?
            .extract()
    })?)
}
pub fn import_text(path: &std::path::Path) -> Result<(usize, usize)> {
    let text = std::fs::read_to_string(path)?;
    Ok(Python::attach(|py| -> PyResult<_> {
        let result = PyModule::import(py, "lipflow.personal")?
            .getattr("save_phrases")?
            .call1((text.lines().collect::<Vec<_>>(),))?;
        Ok((
            result.get_item("phrases")?.extract()?,
            result.get_item("words")?.extract()?,
        ))
    })?)
}
pub fn transcribe(
    paths: &Paths,
    video: &str,
    start: f64,
    end: Option<f64>,
    cleanup: &str,
) -> Result<String> {
    initialize(paths, false)?;
    let settings = Settings {
        beam: 10,
        cleanup: cleanup.into(),
        ..Settings::default()
    };
    let (engine, _) = Engine::load(&settings)?;
    Ok(Python::attach(|py| -> PyResult<_> {
        let raw = PyModule::import(py, "lipflow.offline")?
            .getattr("transcribe_file")?
            .call1((video, engine.reader.bind(py), start, end))?
            .extract::<String>()?;
        if cleanup == "none" {
            return Ok(raw);
        }
        engine.cleaner.bind(py).call1((vec![raw],))?.extract()
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rust_bridge_exercises_existing_tokenizer_tensor_and_cleanup() {
        let paths = Paths::discover().unwrap();
        paths.prepare().unwrap();
        initialize(&paths, false).unwrap();
        Python::attach(|py| -> PyResult<()> {
            let reader = PyModule::import(py, "lipflow.vsr")?.getattr("LipReader")?;
            let np = PyModule::import(py, "numpy")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("dtype", np.getattr("uint8")?)?;
            let rois = np.getattr("zeros")?.call(((30, 96, 96),), Some(&kwargs))?;
            let tensor = reader.getattr("to_tensor")?.call1((rois,))?;
            assert_eq!(
                tensor.getattr("shape")?.extract::<Vec<usize>>()?,
                vec![1, 30, 88, 88]
            );
            let timestamps = (0..90).map(|i| i as f64 / 30.0).collect::<Vec<_>>();
            let frames = reader
                .getattr("resample")?
                .call1((timestamps, 90))?
                .extract::<Vec<usize>>()?;
            assert_eq!(frames.len(), 75);
            let cleanup = PyModule::import(py, "lipflow.cleanup")?
                .getattr("basic_cleanup")?
                .call1(("I THINK I'LL GO",))?
                .extract::<String>()?;
            assert_eq!(cleanup, "I think I'll go.");
            Ok(())
        })
        .unwrap();
    }
}
