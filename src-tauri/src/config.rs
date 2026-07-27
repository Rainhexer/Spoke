//! Configuration: the single `spoke.toml` file in the OS config dir.
//!
//! Mirrors the schema documented in SPOKE.md. Every field has a default so a
//! missing or partial file still produces a usable config.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Offline,
    Online,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    PushToTalk,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    Wav,
    Flac,
}

/// Where the transcript goes: typed via keystroke injection, copied to the
/// clipboard, or both. Deserializes from the legacy `copy_to_clipboard` bool
/// key too (false -> Type, true -> Copy) so old config files keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputDest {
    Type,
    Copy,
    Both,
}

impl OutputDest {
    pub fn types(self) -> bool {
        matches!(self, OutputDest::Type | OutputDest::Both)
    }
    pub fn copies(self) -> bool {
        matches!(self, OutputDest::Copy | OutputDest::Both)
    }
}

impl Serialize for OutputDest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            OutputDest::Type => "type",
            OutputDest::Copy => "copy",
            OutputDest::Both => "both",
        };
        serializer.serialize_str(s)
    }
}

impl<'de> Deserialize<'de> for OutputDest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Legacy(bool),
            Named(String),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Legacy(true) => OutputDest::Copy,
            Repr::Legacy(false) => OutputDest::Type,
            Repr::Named(s) if s == "copy" => OutputDest::Copy,
            Repr::Named(s) if s == "both" => OutputDest::Both,
            Repr::Named(_) => OutputDest::Type,
        })
    }
}

impl Default for OutputDest {
    fn default() -> Self {
        OutputDest::Type
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    pub mode: Mode,
    pub hotkey: String,
    pub trigger: Trigger,
    /// "auto" | BCP-47 code ("en", "da", ...).
    pub language: String,
    /// Where the transcript goes: "type" | "copy" | "both".
    #[serde(alias = "copy_to_clipboard")]
    pub output_dest: OutputDest,
}

/// Platform-appropriate default hotkey: Cmd+Shift+S on macOS, Ctrl+Alt+Space elsewhere.
fn default_hotkey() -> String {
    if cfg!(target_os = "macos") {
        "cmd+shift+s".into()
    } else {
        "ctrl+alt+space".into()
    }
}

/// Offline whenever the binary can actually do it. Builds without the `whisper`
/// feature have no local engine, so online is the only mode that works there.
fn default_mode() -> Mode {
    if cfg!(feature = "whisper") {
        Mode::Offline
    } else {
        Mode::Online
    }
}

impl Default for General {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            hotkey: default_hotkey(),
            trigger: Trigger::PushToTalk,
            language: "auto".into(),
            output_dest: OutputDest::Type,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Offline {
    /// "tiny" | "base" | "small" | "medium" | "large-v3-turbo" | "large-v3"
    pub model: String,
    pub use_gpu: bool,
    /// Acceleration backend: "auto" | "metal" | "coreml" | "cuda" | "vulkan" | "none".
    /// Only backends compiled into this build take effect (see src/platform.rs);
    /// "auto" picks the best one available. The `mac_accel` alias accepts
    /// configs written before the field was generalised beyond macOS.
    #[serde(alias = "mac_accel")]
    pub accel: String,
}

impl Default for Offline {
    fn default() -> Self {
        Self {
            model: "large-v3-turbo".into(),
            use_gpu: true,
            accel: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Online {
    /// Online STT provider. Only "google" is implemented.
    pub provider: String,
    /// Stored as plain text in `spoke.toml` — treat the file accordingly.
    pub api_key: String,
}

impl Default for Online {
    fn default() -> Self {
        Self {
            provider: "google".into(),
            api_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Recording {
    pub save_audio: bool,
    pub save_path: String,
    pub format: AudioFormat,
    /// When true, save the processed (mono, silence-stripped, 16kHz) audio instead
    /// of the raw multi-channel recording.
    pub save_processed: bool,
    /// Name of the input device to use. Empty means default.
    pub input_device: String,
}

impl Default for Recording {
    fn default() -> Self {
        Self {
            save_audio: false,
            save_path: "~/Documents/Spoke".into(),
            format: AudioFormat::Wav,
            save_processed: false,
            input_device: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub bubble_position: String,
    pub bubble_opacity_idle: f32,
    /// Whether the first-run onboarding has been completed. False on a fresh
    /// install (no config file yet), so the onboarding card shows exactly once.
    pub onboarded: bool,
    /// Start hidden in the tray instead of showing the bubble. Chosen during
    /// onboarding and honored on every subsequent launch.
    pub start_minimized: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            bubble_position: "bottom-right".into(),
            bubble_opacity_idle: 0.4,
            onboarded: false,
            start_minimized: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub offline: Offline,
    pub online: Online,
    pub recording: Recording,
    pub ui: Ui,
}

impl Config {
    /// Path to `spoke.toml` in the OS config dir, e.g.
    /// `~/.config/spoke/spoke.toml` on Linux, `~/Library/Application Support`
    /// on macOS, `%APPDATA%` on Windows.
    pub fn path() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join("spoke").join("spoke.toml")
    }

    /// Load from the default path, falling back to defaults if absent or
    /// unreadable. A parse error is returned so the caller can surface it.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Expand a leading `~` in the recording save path to the home dir.
    pub fn resolved_save_path(&self) -> PathBuf {
        expand_tilde(&self.recording.save_path)
    }
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.general.mode, default_mode());
        assert_eq!(c.general.hotkey, default_hotkey());
        assert_eq!(c.general.trigger, Trigger::PushToTalk);
        assert_eq!(c.general.language, "auto");
        assert_eq!(c.offline.model, "large-v3-turbo");
        assert!(c.offline.use_gpu);
        assert_eq!(c.online.provider, "google");
        assert!(!c.recording.save_audio);
        assert_eq!(c.recording.format, AudioFormat::Wav);
        assert_eq!(c.ui.bubble_position, "bottom-right");
        assert!((c.ui.bubble_opacity_idle - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn round_trip_through_toml() {
        let mut c = Config::default();
        c.general.mode = Mode::Online;
        c.online.api_key = "secret".into();
        let dir = std::env::temp_dir().join(format!("spoke-test-{}", std::process::id()));
        let path = dir.join("spoke.toml");
        c.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.general.mode, Mode::Online);
        assert_eq!(loaded.online.api_key, "secret");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = std::env::temp_dir().join("spoke-does-not-exist-xyz.toml");
        let c = Config::load_from(&path).unwrap();
        assert_eq!(c.general.mode, default_mode());
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let toml = "[general]\nmode = \"online\"\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.general.mode, Mode::Online);
        // Untouched fields keep their defaults.
        assert_eq!(c.general.hotkey, default_hotkey());
        assert_eq!(c.offline.model, "large-v3-turbo");
    }

    #[test]
    fn legacy_mac_accel_field_still_parses() {
        let toml = "[offline]\nmac_accel = \"metal\"\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.offline.accel, "metal");
    }

    #[test]
    fn legacy_copy_to_clipboard_bool_still_parses() {
        let toml = "[general]\ncopy_to_clipboard = true\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.general.output_dest, OutputDest::Copy);

        let toml = "[general]\ncopy_to_clipboard = false\n";
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.general.output_dest, OutputDest::Type);
    }

    #[test]
    fn output_dest_both_round_trips() {
        let mut c = Config::default();
        c.general.output_dest = OutputDest::Both;
        let dir = std::env::temp_dir().join(format!("spoke-test-both-{}", std::process::id()));
        let path = dir.join("spoke.toml");
        c.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.general.output_dest, OutputDest::Both);
        assert!(loaded.general.output_dest.types());
        assert!(loaded.general.output_dest.copies());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tilde_expands() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/Documents/Spoke"), home.join("Documents/Spoke"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
