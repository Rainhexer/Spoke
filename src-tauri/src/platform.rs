//! Build-target platform description.
//!
//! Single source of truth for which OS this binary targets and which
//! acceleration backends were compiled in. Everything downstream — the UI's
//! accel selector, the badge label, the `get_build_info` command — derives
//! from this module, so adding a new backend means adding one cargo feature
//! and one `Backend` entry here.
//!
//! The `compile_error!` guards below reject impossible OS/feature
//! combinations at build time, guaranteeing a binary built for one platform
//! carries no code (or native dependencies) for another.

// ---- Compile-time platform guards ------------------------------------------

#[cfg(all(feature = "metal", not(target_os = "macos")))]
compile_error!("feature `metal` is macOS-only; use `cuda` or `vulkan` on this platform");

#[cfg(all(feature = "coreml", not(target_os = "macos")))]
compile_error!("feature `coreml` is macOS-only; use `cuda` or `vulkan` on this platform");

#[cfg(all(feature = "cuda", target_os = "macos"))]
compile_error!("feature `cuda` is not supported on macOS; use `metal` or `coreml`");

#[cfg(all(feature = "vulkan", target_os = "macos"))]
compile_error!("feature `vulkan` is not supported on macOS; use `metal` or `coreml`");

// ---- Backend catalogue ------------------------------------------------------

/// One acceleration backend compiled into this binary.
///
/// `id` is the value stored in `config.offline.accel`; `label` is the human
/// dropdown text; `badge` is the short form shown on the accel badge.
#[derive(Clone, Copy)]
pub struct Backend {
    pub id: &'static str,
    pub label: &'static str,
    pub badge: &'static str,
}

const CPU: Backend = Backend {
    id: "none",
    label: "CPU only",
    badge: "CPU",
};

/// Human-readable OS this binary was built for.
pub fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Unknown"
    }
}

/// Backends compiled into this binary, best-capability first.
/// Always ends with the CPU fallback, so the list is never empty.
pub fn compiled_backends() -> Vec<Backend> {
    let mut backends = Vec::new();
    if cfg!(feature = "coreml") {
        backends.push(Backend {
            id: "coreml",
            label: "CoreML (Neural Engine)",
            badge: "CoreML",
        });
    }
    if cfg!(feature = "metal") {
        backends.push(Backend {
            id: "metal",
            label: "Metal (GPU)",
            badge: "Metal",
        });
    }
    if cfg!(feature = "cuda") {
        backends.push(Backend {
            id: "cuda",
            label: "CUDA (NVIDIA GPU)",
            badge: "CUDA",
        });
    }
    if cfg!(feature = "vulkan") {
        backends.push(Backend {
            id: "vulkan",
            label: "Vulkan (GPU)",
            badge: "Vulkan",
        });
    }
    backends.push(CPU);
    backends
}

/// True when this is a Wayland desktop session. Spoke forces the X11 GDK
/// backend (XWayland), so the toolkit never reports Wayland itself — the
/// session type has to come from the environment. Wayland refuses global window
/// positioning and always-on-top, which is what makes the floating bubble
/// unreliable there; the UI uses this to warn before the user picks it.
fn is_wayland_session() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    std::env::var("XDG_SESSION_TYPE")
        .map(|v| v.eq_ignore_ascii_case("wayland"))
        .unwrap_or(false)
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Everything the UI needs to describe this build:
/// OS, whether offline transcription is compiled in, the best backend's badge
/// (what "auto" resolves to), and the full backend list for the selector.
pub fn build_info() -> serde_json::Value {
    let backends = compiled_backends();
    serde_json::json!({
        "os": os_name(),
        "wayland": is_wayland_session(),
        "whisper": cfg!(feature = "whisper"),
        "acceleration": backends[0].badge,
        "backends": backends
            .iter()
            .map(|b| serde_json::json!({ "id": b.id, "label": b.label, "badge": b.badge }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backends_never_empty_and_end_with_cpu() {
        let b = compiled_backends();
        assert!(!b.is_empty());
        assert_eq!(b.last().unwrap().id, "none");
    }

    #[test]
    fn build_info_shape() {
        let info = build_info();
        assert!(info["os"].is_string());
        assert!(info["wayland"].is_boolean());
        assert!(info["whisper"].is_boolean());
        assert!(info["acceleration"].is_string());
        assert!(info["backends"].is_array());
    }
}
