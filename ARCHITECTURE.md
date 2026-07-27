# Spoke Architecture

How Spoke works — the global pipeline shared by every platform, and the parts
that differ per platform.

---

## The global pipeline

Identical on macOS, Linux, and Windows:

```
global hotkey pressed
        ↓
cpal opens the microphone → PCM buffer (Vec<f32>, device native rate)
        ↓  (amplitude streamed to the UI for the bubble animation)
hotkey released
        ↓
downmix to mono → strip/compress silence → [optional: save WAV]
        ↓
STT engine (one of two, chosen by config):
   ├─ Whisper (offline)  — whisper.cpp via whisper-rs, 16 kHz mono
   └─ Google  (online)   — Speech-to-Text v1 REST, base64 LINEAR16
        ↓
transcript: String
        ↓
   ├─ enigo types it into the focused window   (output_dest = "type", default)
   ├─ copied to the clipboard                  (output_dest = "copy")
   └─ or both                                  (output_dest = "both")
```

The UI is a single transparent always-on-top Tauri window ("the bubble"),
plain HTML/CSS/JS — no framework, no bundler. Rust and the UI talk over Tauri
commands (UI → Rust) and events (Rust → UI: state, amplitude, transcript,
download progress).

### Module map (`src-tauri/src/`)

| File | Responsibility |
|---|---|
| `lib.rs` | Glue: hotkey → capture → STT → injection; Tauri commands/events; model downloads |
| `platform.rs` | **Build-target description**: OS name, compiled backends, compile-time guards |
| `permissions.rs` | OS permission checks (macOS: mic TCC + Accessibility) feeding the UI warning banner |
| `config.rs` | `spoke.toml` schema, defaults, load/save |
| `audio.rs` | cpal capture thread, mono downmix, resampling, silence stripping, WAV save |
| `hotkey.rs` | `"ctrl+alt+space"` → global shortcut parsing |
| `inject.rs` | enigo keyboard-simulation injection |
| `stt/mod.rs` | `SttEngine` enum — one interface over both backends |
| `stt/whisper.rs` | whisper.cpp engine, model paths/URLs, CoreML bundle toggling |
| `stt/google.rs` | Google STT v1 REST client |

Cross-cutting details worth knowing:

- **Engine caching** — building the Whisper engine loads the whole model into
  RAM *and* creates the whisper.cpp state (Metal backend init + CoreML encoder
  load; the first-ever ANE specialization of a model can take minutes), so
  `SpokeState` caches the engine keyed on the engine-relevant config fields
  (`EngineKey`) and rebuilds only when those change. The `WhisperState` lives
  inside `WhisperStt` behind a `Mutex` and is reused across transcriptions —
  never recreate it per run (that re-pays the Metal/CoreML init every
  recording). `FullParams` sets `no_context(true)` so the reused KV cache
  doesn't bleed the previous transcript into the next one. `prewarm_engine`
  builds the engine in the background at startup and after config saves so the
  first recording doesn't pay the init cost; whisper.cpp inference runs on a
  `spawn_blocking` thread so it never stalls async executor workers.
- **Session counter** — every recording bumps an atomic counter; a pipeline
  checks it before injecting, so re-triggering cancels stale in-flight
  transcriptions instead of typing old text late.
- **Memory** — on glibc Linux, `malloc_trim` runs after each transcription to
  return freed heap pages to the OS (glibc arenas otherwise ratchet RSS up).

---

## The platform system

A Spoke binary is built **for exactly one platform** and contains only that
platform's technology. Three layers enforce and surface this:

### 1. Cargo features (build time)

`Cargo.toml` defines capability flags (`whisper`, `metal`, `coreml`, `cuda`,
`vulkan`) and platform presets (`platform-macos`, `platform-linux-cuda`, …)
that bundle them. Heavy dependencies are tied to the flags that need them —
`whisper-rs` and `futures-util` only exist in `whisper` builds, `zip` only in
`coreml` builds. A build without a flag contains none of that flag's code or
dependencies.

There is no headless build variant: every build ships the bubble UI. Running
tray-only is a runtime preference (`ui.start_minimized`, chosen at onboarding)
— the bubble window is still created in `setup()` (programmatically, not in
`tauri.conf.json`) but starts hidden, `emit_state` recolors the tray whenever
`SpokeState.bubble_hidden` is set (tray mode at launch, or minimize-to-tray —
tracked explicitly because GTK misreports `is_visible()` for a window that was
built hidden), downloads report via desktop notifications, and
`ExitRequested` without an exit code is always prevented so a hidden bubble
never lets the process die.

First run is the one boot where `setup()` puts nothing else on screen: with
`ui.onboarded` false the tray icon isn't created and the hotkey isn't
registered, so the onboarding card is the whole app until "Start Spoke".
`finish_onboarding` then flips the flag (arming the hotkey through
`apply_config`), builds the tray, and shows the bubble unless the user picked
tray-only.

### 2. `platform.rs` (single source of truth)

- **Compile-time guards**: `compile_error!` rejects impossible combinations
  (Metal/CoreML off macOS, CUDA/Vulkan on macOS). A mis-targeted build fails
  loudly instead of shipping dead code.
- **Backend catalogue**: `compiled_backends()` returns the backends this
  binary actually contains, best first, always ending with the CPU fallback.
- **`build_info()`**: serializes OS name, whisper availability, the best
  backend, and the full backend list for the UI.

### 3. The UI (runtime)

On startup the UI calls `get_build_info` and renders from it:

- The **badge** shows the effective backend for the current setting.
- The **Accel** dropdown lists *Auto* + every compiled backend + *CPU only* —
  it never shows a backend the binary doesn't have.
- The **ANE bundle** row appears only in CoreML builds.

The user's choice is stored in `config.offline.accel`
(`"auto" | "metal" | "coreml" | "cuda" | "vulkan" | "none"`; the old
`mac_accel` name is still accepted when reading). A value from a different
build (e.g. a config written by a CUDA build opened in a Metal build) falls
back to *Auto*.

### How the choice takes effect

- **CPU vs GPU**: `accel = "none"` builds the Whisper context with
  `use_gpu = false`; anything else enables the compiled GPU backend.
- **CoreML vs Metal** (macOS): whisper.cpp uses a CoreML encoder bundle
  automatically if it sits next to the model file. Spoke toggles this by
  renaming the bundle to `.disabled` when the user selects Metal, and back
  when they select CoreML/Auto — a runtime switch with no rebuild.
- Switching accel (or model, or mode) invalidates the cached engine;
  `prewarm_engine` rebuilds it in the background right after the config save
  (falling back to a lazy rebuild on the next transcription).

### Adding a new backend

1. Add a cargo feature in `Cargo.toml` mapping to the `whisper-rs` feature
   (and a platform preset if it defines a new shipping target).
2. Add a `Backend` entry (+ OS guard if needed) in `platform.rs`.
3. Done — the UI dropdown, badge, and config handling pick it up from
   `build_info()`; no UI changes required.

---

## Per-platform technology

| | macOS | Linux | Windows |
|---|---|---|---|
| Audio capture (cpal) | CoreAudio | ALSA (works with PipeWire/PulseAudio) | WASAPI |
| Text injection (enigo) | CGEvent (needs Accessibility permission) | X11 / Wayland virtual keyboard | SendInput |
| Global hotkey | Carbon event tap (via tauri plugin) | X11 grab / compositor protocol | RegisterHotKey |
| Webview | WKWebView (built-in) | WebKitGTK | WebView2 |
| Whisper acceleration | CoreML (Neural Engine), Metal (GPU) | CUDA, Vulkan | CUDA, Vulkan |
| Config dir | `~/Library/Application Support/spoke/` | `~/.config/spoke/` | `%APPDATA%\spoke\` |
| Default hotkey | `cmd+shift+s` | `ctrl+alt+space` | `ctrl+alt+space` |
| Bundle formats | `.app`, `.dmg` | `.deb`, `.rpm`, `.AppImage` | `.msi`, `.exe` (NSIS) |

### Platform quirks handled in code

- **Linux/WebKitGTK**: `GDK_BACKEND` defaults to `x11` (XWayland on Wayland
  sessions) because native Wayland can't report/set global window positions —
  that breaks the bubble's edge-aware menu flipping. If `GDK_BACKEND` is
  forced to `wayland`, a 1 px "repaint nudge" resize forces the compositor to
  present panel updates (no-op on X11).
- **Linux transparent-window repaints don't erase** (all WebKitGTK modes on
  at least NVIDIA): repaints blend OVER the stale buffer instead of replacing
  it, so translucent pixels (drop shadows, fading elements) stack darker each
  frame and moving elements leave trails; only a window resize swaps in a
  clean buffer. Root cause (found later): WebKitGTK ≥ 2.50 ships the
  "damage propagation" feature (`PropagateDamagingInformation`) enabled by
  default — only damaged rectangles get presented, and on a transparent
  window their translucent pixels blend over the previous frame instead of
  replacing it, until the next full redraw resets the cycle.
  `disable_damage_propagation()` in lib.rs turns the feature off through the
  WebKitSettings feature-list C API (called over FFI — the webkit2gtk crate's
  bindings predate it). Countermeasure layer (Linux-gated): menu transitions are disabled
  in CSS (`html.linux` block — state changes are instant, hover feedback
  avoids shadow changes), and every discrete menu change is followed by one
  1px gravity-anchored "buffer swap" resize (`nudgeOnce`/`presentFrame` in
  main.js); the close shrink is immediate. Card scrolling would repaint the
  card's translucent shadow every tick, so on Linux the card uses a border
  instead and a debounced buffer swap runs when scrolling settles. The
  menu-open resize used to flash black (X fills exposed regions with the
  window background before WebKit paints); fixed by `set_app_paintable` plus
  a transparent X background set through `gdk_window_set_background_rgba`
  (via ffi — the rust binding is gated). Don't run a per-frame resize loop
  instead — it jiggles visibly and fights the WM's drag grab. Don't set
  `WEBKIT_DISABLE_COMPOSITING_MODE` / `WEBKIT_DISABLE_DMABUF_RENDERER` — they
  don't fix it and add their own artifacts. The window is also made resizable
  at runtime on Linux: GTK snaps non-resizable windows back to the webview's
  ~200×200 natural size, overriding the 80×80 bubble size.
- **Linux window resize (`set_window_size_anchored`)**: menu open/close must
  keep the bubble's screen position fixed. A resize+move pair gets the move
  validated by the WM against the *old* size, so near screen/monitor edges
  KWin clamps it and the bubble walks. Instead the command sets ICCCM
  win-gravity to the bubble's corner (from the flip state) and sends a
  resize only — the WM keeps that corner pinned. Never use gdk
  `move_resize` here: it resizes the X window behind GTK's back and the
  webview keeps painting only the old area.
- **Linux/ALSA**: device enumeration is filtered (no `hw:`/`dmix:` pseudo
  devices) and runs on a timeout thread, since misconfigured backends can
  block indefinitely. Capture prefers `pulse`/`pipewire`/`default`.
- **Linux/glibc**: `malloc_trim` after each transcription (see above).
- **macOS**: the bubble is marked visible on all Spaces; the private-API flag
  gives the transparent window proper behavior.
- **macOS permissions**: the UI polls `check_permissions` (AVCaptureDevice
  TCC status + `AXIsProcessTrusted`) and shows an amber `!` on the bubble plus
  a banner in the panel when Microphone or Accessibility is missing. Other
  platforms report `unknown` and never warn — extend `permissions.rs` if one
  grows a queryable API. The Accessibility warning is suppressed in clipboard
  mode, which doesn't inject keystrokes. Granting flows:
  - *Microphone*: `request_microphone_permission` fires the native
    `requestAccessForMediaType:` prompt — a grant applies to the running
    process immediately. Recording refuses to start while the permission is
    undetermined/denied (otherwise the OS prompt appears mid-dictation and the
    capture is silence). If previously denied, the UI offers "Ask me again"
    (`reset_permission` → `tccutil reset` → re-prompt) or System Settings.
  - *Accessibility*: `request_accessibility_permission`
    (`AXIsProcessTrustedWithOptions` with prompt) registers the current binary
    with TCC before opening System Settings. Because ad-hoc-signed builds
    change their code hash every rebuild, an old grant can show as enabled in
    Settings while the OS denies the new binary — the "Already on? Fix it"
    button resets the stale entry and re-registers. After any grant action the
    UI polls at 1.5 s (baseline 15 s) so the warning clears within seconds,
    and offers a one-click `restart_app` for the cases where only a fresh
    process picks the grant up. Signing release builds with a stable identity
    avoids the stale-grant problem entirely.

### Why the binary is self-contained

whisper.cpp and all GGML backends are **statically linked** into the Spoke
executable — there are no whisper/GGML shared libraries to bundle or install:

- **macOS**: Metal compute kernels are embedded in the binary
  (`GGML_METAL_EMBED_LIBRARY`); Metal/CoreML/Accelerate are OS frameworks.
  What a bundled app *does* need is permission metadata:
  `src-tauri/Info.plist` (microphone usage description — without it macOS
  silently denies mic access to the .app) and `src-tauri/entitlements.plist`
  (audio-input, for hardened-runtime signing).
- **Windows**: the MSVC C runtime is statically linked
  (`src-tauri/.cargo/config.toml` sets `+crt-static`), so no VC++
  Redistributable is required. CUDA builds link the CUDA runtime statically —
  users only need the normal NVIDIA driver.
- **Linux**: CUDA runtime static, same driver-only story; Vulkan uses the
  system loader (`libvulkan.so.1`, part of every desktop's GPU stack). GTK/
  WebKitGTK are declared as package dependencies in the `.deb`/`.rpm` and
  bundled into the `.AppImage`.

The only runtime artifacts are the models, which the app downloads itself.

---

## Model management

Models are **not** bundled into installers. Both downloads stream from
Hugging Face (`ggerganov/whisper.cpp`) with progress events to the UI:

- **GGML model** (`ggml-<name>.bin`) — required for offline mode. Downloaded
  to a `.tmp` file and renamed on completion, so an interrupted download can
  never masquerade as an installed model.
- **CoreML encoder bundle** (`ggml-<name>-encoder.mlmodelc`, macOS CoreML
  builds only) — downloaded as a zip and extracted next to the model
  (extraction validates entry paths against zip path traversal).

Lookup order at runtime: `src-tauri/models/` (dev convenience) first, then
`<config dir>/spoke/models/`. whisper.cpp finds the CoreML bundle by naming
convention — no path configuration.

Models are managed in one place — the bubble's Model section and the tray's
Settings → Model submenu both let you use, download (with size), or **delete**
a model. Deletion (`delete_model` command → `whisper::delete_model`) only
removes `ggml-<name>.bin` from the runtime `<config dir>/spoke/models/` dir; it
validates the model name to a safe charset and confines the path to that dir,
and never touches the read-only `src-tauri/models/` build copy. Download
success/failure and deletion also raise a desktop notification (via
`tauri-plugin-notification`, fired from Rust), so status still reaches the user
when the bubble is hidden in the tray.

Online mode needs no models: audio is sent as one batch REST request to
Google Speech-to-Text v1 with the API key from config.

---

## Configuration

One file, `spoke.toml`, in the OS config dir. Every field has a default, so a
missing or partial file always works. Engine-relevant fields (mode, model,
accel, use_gpu, provider, api_key) invalidate the cached engine on change;
the hotkey re-registers immediately on save. Schema lives in
`src-tauri/src/config.rs` with the documented sample in
[SPOKE.md](SPOKE.md#configuration).
