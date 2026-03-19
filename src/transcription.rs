use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use transcribe_rs::engines::parakeet::{
    ParakeetEngine, ParakeetInferenceParams, ParakeetModelParams, TimestampGranularity,
};
use transcribe_rs::TranscriptionEngine;
use tracing::info;

static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

// ──────────────────────────────────────────────────────────────────────────────
// Constants
// ──────────────────────────────────────────────────────────────────────────────

const MODEL_DIR_NAME: &str = "parakeet-tdt-0.6b-v3-int8";
const MODEL_URL: &str = "https://s3.crispy.fyi/models/parakeet-v3-int8.tar.gz";
pub const MODEL_SIZE_MB: u64 = 478;

// ──────────────────────────────────────────────────────────────────────────────
// Model management
// ──────────────────────────────────────────────────────────────────────────────

/// Return the default models directory: `~/.toodles/models`.
pub fn default_models_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".toodles")
        .join("models")
}

/// Check whether the Parakeet model is already downloaded.
pub fn is_model_downloaded(models_dir: &Path) -> bool {
    let model_path = models_dir.join(MODEL_DIR_NAME);
    model_path.exists() && model_path.is_dir()
}

/// Download and extract the Parakeet V3 model with a terminal progress bar.
pub async fn download_model(models_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(models_dir)
        .with_context(|| format!("Failed to create models dir: {}", models_dir.display()))?;

    let partial_path = models_dir.join(format!("{MODEL_DIR_NAME}.tar.gz.partial"));
    let final_path = models_dir.join(MODEL_DIR_NAME);

    if final_path.exists() && final_path.is_dir() {
        info!("Model already downloaded at {}", final_path.display());
        return Ok(());
    }

    // Download
    let response = HTTP_CLIENT
        .get(MODEL_URL)
        .send()
        .await
        .context("Failed to start model download")?;

    if !response.status().is_success() {
        anyhow::bail!("Download failed: HTTP {}", response.status());
    }

    let total_size = response.content_length().unwrap_or(0);

    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::with_template(
            "  {spinner:.green} [{bar:40.cyan/dim}] {bytes}/{total_bytes} ({eta})",
        )
        .unwrap()
        .progress_chars("█▓░"),
    );

    let mut stream = response.bytes_stream();
    let mut file = std::fs::File::create(&partial_path)
        .with_context(|| format!("Failed to create {}", partial_path.display()))?;

    use std::io::Write;
    let mut downloaded: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error downloading chunk")?;
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;
        pb.set_position(downloaded);
    }
    file.flush()?;
    drop(file);
    pb.finish_with_message("Download complete");

    // Verify size
    if total_size > 0 {
        let actual = std::fs::metadata(&partial_path)?.len();
        if actual != total_size {
            let _ = std::fs::remove_file(&partial_path);
            anyhow::bail!(
                "Download incomplete: expected {total_size} bytes, got {actual} bytes"
            );
        }
    }

    // Extract tar.gz
    println!("  Extracting model...");
    let temp_dir = models_dir.join(format!("{MODEL_DIR_NAME}.extracting"));
    if temp_dir.exists() {
        std::fs::remove_dir_all(&temp_dir)?;
    }
    std::fs::create_dir_all(&temp_dir)?;

    let tar_gz = std::fs::File::open(&partial_path)?;
    let tar = flate2::read::GzDecoder::new(tar_gz);
    let mut archive = tar::Archive::new(tar);
    archive.unpack(&temp_dir).map_err(|e| {
        let _ = std::fs::remove_dir_all(&temp_dir);
        anyhow::anyhow!("Failed to extract archive: {e}")
    })?;

    // Move extracted directory into place
    let extracted_dirs: Vec<_> = std::fs::read_dir(&temp_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
        .collect();

    if extracted_dirs.len() == 1 {
        let source = extracted_dirs[0].path();
        if final_path.exists() {
            std::fs::remove_dir_all(&final_path)?;
        }
        std::fs::rename(&source, &final_path)?;
        let _ = std::fs::remove_dir_all(&temp_dir);
    } else {
        if final_path.exists() {
            std::fs::remove_dir_all(&final_path)?;
        }
        std::fs::rename(&temp_dir, &final_path)?;
    }

    // Clean up partial file
    let _ = std::fs::remove_file(&partial_path);

    println!("  ✔ Model extracted to {}", final_path.display());
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// Transcription engine wrapper
// ──────────────────────────────────────────────────────────────────────────────

/// A loaded Parakeet transcription engine, ready to transcribe audio.
pub struct LocalTranscriber {
    engine: ParakeetEngine,
}

impl LocalTranscriber {
    /// Load the Parakeet model from the given models directory.
    pub fn load(models_dir: &Path) -> Result<Self> {
        let model_path = models_dir.join(MODEL_DIR_NAME);
        if !model_path.exists() || !model_path.is_dir() {
            anyhow::bail!(
                "Parakeet model not found at {}. Run `toodles --setup` to download it.",
                model_path.display()
            );
        }

        info!("Loading Parakeet model from {}", model_path.display());
        let mut engine = ParakeetEngine::new();
        engine
            .load_model_with_params(&model_path, ParakeetModelParams::int8())
            .map_err(|e| anyhow::anyhow!("Failed to load Parakeet model: {e}"))?;
        info!("Parakeet model loaded successfully");

        Ok(Self { engine })
    }

    /// Transcribe f32 samples at 16 kHz mono.
    pub fn transcribe(&mut self, audio: Vec<f32>) -> Result<String> {
        if audio.is_empty() {
            return Ok(String::new());
        }

        let result = self
            .engine
            .transcribe_samples(
                audio,
                Some(ParakeetInferenceParams {
                    timestamp_granularity: TimestampGranularity::Segment,
                    ..Default::default()
                }),
            )
            .map_err(|e| anyhow::anyhow!("Parakeet transcription failed: {e}"))?;

        let text = result.text.trim().to_string();
        if text.is_empty() {
            info!("Transcription result is empty");
        } else {
            info!("Transcription: {} chars", text.len());
        }
        Ok(text)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Audio decoding: OGG Opus → f32 16 kHz mono (via ffmpeg)
// ──────────────────────────────────────────────────────────────────────────────

/// Decode OGG Opus bytes (from Telegram voice message) into 16 kHz mono f32 samples.
///
/// Uses `ffmpeg` as a subprocess to avoid C library linking issues.
/// Requires `ffmpeg` to be installed on the system.
pub fn decode_ogg_to_f32_16khz(ogg_bytes: &[u8]) -> Result<Vec<f32>> {
    use std::process::Command;

    // Write OGG to a temp file (unique per call).
    let tmp_dir = std::env::temp_dir();
    let id = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let input_path = tmp_dir.join(format!("toodles_{id}_{ts}.ogg"));
    let output_path = tmp_dir.join(format!("toodles_{id}_{ts}.raw"));

    std::fs::write(&input_path, ogg_bytes)
        .context("Failed to write temp OGG file")?;

    // Convert to raw 16-bit signed LE, 16 kHz, mono via ffmpeg.
    let status = Command::new("ffmpeg")
        .args([
            "-y",              // overwrite output
            "-i", &input_path.to_string_lossy(),
            "-f", "s16le",     // raw PCM signed 16-bit little-endian
            "-acodec", "pcm_s16le",
            "-ar", "16000",    // 16 kHz
            "-ac", "1",        // mono
            &output_path.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to run ffmpeg. Is it installed? (brew install ffmpeg)")?;

    // Clean up input file.
    let _ = std::fs::remove_file(&input_path);

    if !status.success() {
        let _ = std::fs::remove_file(&output_path);
        anyhow::bail!("ffmpeg exited with status: {status}");
    }

    // Read raw PCM bytes.
    let raw_bytes = std::fs::read(&output_path)
        .context("Failed to read ffmpeg output")?;
    let _ = std::fs::remove_file(&output_path);

    if raw_bytes.len() < 2 {
        anyhow::bail!("No audio samples decoded from OGG");
    }

    // Convert i16 LE bytes → f32 normalized to [-1.0, 1.0].
    let samples_f32: Vec<f32> = raw_bytes
        .chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / i16::MAX as f32
        })
        .collect();

    info!(
        "Decoded OGG: {} samples at 16000 Hz ({:.1}s)",
        samples_f32.len(),
        samples_f32.len() as f32 / 16000.0,
    );

    Ok(samples_f32)
}

