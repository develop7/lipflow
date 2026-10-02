use crate::config::Paths;
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    path::Path,
    time::Duration,
};

pub struct Asset {
    pub name: &'static str,
    pub url: &'static str,
    pub minimum: u64,
}
pub const ASSETS: &[Asset] = &[
    Asset { name: "vsr/model.json", url: "https://huggingface.co/Amanvir/LRS3_V_WER19.1/resolve/main/model.json", minimum: 100 },
    Asset { name: "vsr/model.pth", url: "https://huggingface.co/Amanvir/LRS3_V_WER19.1/resolve/main/model.pth", minimum: 900_000_000 },
    Asset { name: "lm/model.json", url: "https://huggingface.co/Amanvir/lm_en_subword/resolve/main/model.json", minimum: 100 },
    Asset { name: "lm/model.pth", url: "https://huggingface.co/Amanvir/lm_en_subword/resolve/main/model.pth", minimum: 200_000_000 },
    Asset { name: "lm/unigram5000.model", url: "https://github.com/mpc001/auto_avsr/raw/main/spm/unigram/unigram5000.model", minimum: 300_000 },
    Asset { name: "face_landmarker.task", url: "https://storage.googleapis.com/mediapipe-models/face_landmarker/face_landmarker/float16/1/face_landmarker.task", minimum: 3_000_000 },
];
pub const AV_ASSETS: &[Asset] = &[
    Asset { name: "av/config.json", url: "https://huggingface.co/nguyenvulebinh/auto_avsr_av_trlrwlrs2lrs3vox2avsp_base/resolve/main/config.json", minimum: 100 },
    Asset { name: "av/model.safetensors", url: "https://huggingface.co/nguyenvulebinh/auto_avsr_av_trlrwlrs2lrs3vox2avsp_base/resolve/main/model.safetensors", minimum: 1_000_000_000 },
];
pub const SAMPLE: Asset = Asset { name: "2016-03-12.mov", url: "https://upload.wikimedia.org/wikipedia/commons/transcoded/c/ce/2016-03-12_President_Obama%27s_Weekly_Address.webm/2016-03-12_President_Obama%27s_Weekly_Address.webm.360p.mpeg4.mov", minimum: 1_000_000 };

pub fn missing(root: &Path, whisper: bool) -> Vec<String> {
    ASSETS
        .iter()
        .chain(AV_ASSETS.iter().filter(|_| whisper))
        .filter(|asset| {
            std::fs::metadata(root.join(asset.name))
                .map(|m| m.len() < asset.minimum)
                .unwrap_or(true)
        })
        .map(|a| a.name.into())
        .collect()
}

pub fn client() -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(1800))
        .user_agent("Lipflow/0.1 GNOME");
    // Honor a supplied trust bundle while retaining certificate and hostname verification.
    if let Some(bundle) = std::env::var_os("SSL_CERT_FILE") {
        for certificate in reqwest::Certificate::from_pem_bundle(&std::fs::read(bundle)?)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder.build()?)
}

pub fn download(
    paths: &Paths,
    whisper: bool,
    samples: bool,
    progress: impl Fn(&str, f64),
) -> Result<()> {
    let client = client()?;
    for asset in ASSETS.iter().chain(AV_ASSETS.iter().filter(|_| whisper)) {
        fetch(&client, &paths.models, asset, &progress)?;
    }
    if samples {
        let samples =
            if paths.engine.join("pyproject.toml").is_file() && !paths.engine.starts_with("/app") {
                paths.engine.join("samples")
            } else {
                paths.data.join("samples")
            };
        fetch(&client, &samples, &SAMPLE, &progress)?;
    }
    Ok(())
}

fn fetch(
    client: &reqwest::blocking::Client,
    root: &Path,
    asset: &Asset,
    progress: &impl Fn(&str, f64),
) -> Result<()> {
    let destination = root.join(asset.name);
    if destination
        .metadata()
        .is_ok_and(|m| m.len() >= asset.minimum)
    {
        return Ok(());
    }
    std::fs::create_dir_all(destination.parent().unwrap())?;
    let temporary = destination.with_extension("part");
    let result = (|| -> Result<()> {
        let mut response = client
            .get(asset.url)
            .send()
            .with_context(|| format!("Downloading {}", asset.name))?
            .error_for_status()
            .with_context(|| format!("Downloading {}", asset.name))?;
        let total = response.content_length().unwrap_or(0);
        let mut file = std::fs::File::create(&temporary)?;
        let mut buffer = vec![0; 1 << 20];
        let mut done = 0;
        let mut last = -1;
        loop {
            let length = response.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            file.write_all(&buffer[..length])?;
            done += length as u64;
            let percent = if total > 0 {
                (100 * done / total) as i32
            } else {
                0
            };
            if percent != last {
                progress(asset.name, percent.min(100) as f64);
                last = percent;
            }
        }
        if done < asset.minimum || (total > 0 && done != total) {
            bail!("Incomplete download: {}", asset.name);
        }
        file.sync_all()?;
        if asset.name.ends_with(".json") {
            let _: serde_json::Value = serde_json::from_slice(&std::fs::read(&temporary)?)?;
        }
        std::fs::rename(&temporary, &destination)?;
        progress(asset.name, 100.0);
        Ok(())
    })();
    if temporary.exists() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}
