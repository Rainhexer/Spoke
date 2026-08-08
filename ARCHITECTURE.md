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
- **Sounds** — the UI cues (`ui/sounds/*.mp3`, played by `ui/sounds.js`) are a
  webview concern; Rust owns only the `ui.sounds` master switch, surfaced in the
  Output card and the tray Settings submenu. The bubble window is hidden rather
  than closed in tray mode, so its `spoke:state` cues keep firing off-screen.
  On Linux, WebKitGTK decodes them through GStreamer: the packages depend on
  `gst-plugins-{base,good}` and the AppImage sets `bundleMediaFramework` so the
  plugins travel with it — without them the app is simply silent, no error.

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
  at least NVIDIA): the window's buffer is never cleared between frames, every
  repaint is composited source-over onto what is already there. Two
  consequences: a translucent pixel repainted in place blends with itself and
  creeps toward opaque (a drop shadow darkens frame by frame until WebKit's
  periodic full redraw, ~10 s, resets it), and pixels a moving element vacates
  are never repainted, so the old frame stays on screen as a trail.

  Measured against a repainting test page, reading the window's own ARGB
  buffer with `XGetImage`: shadow alpha climbed 37 → 152 over 12 s, and an
  opaque box sliding across the window smeared over its whole path. None of
  `WEBKIT_DISABLE_DMABUF_RENDERER`, `WEBKIT_DISABLE_COMPOSITING_MODE`,
  `LIBGL_ALWAYS_SOFTWARE`, layer promotion (`will-change`),
  `webkit_web_view_set_background_color`, `gtk_widget_queue_draw`,
  `XClearArea`, `size_allocate` with an unchanged rect, an unchanged
  `set_size_request`, webview hide/show, or the damage-propagation feature
  flags changed either number.

  **What does clear it is a size change on the webview widget.** That is the
  repaint clock: `set_repaint_clock` (lib.rs) runs a 60 Hz GTK timeout that
  allocates the webview one pixel taller every other frame, and both numbers
  drop to their clean values and stay there. Use `size_allocate`, not
  `set_size_request` — a size request is a *minimum*, so GTK propagates it up
  and the toplevel creeps a pixel taller every tick. Unlike a toplevel resize
  this never reaches the window manager: no geometry change, no gravity dance,
  no flutter, nothing to fight a drag grab.

  main.js runs the clock while anything is animating — `startRepaint()` at the
  top of every menu transition, `stopRepaintSoon()` (900 ms tail, so the CSS
  transition the change started is fully covered) plus pointer/scroll
  keep-alives while the menu is open. That is what lets the menu's open/close
  animation, hover feedback and card transitions run on Linux at all.

  The clock only covers *motion*, though. Between runs, a translucent pixel
  repainted in place still creeps toward opaque — so the second half of the fix
  stands: **everything over the desktop is opaque.** No shadows, glows or
  partial opacity outside `#subcard` (translucency *inside* the card is fine,
  its background is opaque), enforced by the `html.linux` block in style.css;
  inactive orbit bubbles dim by colour rather than opacity. The bubble follows
  the same rule in canvas — `shapeR()` returns a fixed radius on Linux so the
  idle silhouette never moves (interior tiles still animate; they land on the
  blob's opaque fill), the press/pop squish is off, and the CSS drop-shadow
  halo is replaced by an opaque two-tone rim stroked into the canvas. macOS and
  Windows keep the full shadowed design; every one of these rules is
  `html.linux`- or `IS_LINUX`-gated.

  Because the clock makes the viewport a pixel taller than the window every
  other frame, **nothing may be laid out against the viewport** — everything is
  absolutely positioned inside `#root`, which `setRootSize()` pins to the
  window's exact logical size.

  `disable_damage_propagation()` in lib.rs additionally asks WebKit for full
  frames rather than damaged rects, through the WebKitSettings feature-list C
  API (called over FFI — the webkit2gtk crate's bindings predate it). It is
  insurance, not the cure; note the flag was renamed
  (`PropagateDamagingInformation` ≤ 2.48, `UseDamagingInformationForCompositing`
  ≥ 2.50) and matching only the old name made this a silent no-op for a while.

  Don't set `WEBKIT_DISABLE_COMPOSITING_MODE` — it doesn't fix anything and adds
  its own artifacts. The window is also made resizable at runtime on Linux: GTK
  snaps non-resizable windows back to the webview's ~200×200 natural size.
- **Linux keeps the bubble window at one size for its whole life.** It is
  created menu-sized (340×480, matching `MENU_W`/`MENU_H` in main.js) and never
  resized; opening the menu only changes what is drawn. Growing the window on
  open is what used to put a black rectangle on screen for a frame — the newly
  exposed area is mapped before WebKit has painted it — and what forced the
  gravity-anchored resize (a plain move gets clamped by the WM near a screen
  edge, walking the bubble). Click-through comes from an input region instead:
  `set_input_region` (lib.rs) shapes the window to a circle around the blob
  while the menu is closed and to the whole window while it is open (the empty
  area around the ring is what closes it), and to nothing at all while the
  bubble is hidden. `linuxLayout()` in main.js therefore only ever *moves* the
  window, and only when a flip changes which corner the bubble sits in.

  The region is set on the *GTK widget*, not on the GdkWindow: GTK remembers a
  widget's region and re-applies it on realize and on every allocation, while
  one pushed onto the GdkWindow is dropped by the next map or resize. That drop
  was issue #6 — a bubble booted straight into the tray stayed invisible but
  went on eating clicks in the middle of the screen, and only showing, moving
  and re-hiding it (which re-applied the empty region with nothing left to
  clear it) gave the clicks back. `watch_hit_shape` puts the region back on
  `map` and on real geometry changes as a second line of defence.
- **macOS and Windows have no input region**, so click-through there is a
  cursor poll (`spawn_cursor_hit_test`, 50 ms): the desktop cursor position is
  tested against the same shape and the whole window is flipped between
  `set_ignore_cursor_events(true)` and `false` as the pointer enters and leaves
  the blob. It has to poll — a window ignoring the cursor gets no mouse events
  to react to — and anything unknown (no region yet, no cursor position) counts
  as a hit, since a window taking clicks it shouldn't beats a bubble that can't
  be clicked at all.
- **Linux black flash on reveal**: a transparent undecorated window is on
  screen before WebKit has drawn anything, and what the compositor shows
  meanwhile is the webview's uninitialised backing store — a solid black
  rectangle for ~450 ms at startup. The bubble window is therefore never mapped
  visible on Linux: it is mapped at zero opacity with an empty input region
  (`set_bubble_visible`), and revealed by the `bubble_painted` command that
  main.js fires after its first canvas frame (with a 2.5 s watchdog so a JS
  error can't leave the user with no bubble). "Hidden to tray" uses the same
  zero-opacity state rather than `hide()`: WebKitGTK only renders into a mapped
  window, so an unmapped bubble has no frame ready when it comes back.

  A zero-opacity window isn't composited either, so its buffer still holds
  whatever was last drawn — at boot, the black it was created with. Raising the
  opacity before WebKit has redrawn *is* the flash. So `reveal_bubble` starts
  the repaint clock first, waits 60 ms for a frame to land, and only then turns
  the opacity up. The
  toplevel additionally gets `set_app_paintable` plus a transparent X
  background (`gdk_window_set_background_rgba` via ffi — the rust binding is
  gated) and a `draw` handler that clears with cairo operator SOURCE, and the
  webview's own GdkWindow gets the same treatment.
- **Linux window moves**: the window never resizes (above), so a flip is the
  only thing that moves it, and the move must keep the bubble on the same
  screen pixel. If a resize is ever reintroduced here, note the trap it used to
  hit: a resize+move pair gets the move validated by the WM against the *old*
  size, so near screen/monitor edges KWin clamps it and the bubble walks — the
  old `set_window_size_anchored` worked around that with ICCCM win-gravity.
  Never use gdk `move_resize`: it resizes the X window behind GTK's back and the
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
