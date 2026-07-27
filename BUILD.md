# Building Spoke

Spoke is built per platform: you pick one **platform preset** flag, and the
resulting binary contains exactly the acceleration backends that platform
supports — nothing else. Impossible combinations (e.g. Metal on Linux) are
rejected at compile time, so a build can never carry another platform's code.

The output is a normal installer/bundle for the target OS. Speech models are
**not** bundled — the app downloads them on demand (see
[ARCHITECTURE.md](ARCHITECTURE.md#model-management)), which keeps installers
small while staying fully self-contained after the first model download.

---

## Platform presets

Release builds should use one of these single flags:

| Preset | OS | Backends compiled in |
|---|---|---|
| `platform-macos` | macOS (Apple Silicon) | CoreML + Metal + CPU |
| `platform-linux-cuda` | Linux | CUDA + CPU |
| `platform-linux-vulkan` | Linux | Vulkan + CPU |
| `platform-linux-cpu` | Linux | CPU |
| `platform-windows-cuda` | Windows | CUDA + CPU |
| `platform-windows-vulkan` | Windows | Vulkan + CPU |
| `platform-windows-cpu` | Windows | CPU |

```sh
cargo tauri build --features platform-macos
```

The user picks the active backend at runtime in the settings panel
(**Accel** row); *Auto* selects the best one compiled in.

### Individual capability flags

Presets are just bundles of these, for finer control:

| Flag | Description | Platform |
|---|---|---|
| `whisper` | Offline transcription via whisper.cpp | All |
| `metal` | Apple GPU acceleration (implies `whisper`) | macOS only |
| `coreml` | Apple Neural Engine acceleration (implies `whisper`) | macOS only |
| `cuda` | NVIDIA GPU acceleration (implies `whisper`) | Linux/Windows |
| `vulkan` | AMD/Intel/NVIDIA GPU via Vulkan (implies `whisper`) | Linux/Windows |

No flags at all = online-only build (Google STT, no local models).

### Tray-only running (no build flag)

Running without the bubble is a runtime preference, not a build variant. Every
build ships the bubble UI; choosing **Start in the tray** during onboarding (or
`start_minimized = true` in `spoke.toml`) launches Spoke straight into the tray
with no window on screen. The tray icon carries the status colours (grey idle,
red recording, blue processing, yellow warning), the tray menu holds the
transcript history and all main settings, model downloads report via desktop
notifications, and **Show Spoke** reveals the bubble whenever it's wanted.

---

## Automated release builds

Publishing a GitHub release triggers
[`.github/workflows/release.yml`](.github/workflows/release.yml), which builds
**all seven platform presets** and attaches every installer to the release,
tagged with its preset (e.g. `_linux-vulkan`, `_windows-cuda`). A manual run
from the Actions tab
builds everything without uploading, for testing the pipeline.

---

## Prerequisites (all platforms)

- Rust 1.77+ (`rustup update`)
- Tauri CLI: `cargo install tauri-cli --version "^2"`
- For any `whisper` build: CMake 3.20+ and a C/C++ toolchain
  (whisper.cpp is compiled from source during the build)

---

## macOS (Apple Silicon)

**Prerequisites:** Xcode command-line tools — `xcode-select --install`

```sh
cargo tauri build --features platform-macos
```

This compiles in both CoreML (Apple Neural Engine — fastest for clips >3 s)
and Metal (GPU). The settings panel offers *Auto*, *CoreML*, *Metal*, and
*CPU only*; the CoreML encoder bundle is downloaded in-app on first use
(settings → **ANE bundle → Get**).

**Output:** `src-tauri/target/release/bundle/` — `Spoke.app` and a `.dmg`.

### macOS permissions (release bundles)

The bundle build merges `src-tauri/Info.plist` (microphone usage description)
into the app and signs with `src-tauri/entitlements.plist` (audio-input).
Without the usage description macOS silently denies microphone access to a
bundled app — dev builds don't hit this because the terminal's own mic
permission covers `cargo tauri dev`. Don't remove these files.

On first launch, grant when prompted (System Settings → Privacy & Security):

- **Microphone** — recording
- **Accessibility** — lets Spoke type the transcript into other apps

> **Rebuild gotcha:** local builds are ad-hoc signed, and every rebuild gets a
> new signature — macOS then treats it as a new app and silently drops the old
> Microphone/Accessibility grants. If a rebuilt Spoke.app stops hearing you or
> stops typing, remove and re-add it in both permission lists.

### macOS (Intel)

whisper.cpp's Metal/CoreML paths target Apple Silicon; build CPU-only:

```sh
cargo tauri build --features whisper
```

---

## Linux

### System dependencies

**Debian / Ubuntu:**
```sh
sudo apt install build-essential curl wget git cmake pkg-config \
    libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
    librsvg2-dev libssl-dev libasound2-dev \
    patchelf gstreamer1.0-plugins-base gstreamer1.0-plugins-good
```

**Arch:**
```sh
sudo pacman -S --needed base-devel curl wget git cmake \
    webkit2gtk-4.1 gtk3 libappindicator-gtk3 librsvg \
    openssl pkgconf alsa-lib pipewire-alsa \
    patchelf gst-plugins-base gst-plugins-good
```

WebKitGTK plays the UI sounds through GStreamer, so the plugin packages are a
runtime requirement, not just a build one — without them the app runs fine but
stays silent. `patchelf` is needed only to bundle those plugins into the
AppImage (`bundleMediaFramework` in `tauri.conf.json`); the AppImage build fails
with `Failed to run plugin: gstreamer` if it is missing.

### Build

**NVIDIA GPU (CUDA)** — needs CUDA Toolkit 11.8+ (`nvcc` on PATH):
```sh
cargo tauri build --features platform-linux-cuda
```
If `nvcc` is missing the whisper.cpp build fails; install the toolkit
(`sudo pacman -S cuda` / NVIDIA's apt repo) or use Vulkan instead.

**Any GPU (Vulkan)** — needs Vulkan headers
(`sudo apt install libvulkan-dev` / `sudo pacman -S vulkan-devel`):
```sh
cargo tauri build --features platform-linux-vulkan
```

**CPU only:**
```sh
cargo tauri build --features platform-linux-cpu
```

**Output:** `src-tauri/target/release/bundle/` — `.deb`, `.rpm`, and
`.AppImage`. The AppImage is the single-file option: one executable that runs
on any distro, no install step.

### Installing

- **Debian/Ubuntu:** `sudo apt install ./src-tauri/target/release/bundle/deb/Spoke_0.2.0_amd64.deb`
- **Fedora/openSUSE:** `sudo rpm -i src-tauri/target/release/bundle/rpm/Spoke-0.2.0-1.x86_64.rpm`
- **Any distro:** `chmod +x Spoke_0.2.0_amd64.AppImage && ./Spoke_0.2.0_amd64.AppImage`
- **Arch:** no native pacman target; use the AppImage, or install the deb's
  contents manually:
  ```sh
  B=src-tauri/target/release/bundle/deb/Spoke_0.2.0_amd64/data
  sudo install -Dm755 $B/usr/bin/spoke /usr/local/bin/spoke
  sudo install -Dm644 $B/usr/share/applications/Spoke.desktop /usr/share/applications/Spoke.desktop
  for s in 32x32 128x128; do
    sudo install -Dm644 $B/usr/share/icons/hicolor/$s/apps/spoke.png /usr/share/icons/hicolor/$s/apps/spoke.png
  done
  sudo gtk-update-icon-cache -f /usr/share/icons/hicolor/
  ```

### Wayland note

Global hotkeys and text injection are most reliable on X11. On Wayland the
compositor must support `zwp_virtual_keyboard_v1`. If the hotkey doesn't
register: `GDK_BACKEND=x11 spoke`.

---

## Windows — step-by-step setup

Run each verification command before proceeding.

### 1. Visual Studio 2022 Build Tools (MSVC)

Rust's MSVC target and whisper.cpp's C/C++ compiler.

1. Download: https://visualstudio.microsoft.com/downloads/#build-tools-for-visual-studio-2022
2. Select **Desktop development with C++**
3. Under **Individual components**, ensure a **Windows 10/11 SDK** is included

**Verify:** `cl.exe` prints the compiler version.

### 2. CMake

1. Download the Windows x64 installer: https://cmake.org/download/
2. During install, **check "Add CMake to the system PATH"**

**Verify:** `cmake --version`

Missed the PATH option? Add `C:\Program Files\CMake\bin` to `Path`, or
session-only: `$env:Path += ";C:\Program Files\CMake\bin"`

### 3. LLVM / Clang (libclang)

Rust's `bindgen` needs libclang to parse whisper.cpp's headers. Without it:
```
Unable to find libclang: "couldn't find any valid shared libraries matching: ['clang.dll', 'libclang.dll']"
```

1. Download `LLVM-<version>-win64.exe`: https://github.com/llvm/llvm-project/releases
2. **Check "Add LLVM to the system PATH"** during install

**Verify:** `clang --version`

Then set (permanently, via a user environment variable, or per session):
```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

### 4. Rust + Tauri CLI

```powershell
rustup update
rustup component add rustfmt
cargo install tauri-cli
```

### 5a. CUDA Toolkit (NVIDIA GPU builds)

1. Download CUDA Toolkit 11.8+: https://developer.nvidia.com/cuda-downloads
2. Install — it adds itself to `PATH`

**Verify:** `nvcc --version`

> End users don't need any of this — a CUDA build only requires the normal
> NVIDIA driver on the user's machine.

### 5b. Vulkan SDK (alternative to CUDA)

For non-NVIDIA GPUs, or to avoid the CUDA toolkit:
download and install the Vulkan SDK from https://vulkan.lunarg.com

### Build

```powershell
cargo tauri build --features platform-windows-cuda    # NVIDIA GPU
cargo tauri build --features platform-windows-vulkan  # any GPU
cargo tauri build --features platform-windows-cpu     # CPU only
```

Dev build with live reload: `cargo tauri dev --features platform-windows-cuda`

**Output:** `src-tauri\target\release\bundle\` — `.msi` (Windows Installer)
and `.exe` (NSIS setup). Both are self-contained: the MSVC C runtime is
statically linked (see `src-tauri/.cargo/config.toml`), so end users don't
need the Visual C++ Redistributable, and the installer fetches WebView2 if
it's somehow missing (it ships with Windows 10/11).

### Troubleshooting

| Error | Fix |
|---|---|
| `is cmake not installed?` | Install CMake, add to PATH |
| `Unable to find libclang` | Install LLVM, set `LIBCLANG_PATH` |
| CUDA libs `NotPresent` | Install CUDA Toolkit or switch to `platform-windows-vulkan` |
| `rustfmt.exe is not installed` | `rustup component add rustfmt` |

---

## Development builds

`cargo tauri dev` accepts the same flags:

```sh
cargo tauri dev --features platform-macos
cargo tauri dev                              # online-only, fastest compile
```

Unit tests (no audio hardware or network needed):

```sh
cd src-tauri && cargo test --lib
```

---

## Checking the result at runtime

Open the settings panel (click the bubble). The badge next to the version
shows the **active** backend; the **Accel** dropdown lists everything the
build compiled in:

| Badge | Meaning |
|---|---|
| `CoreML` | Apple Neural Engine |
| `Metal` | Apple GPU |
| `CUDA` | NVIDIA GPU |
| `Vulkan` | GPU via Vulkan |
| `CPU` | Whisper on CPU |
| `CPU (no whisper)` | Built without `whisper` — online mode only |

How this detection works, and how to add a new backend, is documented in
[ARCHITECTURE.md](ARCHITECTURE.md#the-platform-system).
