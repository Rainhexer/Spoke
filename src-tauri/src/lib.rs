//! Spoke core: glues hotkey → capture → STT → injection together and exposes
//! a small command/event surface to the HTML/JS bubble UI.

mod audio;
mod config;
mod hotkey;
mod inject;
mod permissions;
mod platform;
mod stt;

use audio::AudioEngine;
use config::{Config, Mode, Trigger};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use stt::SttEngine;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};

/// Stable id so commands can fetch the tray back via `tray_by_id`.
const TRAY_ID: &str = "spoke-tray";
/// Most recent transcriptions to keep for the tray's quick-history menu.
const TRAY_HISTORY_MAX: usize = 20;
/// How many of those show at the top level; the rest fall into a submenu.
const TRAY_HISTORY_QUICK: usize = 4;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// Identifies the config fields that determine how an `SttEngine` is built.
/// When these are unchanged we reuse the cached engine instead of rebuilding —
/// rebuilding the offline engine reloads the whole Whisper model into RAM and
/// re-initializes the Metal/CoreML backends (the CoreML ANE specialization can
/// take minutes on first use), so the engine — including its whisper state —
/// must be built once and reused across transcriptions.
#[derive(PartialEq, Eq)]
struct EngineKey {
    mode: Mode,
    model: String,
    use_gpu: bool,
    accel: String,
    provider: String,
    api_key: String,
}

impl EngineKey {
    fn from_config(cfg: &Config) -> Self {
        Self {
            mode: cfg.general.mode,
            model: cfg.offline.model.clone(),
            use_gpu: cfg.offline.use_gpu,
            accel: cfg.offline.accel.clone(),
            provider: cfg.online.provider.clone(),
            api_key: cfg.online.api_key.clone(),
        }
    }
}

/// Shared application state, managed by Tauri as `Arc<SpokeState>`.
pub struct SpokeState {
    config: Mutex<Config>,
    audio: AudioEngine,
    recording: AtomicBool,
    /// Monotonically increasing counter incremented each time a new recording
    /// begins. The pipeline captures the current value when it starts and checks
    /// it before injecting text — if a newer session has started the result is
    /// discarded, effectively cancelling the stale in-flight transcription.
    session: AtomicU64,
    /// Cached STT engine + the config signature it was built for. Built lazily
    /// on first transcription and reused until the relevant config changes.
    engine: Mutex<Option<(EngineKey, Arc<SttEngine>)>>,
    /// Most-recent-first transcripts, capped at `TRAY_HISTORY_MAX`, feeding the
    /// tray's quick-history menu.
    history: Mutex<Vec<String>>,
    /// Whether the bubble is off screen (tray mode at launch, or minimized to
    /// the tray). Tracked explicitly rather than asked of the window: GTK
    /// reports a window built with `visible(false)` as visible until it is
    /// realized, so `is_visible()` can't be trusted at boot. When set, the tray
    /// icon is the only status channel and `emit_state` recolors it.
    bubble_hidden: AtomicBool,
}

impl SpokeState {
    fn config_snapshot(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    /// Return the cached STT engine, rebuilding it only when the engine-relevant
    /// config has changed since it was last built. The returned `Arc` is cloned
    /// out so the lock isn't held across the (possibly async) transcription.
    fn engine(&self, cfg: &Config) -> anyhow::Result<Arc<SttEngine>> {
        let key = EngineKey::from_config(cfg);
        let mut slot = self.engine.lock().unwrap();
        if let Some((cached_key, engine)) = slot.as_ref() {
            if *cached_key == key {
                return Ok(engine.clone());
            }
        }
        let engine = Arc::new(SttEngine::from_config(cfg)?);
        *slot = Some((key, engine.clone()));
        Ok(engine)
    }
}

/// Build the STT engine in the background so the first recording doesn't pay
/// model load + Metal/CoreML init on the critical path (the first-ever ANE
/// specialization of a CoreML model on a device can take minutes). No-op when
/// the cached engine already matches the config. Only warms offline mode —
/// the online engine is trivial to build lazily, and warming it would surface
/// "missing API key" errors at launch for unconfigured setups.
fn prewarm_engine(app: &AppHandle, state: &Arc<SpokeState>) {
    let cfg = state.config_snapshot();
    if cfg.general.mode != Mode::Offline {
        return;
    }
    #[cfg(feature = "whisper")]
    {
        // Don't warm (and thus error) when the model isn't downloaded yet.
        if !stt::whisper::model_exists(&cfg.offline.model) {
            return;
        }
        let app = app.clone();
        let state = Arc::clone(state);
        tauri::async_runtime::spawn(async move {
            let build = {
                let state = Arc::clone(&state);
                tokio::task::spawn_blocking(move || state.engine(&cfg).map(|_| ())).await
            };
            match build {
                Ok(Ok(())) => {}
                Ok(Err(e)) => emit_state(&app, "error", Some(format!("engine prewarm: {e}"))),
                Err(e) => emit_state(&app, "error", Some(format!("engine prewarm panicked: {e}"))),
            }
        });
    }
    #[cfg(not(feature = "whisper"))]
    let _ = app;
}

/// UI-facing recording state, sent on the `spoke:state` event. When the bubble
/// isn't on screen — the user picked tray mode at onboarding, or minimized to
/// the tray — the tray icon is the only feedback channel, so recolor it here.
fn emit_state(app: &AppHandle, state: &str, message: Option<String>) {
    if app
        .try_state::<Arc<SpokeState>>()
        .is_some_and(|s| s.bubble_hidden.load(Ordering::SeqCst))
    {
        apply_tray_state(app, if state == "error" { "warning" } else { state });
    }
    let _ = app.emit(
        "spoke:state",
        serde_json::json!({ "state": state, "message": message }),
    );
}

// ---- Tauri commands -------------------------------------------------------

#[tauri::command]
fn get_config(state: State<'_, Arc<SpokeState>>) -> Config {
    state.config_snapshot()
}

#[tauri::command]
fn set_config(app: AppHandle, new_config: Config) -> Result<(), String> {
    apply_config(&app, new_config)
}

/// Persist a new config, swap it into shared state, re-register the hotkey,
/// prewarm the engine, and refresh the tray menu. Shared by the `set_config`
/// command (UI edits) and the tray menu handlers (tray edits); the latter is
/// why this also emits `spoke:config` so the UI re-syncs its controls.
fn apply_config(app: &AppHandle, new_config: Config) -> Result<(), String> {
    new_config.save().map_err(|e| e.to_string())?;
    let state = app.state::<Arc<SpokeState>>();
    *state.config.lock().unwrap() = new_config.clone();
    // Re-register the hotkey in case it changed. Not while first-run
    // onboarding is still open — the hotkey goes live with the rest of the app
    // when `finish_onboarding` flips `onboarded` and calls back through here.
    if new_config.ui.onboarded {
        register_hotkey(app, &new_config).map_err(|e| e.to_string())?;
    }
    // Rebuild the engine in the background if engine-relevant config changed
    // (no-op otherwise), so the next recording doesn't pay the init cost.
    prewarm_engine(app, &state);
    rebuild_tray_menu(app);
    let _ = app.emit("spoke:config", &new_config);
    Ok(())
}

#[tauri::command]
fn list_audio_devices() -> Vec<String> {
    audio::list_input_devices()
}

#[tauri::command]
fn get_build_info() -> serde_json::Value {
    platform::build_info()
}

#[tauri::command]
fn check_permissions() -> permissions::Permissions {
    permissions::check()
}

#[tauri::command]
fn open_permission_settings(which: String) {
    permissions::open_settings(&which);
}

#[tauri::command]
fn request_accessibility_permission() {
    #[cfg(target_os = "macos")]
    permissions::request_accessibility();
    #[cfg(not(target_os = "macos"))]
    {}
}

/// Show the native mic permission prompt (if undetermined) and resolve with the
/// resulting status. A grant here applies to the running process immediately.
#[tauri::command]
async fn request_microphone_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        let tx = std::sync::Mutex::new(Some(tx));
        permissions::request_microphone(move |granted| {
            if let Some(tx) = tx.lock().unwrap().take() {
                let _ = tx.send(granted);
            }
        });
        rx.await.unwrap_or(false)
    }
    #[cfg(not(target_os = "macos"))]
    true
}

/// Clear this app's TCC entry for a permission ("microphone" | "accessibility")
/// so it can be re-requested cleanly. Recovers from the stale-grant state where
/// System Settings shows Spoke enabled but the OS denies the running binary
/// (the entry was recorded for a previous build's code signature).
#[tauri::command]
fn reset_permission(app: AppHandle, which: String) {
    permissions::reset(&which, &app.config().identifier);
}

/// Relaunch the process. Some permission changes only apply to a fresh process
/// (e.g. macOS revokes-then-regrants via System Settings while running); the UI
/// offers this as a one-click fallback when a grant doesn't register live.
#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    app.restart();
}

#[cfg(feature = "whisper")]
#[tauri::command]
fn check_model(model: String) -> Result<serde_json::Value, String> {
    let mut info = serde_json::json!({
        "exists": stt::whisper::model_exists(&model),
        "url": stt::whisper::model_download_url(&model),
        "coreml_exists": false,
    });
    #[cfg(feature = "coreml")]
    {
        info["coreml_exists"] = stt::whisper::coreml_bundle_exists(&model).into();
        info["coreml_url"] = stt::whisper::coreml_bundle_url(&model).into();
    }
    Ok(info)
}

#[cfg(feature = "whisper")]
#[tauri::command]
fn check_models() -> Result<Vec<String>, String> {
    let known = ["tiny", "base", "small", "medium", "large-v3-turbo", "large-v3"];
    Ok(known
        .iter()
        .filter(|m| stt::whisper::model_exists(m))
        .map(|s| s.to_string())
        .collect())
}

#[cfg(feature = "coreml")]
#[tauri::command]
async fn download_coreml_bundle(app: AppHandle, model: String) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let url = stt::whisper::coreml_bundle_url(&model);
    let dest_dir = stt::whisper::models_dir();
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("failed to create models dir: {e}"))?;

    let bundle_path = dest_dir.join(format!("ggml-{model}-encoder.mlmodelc"));
    if bundle_path.exists() {
        let _ = app.emit("spoke:coreml-complete", serde_json::json!({ "model": model }));
        return Ok(());
    }

    let tmp_path = dest_dir.join(format!("ggml-{model}-encoder.mlmodelc.zip.tmp"));

    let client = reqwest::Client::builder()
        .user_agent("Spoke/0.1")
        .build()
        .map_err(|e| e.to_string())?;

    let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("server returned {}", response.status()));
    }

    let total = response.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;
    let mut file = tokio::fs::File::create(&tmp_path).await.map_err(|e| e.to_string())?;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if total > 0 {
            let percent = (downloaded as f64 / total as f64 * 100.0) as u32;
            let _ = app.emit("spoke:coreml-progress", serde_json::json!({
                "model": &model,
                "percent": percent,
                "phase": "download",
            }));
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);

    // Extract on a blocking thread (zip::ZipArchive requires Seek).
    let tmp_clone = tmp_path.clone();
    let dest_clone = dest_dir.clone();
    let model_clone = model.clone();
    let app_clone = app.clone();

    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let _ = app_clone.emit("spoke:coreml-progress", serde_json::json!({
            "model": &model_clone,
            "percent": 100,
            "phase": "extract",
        }));

        let f = std::fs::File::open(&tmp_clone)
            .map_err(|e| format!("open zip: {e}"))?;
        let mut archive = zip::ZipArchive::new(f)
            .map_err(|e| format!("read zip: {e}"))?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| format!("zip entry: {e}"))?;
            // enclosed_name() rejects absolute paths and `..` traversal.
            let rel = entry
                .enclosed_name()
                .ok_or_else(|| format!("unsafe zip path: {}", entry.name()))?;
            let out = dest_clone.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&out).map_err(|e| format!("mkdir: {e}"))?;
            } else {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p).map_err(|e| format!("mkdir: {e}"))?;
                }
                let mut out_f = std::fs::File::create(&out).map_err(|e| format!("create: {e}"))?;
                std::io::copy(&mut entry, &mut out_f).map_err(|e| format!("extract: {e}"))?;
            }
        }

        let _ = std::fs::remove_file(&tmp_clone);
        Ok(())
    })
    .await
    .map_err(|e| format!("extraction panicked: {e}"))??;

    let _ = app.emit("spoke:coreml-complete", serde_json::json!({ "model": model }));
    Ok(())
}

/// Fire a desktop notification. Works from the Rust side regardless of window
/// capabilities, so a user running with the bubble hidden still gets download
/// status. Silently ignores failures (a missing notification daemon must never
/// break a download).
#[cfg(feature = "whisper")]
fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Delete a downloaded model. Restricted to the known model set and to the
/// runtime models dir (see `whisper::delete_model`). Refreshes the tray menu
/// and tells the bubble so both reflect the freed slot immediately.
#[cfg(feature = "whisper")]
#[tauri::command]
fn delete_model(app: AppHandle, model: String) -> Result<(), String> {
    const KNOWN: [&str; 6] = ["tiny", "base", "small", "medium", "large-v3-turbo", "large-v3"];
    if !KNOWN.contains(&model.as_str()) {
        return Err(format!("unknown model: {model}"));
    }
    stt::whisper::delete_model(&model).map_err(|e| e.to_string())?;
    rebuild_tray_menu(&app);
    let _ = app.emit("spoke:model-deleted", serde_json::json!({ "model": model }));
    notify(&app, "Model deleted", &format!("Removed the {model} model"));
    Ok(())
}

/// Download a model, emitting progress/complete events and a desktop
/// notification on success or failure. The streaming/atomic-rename work lives in
/// `download_model_inner`; this wrapper only adds the terminal notification so
/// downloads report status even when the bubble is hidden in the tray.
#[cfg(feature = "whisper")]
#[tauri::command]
async fn download_model(app: AppHandle, model: String) -> Result<(), String> {
    let res = download_model_inner(&app, &model).await;
    match &res {
        Ok(()) => {
            let size = stt::whisper::model_size_label(&model);
            let body = if size.is_empty() {
                format!("{model} is ready to use")
            } else {
                format!("{model} ({size}) is ready to use")
            };
            notify(&app, "Model downloaded", &body);
        }
        Err(e) => notify(&app, "Download failed", &format!("{model}: {e}")),
    }
    res
}

#[cfg(feature = "whisper")]
async fn download_model_inner(app: &AppHandle, model: &str) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let url = stt::whisper::model_download_url(&model);
    let dest_dir = stt::whisper::models_dir();
    std::fs::create_dir_all(&dest_dir).map_err(|e| format!("failed to create models directory: {e}"))?;
    let dest_path = dest_dir.join(format!("ggml-{model}.bin"));

    if dest_path.exists() {
        let _ = app.emit("spoke:download-complete", serde_json::json!({ "model": model }));
        return Ok(());
    }

    // Stream to a temp file and rename on success, so an interrupted download
    // never leaves a truncated file that later passes the "model exists" check.
    let tmp_path = dest_dir.join(format!("ggml-{model}.bin.tmp"));

    let client = reqwest::Client::builder()
        .user_agent("Spoke/0.1")
        .build()
        .map_err(|e| e.to_string())?;

    let response = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("server returned {}", response.status()));
    }

    let total = response.content_length().unwrap_or(0);
    let mut downloaded: u64 = 0;
    let mut file = tokio::fs::File::create(&tmp_path).await.map_err(|e| e.to_string())?;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if total > 0 {
            let percent = (downloaded as f64 / total as f64 * 100.0) as u32;
            let _ = app.emit("spoke:download-progress", serde_json::json!({
                "model": model,
                "downloaded": downloaded,
                "total": total,
                "percent": percent,
            }));
        }
    }

    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    tokio::fs::rename(&tmp_path, &dest_path)
        .await
        .map_err(|e| format!("failed to finalize download: {e}"))?;
    let _ = app.emit("spoke:download-complete", serde_json::json!({ "model": model }));
    Ok(())
}

#[tauri::command]
fn start_recording(app: AppHandle, state: State<'_, Arc<SpokeState>>) {
    begin_recording(&app, state.inner());
}

#[tauri::command]
fn stop_recording(app: AppHandle, state: State<'_, Arc<SpokeState>>) {
    let inner = state.inner().clone();
    finish_recording(app, inner);
}

#[tauri::command]
fn is_recording(state: State<'_, Arc<SpokeState>>) -> bool {
    state.recording.load(Ordering::SeqCst)
}

/// Move and resize the bubble window together (boot placement, non-Linux
/// open/close). Issued back-to-back so both land in the same event-loop turn.
#[tauri::command]
fn set_window_bounds(app: AppHandle, x: f64, y: f64, w: f64, h: f64) {
    if let Some(win) = app.get_webview_window("bubble") {
        let _ = win.set_size(tauri::LogicalSize::new(w, h));
        let _ = win.set_position(tauri::LogicalPosition::new(x, y));
    }
}

/// Linux menu open/close: resize the window anchored to the bubble's corner.
///
/// Keeping the bubble fixed through a resize naively takes a resize plus a
/// move, but any move request is validated by the WM against the workarea
/// using whatever size it believes at that instant — closing the menu near a
/// screen/monitor edge gets the position clamped (the still-menu-sized window
/// wouldn't fit) and the bubble visibly walks. Setting the ICCCM win-gravity
/// to the bubble's corner and sending a resize *only* removes the move
/// entirely: the WM itself keeps that corner pinned. (Don't reach for gdk's
/// move_resize instead — it resizes the X window behind GTK's back and the
/// webview keeps painting only the old area.)
#[tauri::command]
fn set_window_size_anchored(app: AppHandle, w: f64, h: f64, gravity: String) {
    #[cfg(target_os = "linux")]
    if let Some(win) = app.get_webview_window("bubble") {
        use gtk::prelude::*;
        let win2 = win.clone();
        let _ = win.run_on_main_thread(move || {
            let Ok(gtk_win) = win2.gtk_window() else { return };
            let g = match gravity.as_str() {
                "nw" => gtk::gdk::Gravity::NorthWest,
                "ne" => gtk::gdk::Gravity::NorthEast,
                "sw" => gtk::gdk::Gravity::SouthWest,
                _ => gtk::gdk::Gravity::SouthEast,
            };
            gtk_win.set_gravity(g);
            gtk_win.resize(w as i32, h as i32);
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (app, w, h, gravity);
}

#[tauri::command]
fn nudge_repaint(app: AppHandle) {
    #[cfg(target_os = "linux")]
    {
        // Only the Wayland backend needs the nudge (run() defaults GDK_BACKEND
        // to x11, so this is only hit on an explicit override). On X11 the
        // webview presents on its own and the oscillating resize just adds
        // visible flicker.
        let wayland = std::env::var("GDK_BACKEND")
            .map(|v| v.contains("wayland"))
            .unwrap_or(false);
        if !wayland {
            return;
        }
        if let Some(win) = app.get_webview_window("bubble") {
            if let Ok(size) = win.outer_size() {
                let width = if size.width % 2 == 0 {
                    size.width + 1
                } else {
                    size.width - 1
                };
                let _ = win.set_size(tauri::PhysicalSize::new(width, size.height));
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

// ---- Recording lifecycle --------------------------------------------------

fn begin_recording(app: &AppHandle, state: &Arc<SpokeState>) {
    // Refuse to start capturing without mic permission. Without this guard the
    // OS prompt fires mid-recording on first use: CoreAudio captures silence
    // while the dialog is up and the dictation "fails" confusingly. Triggering
    // the prompt here instead means the user grants once and the next attempt
    // works — no restart needed.
    #[cfg(target_os = "macos")]
    match permissions::check().microphone {
        permissions::PermissionState::Granted | permissions::PermissionState::Unknown => {}
        permissions::PermissionState::Undetermined => {
            permissions::request_microphone(|_| {});
            emit_state(
                app,
                "error",
                Some("Microphone permission needed — grant it in the dialog, then dictate again".into()),
            );
            return;
        }
        permissions::PermissionState::Denied => {
            emit_state(
                app,
                "error",
                Some("Microphone access denied — open the Microphone card to fix it".into()),
            );
            return;
        }
    }

    if state.recording.swap(true, Ordering::SeqCst) {
        return; // already recording
    }
    // Bump the session counter so any in-flight pipeline can detect
    // it has been superseded and skip injecting stale text.
    state.session.fetch_add(1, Ordering::SeqCst);

    let device = state.config_snapshot().recording.input_device.clone();
    if let Err(e) = state.audio.start(&device) {
        state.recording.store(false, Ordering::SeqCst);
        emit_state(app, "error", Some(e.to_string()));
        return;
    }
    emit_state(app, "recording", None);
}

/// Stop capture, transcribe, and inject. Runs the heavy work off the UI path.
fn finish_recording(app: AppHandle, state: Arc<SpokeState>) {
    if !state.recording.swap(false, Ordering::SeqCst) {
        return; // wasn't recording
    }
    let session = state.session.load(Ordering::SeqCst);
    emit_state(&app, "processing", None);

    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        match run_pipeline(&app_clone, &state, session).await {
            Ok(_) => emit_state(&app, "idle", None),
            Err(e) => {
                emit_state(&app, "error", Some(e.to_string()));
                // Settle back to idle so the bubble doesn't stay stuck.
                emit_state(&app, "idle", None);
            }
        }
        // Per-run scratch (resampled audio, base64 buffers) is freed by now —
        // the whisper state (KV cache, Metal/CoreML contexts) is deliberately
        // retained inside the cached engine because re-initializing it per run
        // costs seconds (Metal) to minutes (CoreML/ANE). glibc keeps freed
        // scratch in its arenas rather than returning it to the OS, so force
        // the freed pages back so RSS stays flat.
        release_heap();
    });
}

/// Return freed heap pages to the OS. glibc's allocator caches freed blocks in
/// per-thread arenas indefinitely; `malloc_trim` releases the top of the heap
/// back to the kernel so RSS drops after each transcription instead of climbing.
/// No-op on non-glibc targets (musl, macOS, Windows).
fn release_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // Safety: malloc_trim is always safe to call; it only frees unused heap.
        unsafe {
            malloc_trim(0);
        }
    }
}

async fn run_pipeline(app: &AppHandle, state: &Arc<SpokeState>, session: u64) -> anyhow::Result<()> {
    let cfg = state.config_snapshot();
    let rec = state.audio.stop()?;

    let mono = audio::to_mono(&rec.samples, rec.channels);
    if mono.is_empty() {
        return Err(anyhow::anyhow!(
            "No audio captured – check microphone permissions and device selection"
        ));
    }
    // Low-gain mics (built-in mics, or any device with the OS input slider
    // turned down) peak around -30 dBFS, which whisper classifies as silence and
    // answers with [BLANK_AUDIO]. Level the audio before it reaches any engine.
    let mono = audio::normalize_gain(&mono);

    let sample_rate = rec.sample_rate;

    // Optional audio save.
    if cfg.recording.save_audio {
        let dir = cfg.resolved_save_path();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = dir.join(format!("spoke-{ts}.wav"));
        if cfg.recording.save_processed {
            let processed = audio::strip_internal_silence(&mono, sample_rate);
            let processed = audio::resample_mono(&processed, sample_rate, 16000);
            if let Err(e) = audio::save_wav_mono(&path, &processed, 16000) {
                eprintln!("[spoke] failed to save processed audio: {e}");
            }
        } else if let Err(e) = audio::save_wav(&path, &rec) {
            eprintln!("[spoke] failed to save audio: {e}");
        }
    }

    // Raw recording (interleaved samples at device rate) is no longer needed once
    // downmixed; drop it before transcription so it isn't resident during inference.
    drop(rec);

    let engine = state.engine(&cfg)?;
    // `mono` is moved in and freed as soon as inference completes; the whisper
    // arm runs on a blocking thread so this await doesn't stall the executor.
    let transcript = engine
        .transcribe(mono, sample_rate, cfg.general.language.clone())
        .await?;

    // Engines can return non-speech annotations ("[BLANK_AUDIO]", "[MUSIC]") as
    // ordinary text; those must never be typed into the user's editor.
    let transcript = stt::clean_transcript(transcript.trim());
    if transcript.is_empty() {
        return Ok(());
    }

    // If a new recording session has started since this pipeline was launched,
    // discard the stale transcript instead of injecting it.
    if state.session.load(Ordering::SeqCst) != session {
        return Ok(());
    }

    let dest = cfg.general.output_dest;

    let _ = app.emit("spoke:transcript", serde_json::json!({ "text": &transcript }));

    // Record it for the tray's quick-history menu, then rebuild the menu so the
    // new entry shows up.
    {
        let mut hist = state.history.lock().unwrap();
        hist.insert(0, transcript.clone());
        hist.truncate(TRAY_HISTORY_MAX);
    }
    rebuild_tray_menu(app);

    if dest.copies() {
        let text = transcript.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut cb = arboard::Clipboard::new()
                .map_err(|e| anyhow::anyhow!("clipboard init: {e}"))?;
            cb.set_text(&text)
                .map_err(|e| anyhow::anyhow!("clipboard set: {e}"))?;
            println!("[spoke] copied to clipboard: {text}");
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("clipboard task panicked: {e}"))??;
    }
    if dest.types() {
        // enigo is not Send; run it on a blocking thread.
        tokio::task::spawn_blocking(move || inject::inject_text(&transcript))
            .await
            .map_err(|e| anyhow::anyhow!("inject task panicked: {e}"))??;
    }

    Ok(())
}

// ---- Hotkey ---------------------------------------------------------------

fn register_hotkey(app: &AppHandle, cfg: &Config) -> anyhow::Result<()> {
    let shortcut = hotkey::parse_shortcut(&cfg.general.hotkey)?;
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    gs.register(shortcut)?;
    Ok(())
}

// ---- App entry ------------------------------------------------------------

/// WebKitGTK ≥ 2.50 enables "damage propagation" by default: instead of
/// presenting a fully repainted frame, only the damaged rectangles are pushed
/// to the windowing system. On a transparent window this is broken (at least
/// on NVIDIA/XWayland): the damaged region's translucent pixels get blended
/// OVER the previous frame instead of replacing it, so drop shadows re-draw
/// on top of themselves and darken with every repaint until the next full
/// redraw (~10 s) resets the cycle. Turn the feature off for our webview.
///
/// The feature-list API (webkit 2.42+) postdates the webkit2gtk crate's
/// bindings, so the C symbols are declared here directly; the library is
/// already linked.
#[cfg(target_os = "linux")]
fn disable_damage_propagation(win: &tauri::WebviewWindow) {
    use std::ffi::{c_char, c_void, CStr};
    extern "C" {
        fn webkit_settings_get_all_features() -> *mut c_void;
        fn webkit_feature_list_get_length(list: *mut c_void) -> usize;
        fn webkit_feature_list_get(list: *mut c_void, index: usize) -> *mut c_void;
        fn webkit_feature_get_identifier(feature: *mut c_void) -> *const c_char;
        fn webkit_settings_set_feature_enabled(
            settings: *mut c_void,
            feature: *mut c_void,
            enabled: i32,
        );
        fn webkit_feature_list_unref(list: *mut c_void);
    }
    let _ = win.with_webview(|webview| {
        use gtk::glib::translate::ToGlibPtr;
        use webkit2gtk::WebViewExt;
        let Some(settings) = webview.inner().settings() else {
            return;
        };
        let settings_ptr: *mut webkit2gtk::ffi::WebKitSettings = settings.to_glib_none().0;
        unsafe {
            let list = webkit_settings_get_all_features();
            if list.is_null() {
                return;
            }
            for i in 0..webkit_feature_list_get_length(list) {
                let feature = webkit_feature_list_get(list, i);
                let id = webkit_feature_get_identifier(feature);
                if !id.is_null()
                    && CStr::from_ptr(id).to_bytes() == b"PropagateDamagingInformation"
                {
                    webkit_settings_set_feature_enabled(settings_ptr.cast(), feature, 0);
                }
            }
            webkit_feature_list_unref(list);
        }
    });
}

/// Mirror the webview's console to stdout when `SPOKE_DEBUG_CONSOLE=1`.
///
/// Release builds ship without devtools, so a JS error in a bundled app is
/// otherwise invisible — the UI just quietly does nothing (this is how the
/// custom-scheme audio failure hid itself). Run the binary from a terminal with
/// the variable set to get console.log/warn/error on stdout.
#[cfg(target_os = "linux")]
fn enable_console_logging(win: &tauri::WebviewWindow) {
    if std::env::var("SPOKE_DEBUG_CONSOLE").as_deref() != Ok("1") {
        return;
    }
    let _ = win.with_webview(|webview| {
        use webkit2gtk::{SettingsExt, WebViewExt};
        if let Some(settings) = webview.inner().settings() {
            settings.set_enable_write_console_messages_to_stdout(true);
        }
    });
}

/// Stop GTK/X painting a default (black) background behind the webview.
///
/// The X server fills not-yet-painted regions of a window with its background
/// before WebKit's first frame lands; the default fill is black, which flashes
/// as a dark rectangle when a window is mapped or resized. Marking the window
/// app-paintable and setting the X-level background to transparent black (valid
/// for the ARGB visual) makes those regions simply invisible instead.
#[cfg(target_os = "linux")]
fn clear_gtk_background(win: &tauri::WebviewWindow) {
    use gtk::glib::translate::ToGlibPtr;
    use gtk::prelude::*;
    let Ok(gtk_win) = win.gtk_window() else {
        return;
    };
    gtk_win.set_app_paintable(true);
    if let Some(gdk_win) = gtk_win.window() {
        let rgba = gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
        // Deprecated in GTK 3.22 but still functional; the rust binding gates
        // it away, so call the C symbol.
        unsafe {
            gtk::gdk::ffi::gdk_window_set_background_rgba(
                gdk_win.to_glib_none().0,
                rgba.to_glib_none().0,
            );
        }
    }
}

/// Set a window's compositor opacity. Unlike hiding, this leaves the window
/// mapped, so the webview keeps rendering while it is invisible.
#[cfg(target_os = "linux")]
fn set_window_opacity(win: &tauri::WebviewWindow, alpha: f64) {
    use gtk::prelude::*;
    if let Ok(gtk_win) = win.gtk_window() {
        gtk_win.set_opacity(alpha);
    }
}

/// Reveal the onboarding card once it has painted (see the builder in `setup`).
/// Idempotent: safe to call from both the UI and the watchdog.
fn show_onboard_window(app: &AppHandle) {
    let Some(win) = app.get_webview_window("onboard") else {
        return;
    };
    #[cfg(target_os = "linux")]
    set_window_opacity(&win, 1.0);
    let _ = win.show();
    let _ = win.set_focus();
}

/// Called by onboard.js once the first step has rendered.
#[tauri::command]
fn show_onboarding(app: AppHandle) {
    show_onboard_window(&app);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Linux windowing: force the X11 GDK backend (XWayland on Wayland sessions)
    // unless the user overrides it. Native Wayland can neither report nor set
    // global window positions, which breaks the bubble's edge-aware menu
    // flipping (outerPosition/setPosition silently no-op, so the menu always
    // grows down-right and gets clipped), and WebKitGTK's Wayland path needs
    // GPU compositing disabled to show transparent windows at all — software
    // rendering that ghosts and mangles drop shadows. Under X11, stock
    // WebKitGTK compositing (DMABUF renderer included) presents the transparent
    // window cleanly — on Mesa. Do NOT set WEBKIT_DISABLE_COMPOSITING_MODE
    // here, and do not disable the DMABUF renderer on non-NVIDIA systems:
    // that fallback path never clears the window's alpha buffer between
    // frames, so translucent pixels (drop shadows) accumulate darker every
    // repaint and moving elements leave trails.
    //
    // The NVIDIA proprietary driver is the exception. There the DMABUF
    // renderer's buffers never reach the X server for our transparent,
    // undecorated windows: the window maps (it shows up in the taskbar and in
    // the window list) but presents no frame at all, so the bubble and the
    // onboarding card are simply invisible. Blank windows beat shadow
    // artifacts, so disable that renderer when the NVIDIA kernel module is
    // loaded. Set WEBKIT_DISABLE_DMABUF_RENDERER yourself (to anything,
    // including 0) to override this either way.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if std::env::var("GDK_BACKEND").is_err() {
            std::env::set_var("GDK_BACKEND", "x11");
        }
        let nvidia = std::path::Path::new("/proc/driver/nvidia/version").exists();
        if nvidia && std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    let config = Config::load().unwrap_or_default();

    // Audio amplitude channel: capture thread → UI events.
    let (amp_tx, amp_rx) = std::sync::mpsc::channel::<f32>();
    let audio = AudioEngine::spawn(amp_tx);

    // Decided here rather than in `setup()`: the engine prewarm emits state
    // (and so consults this flag) from a task spawned before setup finishes.
    let bubble_hidden = !(config.ui.onboarded && !config.ui.start_minimized);

    let state = Arc::new(SpokeState {
        config: Mutex::new(config),
        audio,
        recording: AtomicBool::new(false),
        session: AtomicU64::new(0),
        engine: Mutex::new(None),
        history: Mutex::new(Vec::new()),
        bubble_hidden: AtomicBool::new(bubble_hidden),
    });

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(build_shortcut_plugin())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_config,
            set_config,
            start_recording,
            stop_recording,
            is_recording,
            list_audio_devices,
            nudge_repaint,
            minimize_to_tray,
            show_onboarding,
            finish_onboarding,
            set_tray_state,
            set_window_bounds,
            set_window_size_anchored,
            get_build_info,
            check_permissions,
            open_permission_settings,
            request_accessibility_permission,
            request_microphone_permission,
            reset_permission,
            restart_app,
            #[cfg(feature = "whisper")]
            check_model,
            #[cfg(feature = "whisper")]
            check_models,
            #[cfg(feature = "whisper")]
            download_model,
            #[cfg(feature = "whisper")]
            delete_model,
            #[cfg(feature = "coreml")]
            download_coreml_bundle,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Forward amplitude values to the UI.
            let amp_handle = handle.clone();
            std::thread::spawn(move || {
                while let Ok(v) = amp_rx.recv() {
                    let _ = amp_handle.emit("spoke:amplitude", v);
                }
            });

            // Register the configured hotkey. Skipped during first-run
            // onboarding: nothing but the card should be live yet, and a
            // stray hotkey press there would record with no bubble on screen
            // and type the transcript into whatever had focus. Onboarding's
            // final `finish_onboarding` → `apply_config` registers it.
            let cfg = app.state::<Arc<SpokeState>>().config_snapshot();
            if cfg.ui.onboarded {
                if let Err(e) = register_hotkey(&handle, &cfg) {
                    eprintln!("[spoke] hotkey registration failed: {e}");
                }
            }

            // Warm the STT engine so the first recording is fast.
            prewarm_engine(&handle, &app.state::<Arc<SpokeState>>());

            // Onboarding owns the screen: no tray icon (and so no settings
            // menu) until the user presses "Start Spoke". `finish_onboarding`
            // builds it.
            if cfg.ui.onboarded {
                build_tray(&handle)?;
            }
            #[cfg(feature = "whisper")]
            install_tray_download_feedback(&handle);

            // The bubble window is created here rather than declared in
            // tauri.conf.json so its initial visibility can depend on config:
            // first-run onboarding hides the bubble until the card is dismissed,
            // and a returning user who chose tray mode also boots hidden. The
            // bubble is always built — tray mode is a runtime preference, not a
            // build variant, so it stays one click away from the tray menu.
            let show_bubble_at_boot = cfg.ui.onboarded && !cfg.ui.start_minimized;
            if cfg.ui.onboarded && cfg.ui.start_minimized {
                // Launching straight into the tray: colour the icon idle now,
                // so it reads as "Spoke is running" rather than the app icon.
                apply_tray_state(&handle, "idle");
            }

            {
                use tauri::{WebviewUrl, WebviewWindowBuilder};
                WebviewWindowBuilder::new(app, "bubble", WebviewUrl::default())
                    .title("Spoke")
                    .inner_size(320.0, 320.0)
                    .resizable(false)
                    .decorations(false)
                    .transparent(true)
                    .always_on_top(true)
                    .skip_taskbar(true)
                    .shadow(false)
                    .visible(show_bubble_at_boot)
                    .focused(false)
                    .build()?;
            }

            position_bubble(&handle);

            if let Some(win) = handle.get_webview_window("bubble") {
                // Ensure the window receives pointer (mouse) events. On Linux,
                // transparent windows with decorations=false may default to
                // ignoring cursor events, making clicks pass through silently.
                let _ = win.set_ignore_cursor_events(false);

                // GTK snaps a non-resizable window back to its child's natural
                // size (the webview reports 200×200), silently overriding the
                // 80×80 bubble-only size and stranding the bubble mid-window.
                // Marking the window resizable makes programmatic resizes
                // stick; with no decorations the user still can't drag-resize.
                #[cfg(target_os = "linux")]
                {
                    disable_damage_propagation(&win);
                    enable_console_logging(&win);
                    let _ = win.set_resizable(true);
                    clear_gtk_background(&win);
                }

                // Keep the bubble visible when the user switches Spaces on macOS.
                #[cfg(target_os = "macos")]
                let _ = win.set_visible_on_all_workspaces(true);
            }

            // Fresh install: open the onboarding card centered on screen. It walks
            // the user through startup prefs, permissions, and model download, then
            // calls `finish_onboarding` to reveal the bubble.
            //
            // Revealed only once painted: a borderless transparent window is
            // mapped before the webview has drawn anything, so the compositor
            // shows an empty (black) frame for the first ~second.
            // `show_onboarding` — called from onboard.js once the first step is
            // on screen — reveals it; a watchdog reveals it anyway if that call
            // never arrives, so a JS error can't leave the user with no
            // onboarding at all.
            //
            // On Linux "hidden" means mapped at zero opacity rather than
            // unmapped. WebKitGTK only renders into a mapped window, and on
            // some GPU stacks (NVIDIA + the DMABUF renderer) a webview created
            // in an unmapped window never presents a frame after a later
            // show() — the window comes up permanently blank. Zero opacity
            // keeps it invisible while it paints normally.
            if !cfg.ui.onboarded {
                use tauri::{WebviewUrl, WebviewWindowBuilder};
                if let Ok(win) =
                    WebviewWindowBuilder::new(app, "onboard", WebviewUrl::App("onboard.html".into()))
                        .title("Welcome to Spoke")
                        .inner_size(460.0, 660.0)
                        .resizable(false)
                        .decorations(false)
                        .transparent(true)
                        .center()
                        .always_on_top(true)
                        .visible(false)
                        .focused(true)
                        .build()
                {
                    // Same GTK snap-to-child-size trap as the bubble: a
                    // non-resizable GTK window collapses to the webview's
                    // natural size, which strands the card off-screen.
                    #[cfg(target_os = "linux")]
                    {
                        disable_damage_propagation(&win);
                        enable_console_logging(&win);
                        let _ = win.set_resizable(true);
                        clear_gtk_background(&win);
                        set_window_opacity(&win, 0.0);
                        let _ = win.show();
                    }
                    let _ = win;

                    let watchdog = handle.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(2500));
                        show_onboard_window(&watchdog);
                    });
                }
            }

            Ok(())
        });

    builder
        .build(tauri::generate_context!())
        .expect("error while building Spoke")
        // Spoke is a tray app: hiding the bubble (tray mode, or minimize to
        // tray) must not tear the process down, and closing the onboarding
        // window mustn't either. Only an explicit exit — the tray's Quit item
        // calls `app.exit(0)`, which carries a code — is honored.
        .run(|_app, event| match event {
            tauri::RunEvent::ExitRequested { code, api, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            // Leave without running static destructors on macOS. whisper.cpp
            // keeps its Metal device in a function-local static, so the
            // destructor runs from `__cxa_finalize_ranges` inside `exit()` —
            // after our threads and the cached engine are still very much
            // alive. It then trips
            // `GGML_ASSERT([rsets->data count] == 0)` (ggml-metal-device.m)
            // because the model's Metal buffers are still registered, and
            // aborts. The abort is what macOS reports as "Spoke quit
            // unexpectedly" on an otherwise clean quit. Nothing of ours needs
            // atexit: Tauri has already run `cleanup_before_exit` by the time
            // this event fires, and config is persisted on write.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Exit => unsafe { libc::_exit(0) },
            _ => {}
        });
}

fn build_shortcut_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _shortcut, event| {
            let state = app.state::<Arc<SpokeState>>();
            let trigger = state.config_snapshot().general.trigger;
            match event.state() {
                ShortcutState::Pressed => match trigger {
                    Trigger::PushToTalk => begin_recording(app, &state),
                    Trigger::Toggle => {
                        if state.recording.load(Ordering::SeqCst) {
                            finish_recording(app.clone(), state.inner().clone());
                        } else {
                            begin_recording(app, &state);
                        }
                    }
                },
                ShortcutState::Released => {
                    if trigger == Trigger::PushToTalk {
                        finish_recording(app.clone(), state.inner().clone());
                    }
                }
            }
        })
        .build()
}

/// Create the tray icon. Takes an `AppHandle` rather than `&App` because the
/// tray is not always built during `setup`: first-run onboarding owns the whole
/// screen, so the tray only appears once `finish_onboarding` runs. No-ops if the
/// tray is already up.
fn build_tray(handle: &AppHandle) -> tauri::Result<()> {
    if handle.tray_by_id(TRAY_ID).is_some() {
        return Ok(());
    }
    let menu = build_tray_menu(handle)?;
    let builder = TrayIconBuilder::with_id(TRAY_ID)
        .icon(handle.default_window_icon().unwrap().clone())
        .menu(&menu)
        // Left click restores the bubble; the menu is right-click only.
        .show_menu_on_left_click(false)
        .tooltip("Spoke")
        .on_menu_event(handle_tray_menu_event);
    let builder = builder.on_tray_icon_event(|tray, event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            restore_from_tray(tray.app_handle());
        }
    });
    builder.build(handle)?;
    Ok(())
}

/// Wire model-download events (the same ones the bubble listens to) into tray
/// feedback: the tray tooltip shows live download percent, and the menu rebuilds
/// on completion so a freshly downloaded model moves from "Download model" up
/// into "Model". Keeps model downloads legible when the bubble is hidden.
#[cfg(feature = "whisper")]
fn install_tray_download_feedback(handle: &AppHandle) {
    use tauri::Listener;
    let h = handle.clone();
    handle.listen("spoke:download-progress", move |ev| {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(ev.payload()) {
            let model = v["model"].as_str().unwrap_or("");
            let percent = v["percent"].as_u64().unwrap_or(0);
            if let Some(tray) = h.tray_by_id(TRAY_ID) {
                let _ = tray.set_tooltip(Some(&format!("Downloading {model}… {percent}%")));
            }
        }
    });
    let h = handle.clone();
    handle.listen("spoke:download-complete", move |_ev| {
        if let Some(tray) = h.tray_by_id(TRAY_ID) {
            let _ = tray.set_tooltip(Some("Spoke"));
        }
        rebuild_tray_menu(&h);
    });
}

/// Rebuild the tray's context menu from current config + history and swap it in.
/// Menus are static once built, so any state that the menu reflects (a new
/// transcript, a changed setting) has to re-run this. Cheap enough to call on
/// every transcription. Silently no-ops if the tray isn't up yet.
fn rebuild_tray_menu(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_tray_menu(app) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}

/// Collapse a transcript to a single-line, length-capped menu label.
fn truncate_label(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 48;
    if one_line.chars().count() > MAX {
        let mut s: String = one_line.chars().take(MAX).collect();
        s.push('…');
        s
    } else {
        one_line
    }
}

/// Build the full tray context menu: restore, quick history + overflow submenu,
/// a Settings submenu mirroring the bubble's controls, and quit.
fn build_tray_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let cfg = app.state::<Arc<SpokeState>>().config_snapshot();
    let history = app.state::<Arc<SpokeState>>().history.lock().unwrap().clone();

    let show = MenuItem::with_id(app, "show", "Show Spoke", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Spoke", true, None::<&str>)?;

    // ---- Quick history + overflow submenu ----
    // Own the dynamically built items so `&dyn IsMenuItem` refs into them stay
    // valid until the menu is constructed.
    let mut quick_items: Vec<MenuItem<tauri::Wry>> = Vec::new();
    for (i, text) in history.iter().take(TRAY_HISTORY_QUICK).enumerate() {
        quick_items.push(MenuItem::with_id(
            app,
            format!("hist:{i}"),
            truncate_label(text),
            true,
            None::<&str>,
        )?);
    }
    let mut overflow_items: Vec<MenuItem<tauri::Wry>> = Vec::new();
    for (i, text) in history.iter().enumerate().skip(TRAY_HISTORY_QUICK) {
        overflow_items.push(MenuItem::with_id(
            app,
            format!("hist:{i}"),
            truncate_label(text),
            true,
            None::<&str>,
        )?);
    }

    let history_header = MenuItem::with_id(app, "hist-header", "Recent transcriptions", false, None::<&str>)?;
    let history_empty = MenuItem::with_id(app, "hist-empty", "No transcriptions yet", false, None::<&str>)?;
    let overflow_menu = if overflow_items.is_empty() {
        None
    } else {
        let refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
            overflow_items.iter().map(|m| m as &dyn IsMenuItem<tauri::Wry>).collect();
        Some(Submenu::with_items(app, "Older transcriptions", true, &refs)?)
    };

    // ---- Settings submenu ----
    let settings = build_settings_submenu(app, &cfg)?;

    // ---- Assemble top level ----
    let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&show, &sep1];
    items.push(&history_header);
    if history.is_empty() {
        items.push(&history_empty);
    } else {
        for it in &quick_items {
            items.push(it);
        }
        if let Some(ref sub) = overflow_menu {
            items.push(sub);
        }
    }
    items.push(&sep2);
    items.push(&settings);
    let quit_sep = PredefinedMenuItem::separator(app)?;
    items.push(&quit_sep);
    items.push(&quit);

    Menu::with_items(app, &items)
}

/// The Settings submenu: mode, model management (use/download/delete, offline
/// only) + acceleration, trigger, hotkey presets, output, language, microphone,
/// and the save-audio toggle — the same controls the bubble's radial menu
/// exposes, minus the online API key which needs free-text entry a native menu
/// can't provide. Ids are `<group>:<value>`; the handler routes them.
fn build_settings_submenu(app: &AppHandle, cfg: &Config) -> tauri::Result<Submenu<tauri::Wry>> {
    use config::{Mode, OutputDest, Trigger};

    // Mode.
    let mode_off = CheckMenuItem::with_id(app, "mode:offline", "Offline (Whisper)", true, cfg.general.mode == Mode::Offline, None::<&str>)?;
    let mode_on = CheckMenuItem::with_id(app, "mode:online", "Online (Cloud STT)", true, cfg.general.mode == Mode::Online, None::<&str>)?;
    let mode = Submenu::with_items(app, "Mode", true, &[&mode_off, &mode_on])?;

    // Trigger.
    let trig_ptt = CheckMenuItem::with_id(app, "trigger:push_to_talk", "Push to talk", true, cfg.general.trigger == Trigger::PushToTalk, None::<&str>)?;
    let trig_tog = CheckMenuItem::with_id(app, "trigger:toggle", "Toggle", true, cfg.general.trigger == Trigger::Toggle, None::<&str>)?;
    let trigger = Submenu::with_items(app, "Trigger", true, &[&trig_ptt, &trig_tog])?;

    // Output destination.
    let out_type = CheckMenuItem::with_id(app, "output:type", "Type it out", true, cfg.general.output_dest == OutputDest::Type, None::<&str>)?;
    let out_copy = CheckMenuItem::with_id(app, "output:copy", "Copy to clipboard", true, cfg.general.output_dest == OutputDest::Copy, None::<&str>)?;
    let out_both = CheckMenuItem::with_id(app, "output:both", "Type and copy", true, cfg.general.output_dest == OutputDest::Both, None::<&str>)?;
    let output = Submenu::with_items(app, "Output", true, &[&out_type, &out_copy, &out_both])?;

    // Language.
    let langs = [
        ("auto", "Auto"),
        ("en", "English"),
        ("da", "Danish"),
        ("de", "German"),
        ("es", "Spanish"),
        ("fr", "French"),
    ];
    let lang_items: Vec<CheckMenuItem<tauri::Wry>> = langs
        .iter()
        .map(|(code, label)| {
            CheckMenuItem::with_id(app, format!("lang:{code}"), *label, true, cfg.general.language == *code, None::<&str>)
        })
        .collect::<tauri::Result<_>>()?;
    let lang_refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
        lang_items.iter().map(|m| m as &dyn IsMenuItem<tauri::Wry>).collect();
    let language = Submenu::with_items(app, "Language", true, &lang_refs)?;

    // Input device: "Default" (empty id) plus every enumerated mic. Value is the
    // raw device name (may contain colons — the handler splits only on the
    // first), so the round-trip matches config.recording.input_device exactly.
    let mic_default = CheckMenuItem::with_id(
        app,
        "mic:",
        "Default",
        true,
        cfg.recording.input_device.is_empty(),
        None::<&str>,
    )?;
    let mic_items: Vec<CheckMenuItem<tauri::Wry>> = list_audio_devices()
        .into_iter()
        .map(|name| {
            let checked = cfg.recording.input_device == name;
            CheckMenuItem::with_id(app, format!("mic:{name}"), name.clone(), true, checked, None::<&str>)
        })
        .collect::<tauri::Result<_>>()?;
    let mut mic_refs: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&mic_default];
    mic_refs.extend(mic_items.iter().map(|m| m as &dyn IsMenuItem<tauri::Wry>));
    let microphone = Submenu::with_items(app, "Microphone", true, &mic_refs)?;

    // Save-audio toggle.
    let save_audio = CheckMenuItem::with_id(app, "save_audio:toggle", "Save recordings", true, cfg.recording.save_audio, None::<&str>)?;

    // Start in the tray (no bubble on launch) — the onboarding choice, editable
    // afterwards. Takes effect on the next launch; "Show Spoke" reveals the
    // bubble in the meantime.
    let start_tray = CheckMenuItem::with_id(app, "start_minimized:toggle", "Start in tray", true, cfg.ui.start_minimized, None::<&str>)?;

    // Master switch for the webview's sound effects. The sounds are played by
    // the bubble window (which stays alive in tray mode), so this works the same
    // whether or not the bubble is on screen.
    let sounds = CheckMenuItem::with_id(app, "sounds:toggle", "Sounds", true, cfg.ui.sounds, None::<&str>)?;

    let mut items: Vec<&dyn IsMenuItem<tauri::Wry>> = vec![&mode];

    // Model + Acceleration submenus (offline builds only). Mirrors the bubble's
    // engine card. `Model` manages every known model in one place: each model is
    // its own submenu — installed models offer "Use this model" + "Delete
    // download"; missing models offer "Download (<size>)". `accel` lists the
    // backends compiled into this binary plus "auto".
    #[cfg(feature = "whisper")]
    let (model_menu, accel_menu);
    #[cfg(feature = "whisper")]
    let _accel_items: Vec<CheckMenuItem<tauri::Wry>>;
    // All per-model children + submenus are kept alive until the enclosing
    // Settings menu is built, since the menu only borrows them during assembly.
    #[cfg(feature = "whisper")]
    let (_model_use, _model_act): (Vec<CheckMenuItem<tauri::Wry>>, Vec<MenuItem<tauri::Wry>>);
    #[cfg(feature = "whisper")]
    let _model_subs: Vec<Submenu<tauri::Wry>>;
    #[cfg(feature = "whisper")]
    {
        let known = ["tiny", "base", "small", "medium", "large-v3-turbo", "large-v3"];

        // Build every child item first so the backing vecs never reallocate
        // while a submenu holds a borrow into them, then assemble one submenu
        // per model by index.
        struct Row {
            name: &'static str,
            installed: bool,
            selected: bool,
            use_i: Option<usize>,
            act_i: usize,
        }
        let mut rows: Vec<Row> = Vec::new();
        let mut use_items: Vec<CheckMenuItem<tauri::Wry>> = Vec::new();
        let mut act_items: Vec<MenuItem<tauri::Wry>> = Vec::new();
        for m in known {
            let installed = stt::whisper::model_exists(m);
            let selected = cfg.offline.model == m;
            let use_i = if installed {
                use_items.push(CheckMenuItem::with_id(app, format!("model:{m}"), "Use this model", true, selected, None::<&str>)?);
                Some(use_items.len() - 1)
            } else {
                None
            };
            if installed {
                act_items.push(MenuItem::with_id(app, format!("modeldel:{m}"), "Delete download", true, None::<&str>)?);
            } else {
                let size = stt::whisper::model_size_label(m);
                act_items.push(MenuItem::with_id(app, format!("download:{m}"), format!("Download ({size})"), true, None::<&str>)?);
            }
            rows.push(Row { name: m, installed, selected, use_i, act_i: act_items.len() - 1 });
        }

        let mut subs: Vec<Submenu<tauri::Wry>> = Vec::new();
        for r in &rows {
            let size = stt::whisper::model_size_label(r.name);
            let status = if r.selected { " ✓" } else if r.installed { " •" } else { "" };
            let label = format!("{}{}  ·  {}", r.name, status, size);
            let mut children: Vec<&dyn IsMenuItem<tauri::Wry>> = Vec::new();
            if let Some(i) = r.use_i {
                children.push(&use_items[i]);
            }
            children.push(&act_items[r.act_i]);
            subs.push(Submenu::with_items(app, label, true, &children)?);
        }
        _model_use = use_items;
        _model_act = act_items;
        _model_subs = subs;
        let refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
            _model_subs.iter().map(|s| s as &dyn IsMenuItem<tauri::Wry>).collect();
        model_menu = Submenu::with_items(app, "Model", true, &refs)?;
        items.push(&model_menu);

        // Acceleration: "auto" plus every compiled backend (best-first, CPU last).
        let mut accel = vec![CheckMenuItem::with_id(
            app,
            "accel:auto",
            "Auto",
            true,
            cfg.offline.accel == "auto",
            None::<&str>,
        )?];
        for b in platform::compiled_backends() {
            accel.push(CheckMenuItem::with_id(
                app,
                format!("accel:{}", b.id),
                b.label,
                true,
                cfg.offline.accel == b.id,
                None::<&str>,
            )?);
        }
        _accel_items = accel;
        let refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
            _accel_items.iter().map(|m| m as &dyn IsMenuItem<tauri::Wry>).collect();
        accel_menu = Submenu::with_items(app, "Acceleration", true, &refs)?;
        items.push(&accel_menu);
    }

    // Hotkey presets. The bubble's UI has an interactive key-capture recorder
    // for custom binds; here we offer a curated set of common options.
    let hotkeys: &[(&str, &str)] = if cfg!(target_os = "macos") {
        &[
            ("cmd+shift+s", "⌘+Shift+S"),
            ("cmd+shift+space", "⌘+Shift+Space"),
            ("cmd+option+s", "⌘+Option+S"),
            ("cmd+shift+v", "⌘+Shift+V"),
            ("option+space", "⌥+Space"),
            ("ctrl+shift+space", "⌃+Shift+Space"),
        ]
    } else {
        &[
            ("ctrl+alt+space", "Ctrl+Alt+Space"),
            ("ctrl+alt+s", "Ctrl+Alt+S"),
            ("ctrl+shift+space", "Ctrl+Shift+Space"),
            ("ctrl+shift+s", "Ctrl+Shift+S"),
            ("ctrl+alt+v", "Ctrl+Alt+V"),
            ("ctrl+alt+grave", "Ctrl+Alt+`"),
        ]
    };
    let hotkey_items: Vec<CheckMenuItem<tauri::Wry>> = hotkeys
        .iter()
        .map(|(combo, label)| {
            CheckMenuItem::with_id(app, format!("hotkey:{combo}"), *label, true, cfg.general.hotkey == *combo, None::<&str>)
        })
        .collect::<tauri::Result<_>>()?;
    let hotkey_refs: Vec<&dyn IsMenuItem<tauri::Wry>> =
        hotkey_items.iter().map(|m| m as &dyn IsMenuItem<tauri::Wry>).collect();
    let hotkey_menu = Submenu::with_items(app, "Hotkey", true, &hotkey_refs)?;

    items.push(&trigger);
    items.push(&hotkey_menu);
    items.push(&output);
    items.push(&language);
    items.push(&microphone);
    let sep = PredefinedMenuItem::separator(app)?;
    items.push(&sep);
    items.push(&save_audio);
    items.push(&start_tray);
    items.push(&sounds);

    Submenu::with_items(app, "Settings", true, &items)
}

/// Route a tray menu click. Static ids restore/quit; `hist:<i>` copies a past
/// transcript; `<group>:<value>` edits config and refreshes the menu + UI.
fn handle_tray_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().as_ref();
    match id {
        "show" => {
            restore_from_tray(app);
            return;
        }
        "quit" => {
            app.exit(0);
            return;
        }
        _ => {}
    }

    if let Some(idx) = id.strip_prefix("hist:") {
        if let Ok(i) = idx.parse::<usize>() {
            let text = app
                .state::<Arc<SpokeState>>()
                .history
                .lock()
                .unwrap()
                .get(i)
                .cloned();
            if let Some(text) = text {
                tauri::async_runtime::spawn_blocking(move || {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        let _ = cb.set_text(&text);
                    }
                });
            }
        }
        return;
    }

    // Kick off a background model download. Progress + completion flow back
    // through the same events the bubble uses; `install_tray_download_feedback`
    // turns those into tray tooltip updates and a menu rebuild.
    #[cfg(feature = "whisper")]
    if let Some(model) = id.strip_prefix("download:") {
        let app = app.clone();
        let model = model.to_string();
        tauri::async_runtime::spawn(async move {
            if let Err(e) = download_model(app, model).await {
                eprintln!("[spoke] tray model download failed: {e}");
            }
        });
        return;
    }

    // Delete a downloaded model from the tray. delete_model refreshes the menu.
    #[cfg(feature = "whisper")]
    if let Some(model) = id.strip_prefix("modeldel:") {
        if let Err(e) = delete_model(app.clone(), model.to_string()) {
            eprintln!("[spoke] tray model delete failed: {e}");
        }
        return;
    }

    let Some((group, value)) = id.split_once(':') else { return };
    let mut cfg = app.state::<Arc<SpokeState>>().config_snapshot();
    let changed = apply_setting(&mut cfg, group, value);
    if changed {
        if let Err(e) = apply_config(app, cfg) {
            eprintln!("[spoke] tray config change failed: {e}");
        }
    }
}

/// Mutate `cfg` for a `<group>:<value>` tray id. Returns whether anything
/// changed (an unknown/duplicate selection is a no-op).
fn apply_setting(cfg: &mut Config, group: &str, value: &str) -> bool {
    use config::{Mode, OutputDest, Trigger};
    match group {
        "mode" => {
            let m = match value {
                "online" => Mode::Online,
                _ => Mode::Offline,
            };
            if cfg.general.mode == m {
                return false;
            }
            cfg.general.mode = m;
        }
        "hotkey" => {
            if cfg.general.hotkey == value {
                return false;
            }
            cfg.general.hotkey = value.to_string();
        }
        "trigger" => {
            let t = match value {
                "toggle" => Trigger::Toggle,
                _ => Trigger::PushToTalk,
            };
            if cfg.general.trigger == t {
                return false;
            }
            cfg.general.trigger = t;
        }
        "output" => {
            let o = match value {
                "copy" => OutputDest::Copy,
                "both" => OutputDest::Both,
                _ => OutputDest::Type,
            };
            if cfg.general.output_dest == o {
                return false;
            }
            cfg.general.output_dest = o;
        }
        "lang" => {
            if cfg.general.language == value {
                return false;
            }
            cfg.general.language = value.to_string();
        }
        "model" => {
            if cfg.offline.model == value {
                return false;
            }
            cfg.offline.model = value.to_string();
        }
        "accel" => {
            if cfg.offline.accel == value {
                return false;
            }
            cfg.offline.accel = value.to_string();
        }
        "mic" => {
            if cfg.recording.input_device == value {
                return false;
            }
            cfg.recording.input_device = value.to_string();
        }
        "save_audio" => {
            cfg.recording.save_audio = !cfg.recording.save_audio;
        }
        "start_minimized" => {
            cfg.ui.start_minimized = !cfg.ui.start_minimized;
        }
        "sounds" => {
            cfg.ui.sounds = !cfg.ui.sounds;
        }
        _ => return false,
    }
    true
}

/// Show the bubble again and reset the tray to its neutral icon.
fn restore_from_tray(app: &AppHandle) {
    app.state::<Arc<SpokeState>>()
        .bubble_hidden
        .store(false, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window("bubble") {
        let _ = win.show();
        let _ = win.set_focus();
        // A bubble that booted straight into tray mode has never been mapped,
        // and WebKitGTK doesn't always present a first frame for a webview that
        // was created hidden. Bounce the size by a pixel to force one; the
        // bubble re-applies its own geometry from JS on the next frame anyway.
        #[cfg(target_os = "linux")]
        if let Ok(size) = win.outer_size() {
            let _ = win.set_size(tauri::PhysicalSize::new(size.width + 1, size.height));
            let _ = win.set_size(size);
        }
    }
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Some(icon) = app.default_window_icon().cloned() {
            let _ = tray.set_icon(Some(icon));
        }
    }
    // Tell the UI it's no longer minimized so it stops pushing tray colors.
    let _ = app.emit("spoke:restored", ());
}

/// Hide every window; the app lives only in the tray until restored.
#[tauri::command]
fn minimize_to_tray(app: AppHandle) {
    app.state::<Arc<SpokeState>>()
        .bubble_hidden
        .store(true, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window("bubble") {
        let _ = win.hide();
    }
}

/// Mark first-run onboarding as complete: flip the `onboarded` flag (which also
/// arms the hotkey via `apply_config`), create the tray icon, close the
/// onboarding card, and reveal the bubble (unless the user chose to start in the
/// tray). Until this runs the card is the only part of Spoke on screen. The
/// onboarding UI has already written the other prefs via `set_config`; setting
/// the flag here — rather than trusting the client — keeps it a one-shot.
#[tauri::command]
fn finish_onboarding(app: AppHandle) -> Result<(), String> {
    let state = app.state::<Arc<SpokeState>>();
    let mut cfg = state.config_snapshot();
    cfg.ui.onboarded = true;
    apply_config(&app, cfg.clone())?;

    // The tray was withheld while the card was up so onboarding was the only
    // thing on screen; bring it up now, before any tray state is applied.
    if let Err(e) = build_tray(&app) {
        eprintln!("[spoke] tray creation failed: {e}");
    }

    if let Some(win) = app.get_webview_window("onboard") {
        let _ = win.close();
    }
    state
        .bubble_hidden
        .store(cfg.ui.start_minimized, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window("bubble") {
        if cfg.ui.start_minimized {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
    if cfg.ui.start_minimized {
        // Nothing on screen from here on: put the tray icon into its idle
        // colour so the user can tell Spoke is running.
        apply_tray_state(&app, "idle");
    }
    Ok(())
}

/// Recolor the tray icon to reflect app state:
/// gray = idle, red = recording, blue = processing, yellow = warning.
fn apply_tray_state(app: &AppHandle, state: &str) {
    let color: [u8; 3] = match state {
        "recording" => [0xE5, 0x39, 0x35],
        "processing" => [0x1E, 0x88, 0xE5],
        "warning" => [0xFD, 0xD8, 0x35],
        _ => [0x9E, 0x9E, 0x9E],
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(colored_tray_icon(color)));
    }
}

/// UI-driven tray recoloring while the bubble is minimized to the tray.
#[tauri::command]
fn set_tray_state(app: AppHandle, state: String) {
    apply_tray_state(&app, &state);
}

/// Build a filled, anti-aliased circle of the given RGB as a tray icon.
fn colored_tray_icon(rgb: [u8; 3]) -> Image<'static> {
    const SIZE: u32 = 32;
    let r = SIZE as f32 / 2.0;
    let edge = r - 1.5; // start of the anti-aliased rim
    let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - r;
            let dy = y as f32 + 0.5 - r;
            let dist = (dx * dx + dy * dy).sqrt();
            let a = if dist <= edge {
                1.0
            } else if dist >= r {
                0.0
            } else {
                (r - dist) / (r - edge)
            };
            let i = ((y * SIZE + x) * 4) as usize;
            buf[i] = rgb[0];
            buf[i + 1] = rgb[1];
            buf[i + 2] = rgb[2];
            buf[i + 3] = (a * 255.0) as u8;
        }
    }
    Image::new_owned(buf, SIZE, SIZE)
}

/// Park the bubble in the bottom-right of the primary monitor.
fn position_bubble(app: &AppHandle) {
    use tauri::{LogicalPosition, PhysicalSize};
    if let Some(win) = app.get_webview_window("bubble") {
        if let (Ok(Some(monitor)), Ok(PhysicalSize { width, height })) =
            (win.current_monitor(), win.outer_size())
        {
            let scale = monitor.scale_factor();
            let screen = monitor.size().to_logical::<f64>(scale);
            let win_w = width as f64 / scale;
            let win_h = height as f64 / scale;
            let margin = 24.0;
            let x = screen.width - win_w - margin;
            let y = screen.height - win_h - margin;
            let _ = win.set_position(LogicalPosition::new(x, y));
        }
    }
}
