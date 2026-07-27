//! Offline STT via whisper.cpp (whisper-rs bindings).
//!
//! Compiled only with the `whisper` cargo feature. The model file is expected
//! at `src-tauri/models/ggml-<model>.bin` or in the runtime config directory
//! (`~/.config/spoke/models/` on Linux, `~/Library/Application Support/spoke/models/`
//! on macOS). Audio is resampled to 16 kHz mono, which whisper.cpp requires.

use crate::audio;
use crate::config::Config;
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Mutex;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

const WHISPER_RATE: u32 = 16_000;

pub struct WhisperStt {
    /// Reused across transcriptions: whisper.cpp initializes the Metal backend
    /// and the CoreML encoder (incl. the multi-minute first ANE specialization)
    /// per *state*, not per context — recreating it every run would pay that
    /// cost on every recording. The state internally keeps the ggml weights
    /// (`Arc<WhisperInnerContext>`) alive.
    state: Mutex<WhisperState>,
}

impl WhisperStt {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let path = resolve_model_path(&cfg.offline.model)
            .ok_or_else(|| anyhow!("whisper model '{}' not found (download it first)", cfg.offline.model))?;

        // Toggle the CoreML bundle so whisper.cpp selects the right backend.
        #[cfg(feature = "coreml")]
        {
            let bundle = coreml_bundle_path(&cfg.offline.model);
            let mut disabled = bundle.as_os_str().to_os_string();
            disabled.push(".disabled");
            let disabled = PathBuf::from(disabled);
            let use_coreml = cfg.offline.accel == "coreml" || cfg.offline.accel == "auto";

            if use_coreml && disabled.exists() {
                std::fs::rename(&disabled, &bundle)
                    .map_err(|e| anyhow!("failed to restore CoreML bundle: {e}"))?;
            } else if !use_coreml && bundle.exists() {
                std::fs::rename(&bundle, &disabled)
                    .map_err(|e| anyhow!("failed to disable CoreML bundle: {e}"))?;
            }
        }

        let mut params = WhisperContextParameters::default();
        let use_gpu = wants_gpu(cfg);
        params.use_gpu(use_gpu);
        // Flash attention speeds up Metal/GPU decoding significantly; safe here
        // since we don't use DTW token timestamps (incompatible with it).
        params.flash_attn(use_gpu);
        let build_start = std::time::Instant::now();
        let ctx = WhisperContext::new_with_params(
            path.to_str().ok_or_else(|| anyhow!("bad model path"))?,
            params,
        )
        .map_err(|e| anyhow!("failed to load whisper model: {e}"))?;
        let state = ctx
            .create_state()
            .map_err(|e| anyhow!("whisper state: {e}"))?;
        eprintln!(
            "[spoke] whisper engine built (model load + backend init) in {:.1}s",
            build_start.elapsed().as_secs_f32()
        );
        drop(ctx); // the state keeps the weights alive via its inner Arc
        Ok(Self {
            state: Mutex::new(state),
        })
    }

    /// Synchronous (whisper.cpp is blocking); callers should run it off the
    /// async executor's core threads.
    pub fn transcribe(&self, mono: &[f32], sample_rate: u32, language: &str) -> Result<String> {
        let audio = audio::resample_mono(mono, sample_rate, WHISPER_RATE);
        let mut audio = audio::strip_internal_silence(&audio, WHISPER_RATE);
        // Nothing but silence survived: running inference on it only invites a
        // hallucinated segment, so answer with no transcript instead.
        if audio.is_empty() {
            eprintln!("[spoke] no speech detected in recording — skipping inference");
            return Ok(String::new());
        }
        // Whisper's decoder truncates the last segment when audio ends mid-speech.
        // Appending 500 ms of silence gives it room to finalize the last tokens.
        let pad = WHISPER_RATE as usize / 2;
        audio.extend(std::iter::repeat(0.0f32).take(pad));

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(if language.is_empty() { "auto" } else { language }));
        params.set_no_speech_thold(0.6);
        params.set_suppress_blank(true);
        // The reused state's KV cache would otherwise feed the previous
        // recording's text in as context, bleeding it into this transcript.
        params.set_no_context(true);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        let mut state = self.state.lock().unwrap();
        let infer_start = std::time::Instant::now();
        state
            .full(params, &audio)
            .map_err(|e| anyhow!("whisper inference failed: {e}"))?;
        eprintln!(
            "[spoke] whisper inference: {:.2}s for {:.2}s of audio",
            infer_start.elapsed().as_secs_f32(),
            audio.len() as f32 / WHISPER_RATE as f32
        );

        let segments = state.full_n_segments();
        let mut out = String::new();
        for i in 0..segments {
            if let Some(segment) = state.get_segment(i) {
                if let Ok(text) = segment.to_str() {
                    out.push_str(text.trim());
                    out.push(' ');
                }
            }
        }
        Ok(out.trim().to_string())
    }
}

/// Resolve whether GPU should be enabled given the current config and build.
/// `accel = "none"` always disables GPU; otherwise whisper.cpp uses whichever
/// GPU backend was compiled in (Metal/CUDA/Vulkan — see src/platform.rs).
fn wants_gpu(cfg: &crate::config::Config) -> bool {
    cfg.offline.accel != "none" && cfg.offline.use_gpu
}

/// Hugging Face URL for a whisper.cpp ggml model.
pub fn model_download_url(model: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}.bin")
}

/// Hugging Face URL for the CoreML encoder bundle zip.
#[cfg(feature = "coreml")]
pub fn coreml_bundle_url(model: &str) -> String {
    format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{model}-encoder.mlmodelc.zip")
}

/// Path where the CoreML bundle lives, co-located with the GGML model file.
/// Falls back to the runtime models dir if the GGML model hasn't been downloaded yet.
#[cfg(feature = "coreml")]
pub fn coreml_bundle_path(model: &str) -> PathBuf {
    let bundle_name = format!("ggml-{model}-encoder.mlmodelc");
    if let Some(model_path) = resolve_model_path(model) {
        if let Some(parent) = model_path.parent() {
            return parent.join(&bundle_name);
        }
    }
    models_dir().join(bundle_name)
}

#[cfg(feature = "coreml")]
pub fn coreml_bundle_exists(model: &str) -> bool {
    coreml_bundle_path(model).exists()
}

/// Runtime directory where downloaded models are stored (under the OS config dir).
pub fn models_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("spoke").join("models")
}

/// Check whether the given model file exists in either the build or runtime dir.
pub fn model_exists(model: &str) -> bool {
    resolve_model_path(model).is_some()
}

/// Approximate on-disk download size for a known model, as a display label.
/// Empty for unknown models. Kept in sync with the bubble UI's chip notes.
pub fn model_size_label(model: &str) -> &'static str {
    match model {
        "tiny" => "74 MB",
        "base" => "141 MB",
        "small" => "465 MB",
        "medium" => "1.4 GB",
        "large-v3-turbo" => "1.5 GB",
        "large-v3" => "2.9 GB",
        _ => "",
    }
}

/// Delete a downloaded model from the runtime models dir.
///
/// Only removes `ggml-<model>.bin` inside `models_dir()`; the read-only build-dir
/// copy shipped with a binary is never touched. The model name is validated to
/// safe characters and the resolved path is confined to `models_dir()`, so this
/// can't be tricked into deleting anything outside the models directory. A
/// missing file is a no-op (idempotent). Callers restrict `model` to the known
/// set before reaching here.
pub fn delete_model(model: &str) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    if model.is_empty() || !model.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::new(ErrorKind::InvalidInput, "invalid model name"));
    }
    let dir = models_dir();
    let path = dir.join(format!("ggml-{model}.bin"));
    if path.parent() != Some(dir.as_path()) {
        return Err(Error::new(ErrorKind::InvalidInput, "path escapes models dir"));
    }
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Resolve the model path checking the build dir first, then the runtime dir.
pub fn resolve_model_path(model: &str) -> Option<PathBuf> {
    let name = format!("ggml-{model}.bin");
    let build = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("models").join(&name);
    if build.exists() {
        return Some(build);
    }
    let runtime = models_dir().join(&name);
    if runtime.exists() {
        return Some(runtime);
    }
    None
}
