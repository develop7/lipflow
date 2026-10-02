use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const APP_ID: &str = "io.github.develop7.Lipflow";

#[derive(Clone)]
pub struct Paths {
    pub data: PathBuf,
    pub models: PathBuf,
    pub engine: PathBuf,
    pub python_site: Option<PathBuf>,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        let xdg = |name: &str, suffix: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&home).join(suffix))
        };
        let data = std::env::var_os("LIPFLOW_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_DATA_HOME", ".local/share").join("lipflow"));
        let engine = std::env::var_os("LIPFLOW_ENGINE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let installed = PathBuf::from("/app/share/lipflow/engine");
                if installed.join("lipflow/vsr.py").is_file() {
                    installed
                } else {
                    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .parent()
                        .unwrap()
                        .to_path_buf()
                }
            });
        let models = std::env::var_os("LIPFLOW_MODELS")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                if engine.join("models").is_dir() && !engine.starts_with("/app") {
                    engine.join("models")
                } else {
                    xdg("XDG_CACHE_HOME", ".cache").join("lipflow/models")
                }
            });
        let python_site = std::env::var_os("LIPFLOW_PYTHON_SITE_PACKAGES")
            .map(PathBuf::from)
            .or_else(|| {
                let path = engine.join(".venv/lib/python3.12/site-packages");
                path.is_dir().then_some(path)
            });
        Ok(Self {
            data,
            models,
            engine,
            python_site,
        })
    }

    pub fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.data)?;
        std::fs::create_dir_all(&self.models)?;
        // Configure the unchanged Python engine before starting any threads/interpreter.
        std::env::set_var("LIPFLOW_HOME", &self.data);
        std::env::set_var("GLOG_minloglevel", "2");
        std::env::set_var("TF_CPP_MIN_LOG_LEVEL", "3");
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub shortcut: String,
    pub cleanup: String,
    pub paste: bool,
    pub preview: bool,
    pub whisper: bool,
    pub save_clips: bool,
    pub autostart: bool,
    pub camera: String,
    pub beam: usize,
    pub restore_token: String,
    pub permissions_configured: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shortcut: "CTRL+ALT+space".into(),
            cleanup: "auto".into(),
            paste: true,
            preview: true,
            whisper: false,
            save_clips: true,
            autostart: false,
            camera: "auto".into(),
            beam: 4,
            restore_token: String::new(),
            permissions_configured: false,
        }
    }
}

impl Settings {
    pub fn load(data: &Path) -> Result<Self> {
        let path = data.join("linux-settings.json");
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Could not read Linux settings"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn save(&self, data: &Path) -> Result<()> {
        atomic_write(
            &data.join("linux-settings.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().context("File has no parent directory")?)?;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn older_settings_get_defaults_without_losing_preferences() {
        let settings: Settings =
            serde_json::from_str(r#"{"whisper":true,"shortcut":"SUPER+F8"}"#).unwrap();
        assert!(settings.whisper && settings.paste);
        assert_eq!(settings.shortcut, "SUPER+F8");
        assert_eq!(settings.beam, 4);
    }
}
