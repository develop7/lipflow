//! Emit an offline Flatpak wheel module from the locked, installed Linux environment.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, path::Path, process::Command};

#[derive(Deserialize)]
struct Lock {
    package: Vec<Package>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    #[serde(default)]
    wheels: Vec<Wheel>,
}
#[derive(Deserialize)]
struct Wheel {
    url: String,
    hash: String,
    size: u64,
}
#[derive(Deserialize)]
struct Installed {
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct CargoLock {
    package: Vec<CargoPackage>,
}
#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

fn compatible(filename: &str) -> bool {
    let parts: Vec<_> = filename.trim_end_matches(".whl").rsplitn(4, '-').collect();
    if parts.len() != 4 || !filename.ends_with(".whl") {
        return false;
    }
    let (platform, abi, python) = (parts[0], parts[1], parts[2]);
    let platform_ok = platform == "any"
        || platform
            .split('.')
            .any(|tag| tag.starts_with("manylinux") && tag.ends_with("_x86_64"));
    let python_ok = python.split('.').any(|tag| {
        tag == "py3"
            || tag == "cp312"
            || (abi == "abi3"
                && tag
                    .strip_prefix("cp")
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|n| (37..=312).contains(&n)))
    });
    platform_ok && python_ok && ["none", "abi3", "cp312"].contains(&abi)
}
// Defer the GPU-enabled PyTorch stack and its NVIDIA/CUDA support packages as
// one install-time payload; ordinary Python dependencies stay in the build.
fn deferred(name: &str) -> bool {
    name == "torch" || name == "triton" || name.starts_with("nvidia-") || name.starts_with("cuda-")
}

pub fn generate(root: &Path) -> Result<()> {
    anyhow::ensure!(
        std::env::consts::ARCH == "x86_64" && std::env::consts::OS == "linux",
        "Generate Flatpak sources on Linux x86_64"
    );
    let lock: Lock = toml::from_str(&std::fs::read_to_string(root.join("uv.lock"))?)?;
    let output = Command::new("uv")
        .args(["pip", "list", "--format", "json"])
        .current_dir(root)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "uv pip list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let installed: Vec<Installed> = serde_json::from_slice(&output.stdout)?;
    anyhow::ensure!(
        installed.iter().any(|p| p.name == "safetensors"),
        "Install safetensors==0.8.0 for the existing whisper engine before generating sources"
    );
    let excluded: HashSet<&str> = [
        "lipflow",
        "pytest",
        "pluggy",
        "iniconfig",
        "pygments",
        "pynput",
        "evdev",
        "python-xlib",
    ]
    .into_iter()
    .collect();
    let client = crate::models::client()?;
    let mut sources = Vec::new();
    let mut requirements = Vec::new();
    for package in installed
        .iter()
        .filter(|p| !excluded.contains(p.name.as_str()))
    {
        let pinned = lock
            .package
            .iter()
            .find(|p| p.name == package.name && p.version == package.version)
            .with_context(|| {
                format!(
                    "{}=={} is not in uv.lock; restore the frozen environment",
                    package.name, package.version
                )
            })?;
        let (url, hash, size) = if let Some(wheel) = pinned
            .wheels
            .iter()
            .find(|w| compatible(w.url.rsplit('/').next().unwrap_or("")))
        {
            (
                wheel.url.clone(),
                wheel.hash.trim_start_matches("sha256:").to_owned(),
                wheel.size,
            )
        } else if package.name == "safetensors" {
            // uv.lock only contains macOS wheels for this conditional dependency.
            // Resolve the same pinned version's Linux wheel from authoritative PyPI metadata.
            let metadata: Value = client
                .get(format!(
                    "https://pypi.org/pypi/safetensors/{}/json",
                    package.version
                ))
                .send()?
                .error_for_status()?
                .json()?;
            let wheel = metadata["urls"]
                .as_array()
                .context("No PyPI wheel metadata")?
                .iter()
                .find(|w| w["filename"].as_str().is_some_and(compatible))
                .context("No compatible safetensors wheel")?;
            (
                wheel["url"]
                    .as_str()
                    .context("Missing wheel URL")?
                    .to_owned(),
                wheel["digests"]["sha256"]
                    .as_str()
                    .context("Missing wheel digest")?
                    .to_owned(),
                wheel["size"].as_u64().context("Missing wheel size")?,
            )
        } else {
            bail!(
                "No Python 3.12 Linux x86_64 wheel for {}=={}",
                package.name,
                package.version
            );
        };
        anyhow::ensure!(
            url.starts_with("https://files.pythonhosted.org/")
                && hash.len() == 64
                && hash.bytes().all(|c| c.is_ascii_hexdigit()),
            "Invalid source metadata"
        );
        let filename = url.rsplit('/').next().unwrap();
        if deferred(&package.name) {
            anyhow::ensure!(size > 0, "Wheel source has an empty size: {url}");
            sources.push(json!({
                "type": "extra-data",
                "url": url,
                "sha256": hash,
                "size": size,
                "filename": filename
            }));
        } else {
            sources.push(json!({"type":"file", "url":url, "sha256":hash, "dest":"wheels", "dest-filename":filename}));
        }
        requirements.push(format!("{}=={}", package.name, package.version));
    }
    let flatpak = root.join("linux/flatpak");
    let module = json!({"name":"python-ml-dependencies", "buildsystem":"simple", "build-options":{"no-debuginfo":true}, "build-commands":[
        "for wheel in wheels/*.whl; do /app/bin/python3.12 -m pip install --no-index --no-deps --no-compile \"$wheel\" || exit; rm \"$wheel\"; done"
    ], "sources":sources});
    std::fs::write(
        flatpak.join("python-deps.json"),
        serde_json::to_vec_pretty(&module)?,
    )?;
    let cargo_lock: CargoLock =
        toml::from_str(&std::fs::read_to_string(root.join("linux/Cargo.lock"))?)?;
    let mut crates = Vec::new();
    for package in cargo_lock.package.iter().filter(|p| p.source.is_some()) {
        anyhow::ensure!(
            package.source.as_deref()
                == Some("registry+https://github.com/rust-lang/crates.io-index"),
            "Unsupported Cargo source"
        );
        let checksum = package.checksum.as_ref().context("Crate has no checksum")?;
        let destination = format!("vendor/{}-{}", package.name, package.version);
        crates.push(json!({"type":"archive", "archive-type":"tar-gzip", "url":format!("https://static.crates.io/crates/{}/{}-{}.crate", package.name, package.name, package.version), "sha256":checksum, "dest":destination}));
        crates.push(json!({"type":"inline", "contents":json!({"package":checksum,"files":{}}).to_string(), "dest":destination, "dest-filename":".cargo-checksum.json"}));
    }
    std::fs::write(
        flatpak.join("cargo-sources.json"),
        serde_json::to_vec_pretty(&crates)?,
    )?;
    println!(
        "Generated {} hashed wheel sources and Cargo.lock archive sources in {}",
        requirements.len(),
        flatpak.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gpu_distribution_classifier_defers_only_requested_names() {
        for name in ["torch", "triton", "nvidia-cublas-cu13", "cuda-bindings"] {
            assert!(deferred(name), "{name} should use extra-data");
        }
        for name in ["torchvision", "torchaudio", "numpy", "nvidia", "cuda"] {
            assert!(!deferred(name), "{name} should remain build-time");
        }
    }
    #[test]
    fn wheel_selection_rejects_other_interpreters_and_architectures() {
        assert!(compatible(
            "numpy-1.26.4-cp312-cp312-manylinux_2_17_x86_64.whl"
        ));
        assert!(compatible(
            "safetensors-0.8.0-cp310-abi3-manylinux_2_17_x86_64.whl"
        ));
        assert!(compatible("six-1.17.0-py2.py3-none-any.whl"));
        assert!(!compatible(
            "numpy-1.26.4-cp311-cp311-manylinux_2_17_x86_64.whl"
        ));
        assert!(!compatible(
            "safetensors-0.8.0-cp310-abi3-macosx_10_12_x86_64.whl"
        ));
        assert!(!compatible(
            "safetensors-0.8.0-cp310-abi3-manylinux_2_17_aarch64.whl"
        ));
        assert!(!compatible("package-1-py3-none-musllinux_1_2_x86_64.whl"));
    }
}
