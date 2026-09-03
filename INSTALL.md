# Installing Spoke

The easiest way to install or update Spoke on Linux and macOS is the install script. It fetches the right prebuilt binary from the [Releases page](https://github.com/Rainhexer/Spoke/releases/latest), no build tools or root required by default.

```sh
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash
```

Re-running the script updates an existing install to the latest release.

On Windows, the script is not supported — download the `.msi`/`.exe` from the [Releases page](https://github.com/Rainhexer/Spoke/releases/latest) instead.

Prefer to compile it yourself? See [BUILD.md](BUILD.md).

---

## What the script installs

| OS | What you get |
|----|--------------|
| macOS (Apple Silicon) | `Spoke.app` in `/Applications` (CoreML + Metal + CPU) |
| Linux (`vulkan`, default) | Prebuilt AppImage → `~/.local/bin/spoke` + desktop entry + icon (Vulkan + CPU, works on any AMD/Intel/NVIDIA GPU, falls back to CPU) |
| Linux (`cpu`) | Prebuilt AppImage → same locations, CPU only |
| Linux (`cuda`) | Native package (`.deb`/`.rpm`) via your package manager, plus NVIDIA driver/runtime guidance. On Arch: prebuilt `.deb` + NVIDIA 12.6 userspace sidecar — still no build. Needs sudo. |

There is intentionally no CUDA AppImage — bundling the CUDA runtime would be enormous — so `--variant cuda` always goes through the native-package path.

Speech models are **not** bundled. After installing, click the bubble → Settings → Download a model.

## Variants

```sh
# Any GPU (default) — AMD/Intel/NVIDIA via Vulkan, falls back to CPU
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash -s -- --variant vulkan

# CPU only — works everywhere, slowest transcription
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash -s -- --variant cpu

# Max NVIDIA performance (Linux only, needs sudo + proprietary driver)
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash -s -- --variant cuda
```

## Options

```
Usage: install.sh [--variant vulkan|cpu|cuda] [--system] [--version TAG] [--binary PATH] [--yes] [--uninstall] [--help]
```

| Flag | What it does |
|------|--------------|
| `--variant vulkan\|cpu\|cuda` | Linux build flavour (default: `vulkan`). `cuda` is Linux-only; macOS always uses its own Apple Silicon build. |
| `--system` | Install to `/usr/local/bin` + `/usr/share` (needs sudo). Default is `~/.local/bin` + `~/.local/share` (no root). `cuda` always uses system packages and needs sudo. |
| `--version TAG` | Install a specific release, e.g. `--version v0.3`. Env equivalent: `SPOKE_VERSION=v0.3`. |
| `--binary PATH` | Install a local binary instead of downloading the release (for testing local builds; Linux only, run as `bash install.sh --binary ...`). |
| `--yes, -y` | Never prompt; assume yes. |
| `--uninstall` | Remove Spoke files installed by this script. |
| `--help` | Show help. |

## Common tasks

```sh
# Update to the latest release (just re-run)
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash

# Pin a version
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash -s -- --version v0.3
# or: SPOKE_VERSION=v0.3 bash install.sh

# System-wide install (needs sudo)
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash -s -- --system

# Uninstall
curl -fsSL https://raw.githubusercontent.com/Rainhexer/Spoke/main/install.sh | bash -s -- --uninstall

# Test a local build (Linux only, cloned repo)
bash install.sh --binary ./src-tauri/target/release/spoke --variant cuda
```

## Notes per platform

- **macOS:** Spoke isn't notarized yet, so the first launch is blocked — open **System Settings → Privacy & Security → Open Anyway**, then grant **Microphone** and **Accessibility** when prompted. Intel Macs aren't shipped prebuilt; build CPU-only from source (see [BUILD.md](BUILD.md)).
- **Linux CUDA:** requires the proprietary NVIDIA driver (`nvidia-smi` must work) plus the CUDA runtime. The script checks and tells you the exact install command for apt/dnf/zypper/pacman before proceeding. No NVIDIA GPU, or prefer zero setup? Use `--variant vulkan` instead.
- **Linux (other distros):** if no supported package manager is found (apt/dnf/pacman/zypper), the script offers to download the `.deb`/`.rpm` for manual install, switch to the Vulkan AppImage, or cancel.
- **Wayland:** if the hotkey doesn't respond, try `GDK_BACKEND=x11 spoke`.

## After installing

1. Launch Spoke (`spoke`, or `/Applications/Spoke.app` on macOS).
2. Click the bubble to open settings.
3. Pick a model and click **Download** — you're set.

If `~/.local/bin` isn't on your `PATH`, the script will tell you — add it with:

```sh
export PATH="$HOME/.local/bin:$PATH"
```
