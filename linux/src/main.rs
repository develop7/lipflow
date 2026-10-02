mod capture;
mod config;
mod engine;
mod models;
mod packaging;
#[cfg(test)]
mod portal_tests;
mod portals;
mod ptt;
mod ui;
mod worker;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "lipflow",
    version,
    about = "Silent dictation for GNOME Wayland"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}
#[derive(Subcommand)]
enum Commands {
    /// Open the native window, or run behind your other applications.
    Run {
        #[arg(long)]
        background: bool,
        #[arg(long)]
        copy_only: bool,
        #[arg(long)]
        key: Option<String>,
        #[arg(long, hide = true)]
        smoke_test: bool,
    },
    /// Download the research-use models into the model cache.
    DownloadModels {
        #[arg(long)]
        whisper: bool,
        #[arg(long)]
        samples: bool,
    },
    /// Read a video file using the shared ML engine.
    File {
        video: PathBuf,
        #[arg(long, default_value_t = 0.0)]
        start: f64,
        #[arg(long)]
        end: Option<f64>,
        #[arg(long, default_value = "basic", value_parser = ["none", "basic", "auto", "ollama", "claude"])]
        cleanup: String,
    },
    /// Import phrases and suggest names from a text file.
    ImportText { path: PathBuf },
    /// Print model, Python, CUDA, and desktop portal availability.
    Doctor,
    /// Generate hashed wheel and Rust archive sources for an offline Flatpak build.
    FlatpakSources,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = config::Paths::discover()?;
    paths.prepare()?;
    match cli.command.unwrap_or(Commands::Run {
        background: false,
        copy_only: false,
        key: None,
        smoke_test: false,
    }) {
        Commands::Run {
            background,
            copy_only,
            key,
            smoke_test,
        } => {
            let mut settings = config::Settings::load(&paths.data)?;
            if copy_only {
                settings.paste = false;
            }
            if let Some(key) = key {
                settings.shortcut = key;
            }
            let exit = ui::run(paths, settings, background, smoke_test);
            if exit != glib::ExitCode::SUCCESS {
                anyhow::bail!("Desktop application exited with {exit:?}");
            }
        }
        Commands::DownloadModels { whisper, samples } => {
            models::download(&paths, whisper, samples, |name, percent| {
                eprintln!("{name}: {percent:.0}%")
            })?
        }
        Commands::File {
            video,
            start,
            end,
            cleanup,
        } => {
            anyhow::ensure!(
                start.is_finite() && start >= 0.0 && end.is_none_or(|e| e.is_finite() && e > start),
                "Invalid start/end range"
            );
            println!(
                "{}",
                engine::transcribe(
                    &paths,
                    video
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("Video path is not UTF-8"))?,
                    start,
                    end,
                    &cleanup
                )?
            );
        }
        Commands::ImportText { path } => {
            engine::initialize(&paths, false)?;
            let (phrases, words) = engine::import_text(&path)?;
            println!("Imported {phrases} phrases ({words} words)");
        }
        Commands::Doctor => {
            println!(
                "Data: {}\nModels: {}\nEngine: {}",
                paths.data.display(),
                paths.models.display(),
                paths.engine.display()
            );
            let missing = models::missing(&paths.models, false);
            println!(
                "Lip-reading models: {}",
                if missing.is_empty() {
                    "installed".into()
                } else {
                    format!("missing {}", missing.join(", "))
                }
            );
            engine::initialize(&paths, false)?;
            pyo3::Python::attach(|py| -> pyo3::PyResult<()> {
                use pyo3::prelude::*;
                let torch = pyo3::types::PyModule::import(py, "torch")?;
                let cuda = torch.getattr("cuda")?;
                let available = cuda.call_method0("is_available")?.extract::<bool>()?;
                println!(
                    "PyTorch: {} · CUDA: {available}",
                    torch.getattr("__version__")?
                );
                if available {
                    println!("GPU: {}", cuda.call_method1("get_device_name", (0,))?);
                }
                Ok(())
            })?;
            if let Ok(portal) = portals::Portal::new() {
                glib::MainContext::default().block_on(async {
                    for name in [
                        "Camera",
                        "GlobalShortcuts",
                        "RemoteDesktop",
                        "Clipboard",
                        "Background",
                    ] {
                        println!("{name} portal: version {}", portal.version(name).await);
                    }
                });
            } else {
                println!("Desktop portal: no session bus");
            }
        }
        Commands::FlatpakSources => packaging::generate(&paths.engine)?,
    }
    Ok(())
}
