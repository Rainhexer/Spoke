# Spoke
### Cross-platform voice-to-text dictation for the desktop

> **Historical document.** This is the original product specification, kept for
> reference. Some details drifted during implementation (the online engine uses
> the Google Speech-to-Text **v1** REST API with LINEAR16 audio, not v2/Chirp
> with FLAC; only the `google` provider exists; the API key is stored in plain
> text in `spoke.toml`; model sizes and crate versions differ). For how Spoke
> actually works today, see [ARCHITECTURE.md](ARCHITECTURE.md).

---

## Overview

Spoke is a lightweight, cross-platform desktop dictation tool that lets you speak into any text field on your computer — exactly the way macOS's built-in dictation works, but available on Windows, macOS, and Linux, with higher accuracy, offline capability, and full user control.

Press a hotkey, say what you want to type, release — and the transcribed text appears instantly in whatever application your cursor is in. A small, unobtrusive floating bubble sits in the corner of your screen, visible when you need it and quiet when you don't. That's the whole product.

---

## The Problem

Accurate voice dictation on the desktop is fragmented and platform-locked:

- **macOS** has good built-in dictation, but it's closed, non-configurable, and unavailable on other platforms.
- **Windows** has Voice Typing (Win+H), but accuracy is inconsistent and it interrupts workflow with its own modal UI.
- **Linux** has no native solution. Users are left with fragile workarounds.
- **Cloud dictation tools** (Otter, Whisper Web, etc.) require leaving your current application entirely.
- **Third-party tools** are either abandoned, subscription-locked, or built on outdated speech engines.

No existing tool works identically across all three platforms, uses modern speech models, and injects text directly at the OS level without friction.

---

## Solution

Spoke runs as a persistent background process with a minimal floating UI. It listens for a global hotkey, records audio while the key is held (or toggled), transcribes the audio using either a local or cloud speech model, and injects the resulting text directly into the active text field — no clipboard hijacking, no focus stealing, no modal windows.

The core loop is:

```
Hold hotkey → mic opens
Release hotkey → recording stops
              → audio sent to STT engine
              → transcript injected into active field
```

The entire post-release latency for a 10-second dictation is approximately 1–2 seconds in offline mode, and under 1 second with the cloud API.

---

## Core Features

**Global hotkey trigger**
Configurable hotkey (default: `Ctrl+Alt+Space`) activates the mic from anywhere on the system. Supports both push-to-talk (hold) and toggle (tap) modes.

**Text injection**
Transcribed text is typed directly into the focused window using OS-level keyboard simulation — works in any application: browsers, code editors, terminals, chat apps, document editors.

**Dual STT modes**
- **Offline** — Whisper large-v3-turbo runs locally via whisper.cpp. No internet required. Audio never leaves the machine.
- **Online** — Google Cloud Speech-to-Text (Chirp 3 model) via REST API. Same engine that powers Android's Gboard dictation. Higher accuracy on ambiguous speech, faster on low-end hardware.

**Floating bubble UI**
A small, always-on-top bubble displays the current state: idle, recording, or processing. Clicking it expands a compact settings panel. It stays out of the way and requires no dedicated app window.

**Audio file saving** *(optional)*
Each recording can be saved alongside its transcript — useful for reviewing what was said, correcting errors, or building a personal voice dataset over time.

**Multilingual**
Supports Danish, English, and all other Whisper/Chirp-supported languages. Language can be pinned or set to auto-detect per session.

---

## Tech Stack

### Language & Runtime
**Rust** — chosen for low latency, minimal resource usage, and native cross-platform system access. The entire audio capture, STT pipeline, and text injection layer runs in Rust with no garbage collector overhead.

### Application Framework
**Tauri v2** — provides the cross-platform application shell, system tray integration, and the always-on-top transparent bubble window. The UI is **pure HTML + CSS + vanilla JavaScript** (no framework, no bundler, no Node build step), rendered in a lightweight Tauri WebView. The frontend calls into the Rust core through Tauri's global API (`withGlobalTauri`), keeping the shipped bundle to a few small static files.

### Audio Capture
**cpal** — cross-platform audio input crate. Captures raw PCM directly from the microphone into a Rust `Vec<f32>` buffer. The same buffer is used for both file saving and STT input, with no duplication or exclusive-access conflicts.

### Offline Speech-to-Text
**whisper-rs** (bindings to whisper.cpp) running the `large-v3-turbo` model. CUDA acceleration is used automatically when an NVIDIA GPU is available, reducing transcription time to under 500ms for typical dictation lengths. Falls back to CPU for compatibility.

Model options exposed in settings:
| Model | Size | Best for |
|---|---|---|
| tiny | 75 MB | Ultra-low-end hardware |
| base | 145 MB | Fast, decent accuracy |
| small | 465 MB | Balanced |
| medium | 1.5 GB | Higher accuracy, slower |
| large-v3-turbo | 809 MB | Default — best accuracy/speed ratio |
| large-v3 | 2.9 GB | Best accuracy, slowest |

### Online Speech-to-Text
**Google Cloud Speech-to-Text v2 API** (Chirp 3 model) via `reqwest`. Audio is encoded as FLAC, POSTed as a single batch request after recording ends, and the transcript is returned in full. No streaming required.

Free tier: 60 minutes/month per Google account. Beyond that: $0.016/minute.

### Text Injection
**enigo** — cross-platform keyboard simulation crate. Types the transcript string directly into the OS input stream. Supports X11 and Wayland on Linux, Core Graphics on macOS, and SendInput on Windows.

### Global Hotkeys
**global-hotkey** (Tauri ecosystem crate) — registers system-wide keyboard shortcuts that fire even when Spoke's window is not focused.

### Async Runtime
**Tokio** — manages concurrent tasks: audio capture, API calls, file I/O, and UI event communication without blocking the main thread.

---

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    Tauri Shell                       │
│  ┌─────────────────┐   ┌──────────────────────────┐ │
│  │   Bubble UI      │   │     Settings Panel       │ │
│  │  (HTML/CSS/JS)   │   │    (HTML/CSS/JS)         │ │
│  └────────┬─────────┘   └────────────┬─────────────┘ │
│           │ Tauri events             │ config r/w    │
│  ┌────────▼──────────────────────────▼─────────────┐ │
│  │                 Rust Core                        │ │
│  │                                                  │ │
│  │  global-hotkey → [trigger]                       │ │
│  │       ↓                                          │ │
│  │  cpal mic capture → PCM buffer (Vec<f32>)        │ │
│  │       ↓                    ↓                     │ │
│  │  [optional]           [STT mode]                 │ │
│  │  write WAV file    ┌──┴──────────┐               │ │
│  │                    │             │               │ │
│  │              whisper-rs    Google API            │ │
│  │              (offline)     (online)              │ │
│  │                    └──┬──────────┘               │ │
│  │                       ↓                          │ │
│  │                  transcript: String              │ │
│  │                       ↓                          │ │
│  │                    enigo                         │ │
│  │               (text injection)                   │ │
│  └──────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────┘
```

---

## UI Design

### The Bubble
A small circular widget (48px diameter) that sits in a corner of the screen, always on top. Three states:

- **Idle** — dim, minimal. A small microphone icon. Barely noticeable.
- **Recording** — pulses with a soft red glow. A live amplitude ring animates around the bubble, driven by amplitude values pushed from the Rust backend via Tauri events and drawn with the Canvas API.
- **Processing** — a subtle spinner or shimmer. Lasts 1–2 seconds.

Clicking the bubble expands a compact panel (approx. 280px wide) with:
- STT mode toggle (Offline / Online)
- Hotkey display and reconfigure button
- Language selector
- Audio saving toggle + save location
- Model selector (offline mode only)
- Version and API key entry (online mode only)

The UI uses a dark, near-black palette with a single accent color — intentionally minimal. No branding, no onboarding flow, no dashboard. It's a tool, not a product.

### Design Philosophy
Spoke should feel like part of the OS, not like a third-party app. The bubble is the entire product surface. Users should forget it's there until they need it.

---

## Platform Support

| Platform | Audio | Injection | Tray | Hotkeys |
|---|---|---|---|---|
| Linux (X11) | ✓ | ✓ enigo | ✓ | ✓ |
| Linux (Wayland) | ✓ | ✓ enigo | ✓ | ✓ |
| macOS | ✓ | ✓ enigo | ✓ | ✓ |
| Windows 10/11 | ✓ | ✓ enigo | ✓ | ✓ |

Tested environments: Arch Linux (KDE Plasma, Wayland/X11), Ubuntu 22.04+, macOS 13+, Windows 11.

---

## Configuration

All settings are stored in a single `spoke.toml` file in the OS config directory:

```toml
[general]
mode = "offline"          # "offline" | "online"
hotkey = "ctrl+alt+space" # default is "cmd+shift+s" on macOS
trigger = "push_to_talk"  # "push_to_talk" | "toggle"
language = "auto"         # "auto" | "da" | "en" | ...
output_dest = "type"      # "type" | "copy" | "both"

[offline]
model = "large-v3-turbo"
use_gpu = true
accel = "auto"            # "auto" | "metal" | "coreml" | "cuda" | "vulkan" | "none"
                          # only backends compiled into the build take effect

[online]
provider = "google"       # only "google" is implemented
api_key = ""              # stored as plain text in this file

[recording]
save_audio = false
save_path = "~/Documents/Spoke"
format = "wav"            # "wav" | "flac"
save_processed = false    # true: save the mono 16 kHz silence-stripped audio
input_device = ""         # microphone name; empty = system default

[ui]
bubble_position = "bottom-right"
bubble_opacity_idle = 0.4
start_minimized = false   # true: launch into the tray with no bubble on screen
onboarded = false         # set to true once the first-run wizard completes
```

---

## Key Rust Crates

```toml
[dependencies]
tauri          = "2"
cpal           = "0.15"
whisper-rs     = "0.11"
enigo          = "0.2"
global-hotkey  = "0.6"
tokio          = { version = "1", features = ["full"] }
reqwest        = { version = "0.12", features = ["json", "multipart"] }
hound          = "3.5"     # WAV encoding
serde          = { version = "1", features = ["derive"] }
toml           = "0.8"
```

---

## Non-Goals

- **Real-time / streaming transcription.** Spoke prioritises accuracy over watching words appear. Text arrives in full after speaking.
- **Voice commands / AI assistant.** Spoke types what you say. It does not interpret, summarise, or act on content.
- **Mobile.** Desktop only. Android dictation has platform-level audio access restrictions that make this architecture impractical.
- **Speaker diarisation or meeting transcription.** Spoke is a typing tool, not a transcription service.

---

## Project Status

Implemented — see [ARCHITECTURE.md](ARCHITECTURE.md) for the shipped design.
This document is the founding specification, preserved as written.

---

*Spoke — say it once, it's there.*
