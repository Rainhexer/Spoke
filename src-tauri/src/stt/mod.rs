//! Speech-to-text engines.
//!
//! Two backends behind one enum so the core pipeline doesn't care which is
//! active: `Google` (online, REST) and `Whisper` (offline, feature-gated
//! because it pulls in a heavy native build).

mod google;
#[cfg(feature = "whisper")]
pub mod whisper;

pub use google::GoogleStt;

use crate::config::{Config, Mode};
use anyhow::Result;
use std::sync::Arc;

/// Non-speech annotations whisper.cpp emits as ordinary segment text. They are
/// descriptions of the audio, not dictation, and typing them into the user's
/// editor is always wrong. Matched case-insensitively after the surrounding
/// brackets are removed.
const NON_SPEECH_TAGS: &[&str] = &[
    "blank_audio",
    "blank audio",
    "silence",
    "music",
    "sound",
    "noise",
    "inaudible",
    "applause",
    "laughter",
    "laughs",
];

/// Drop non-speech annotations from a raw transcript.
///
/// Whisper answers a recording it considers non-speech with `[BLANK_AUDIO]`
/// (and friends: `[SOUND]`, `(upbeat music)`, …). Those reach us as normal
/// transcript text, so without this the app types `[BLANK_AUDIO]` whenever the
/// mic is quiet. Only bracketed groups are removed — a known tag in any case,
/// or an all-caps tag such as `[MUSIC PLAYING]` — so dictated words that happen
/// to sit in parentheses survive.
pub fn clean_transcript(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;

    while let Some(open) = rest.find(['[', '(']) {
        let close = match rest.as_bytes()[open] {
            b'[' => ']',
            _ => ')',
        };
        let Some(end) = rest[open..].find(close).map(|i| open + i) else {
            break; // unbalanced — keep the remainder verbatim
        };
        let inner = rest[open + 1..end].trim();
        out.push_str(&rest[..open]);
        if !is_non_speech_tag(inner) {
            out.push_str(&rest[open..=end]);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);

    // Bracket removal can leave doubled spaces where a tag sat mid-sentence.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_non_speech_tag(inner: &str) -> bool {
    if inner.is_empty() {
        return false;
    }
    let lower = inner.to_lowercase();
    if NON_SPEECH_TAGS.iter().any(|t| lower == *t) {
        return true;
    }
    // Uppercase-only tags (`[MUSIC PLAYING]`, `[BLANK_AUDIO]`) are whisper's
    // annotation style; dictation never comes back shouting inside brackets.
    inner.chars().any(|c| c.is_ascii_uppercase())
        && inner
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == ' ' || c == '-')
}

/// A ready-to-use transcription backend.
pub enum SttEngine {
    Google(GoogleStt),
    #[cfg(feature = "whisper")]
    Whisper(whisper::WhisperStt),
}

impl SttEngine {
    /// Build the engine the config selects. Offline mode needs the `whisper`
    /// cargo feature; without it we fail loudly rather than silently degrade.
    pub fn from_config(cfg: &Config) -> Result<Self> {
        match cfg.general.mode {
            Mode::Online => Ok(SttEngine::Google(GoogleStt::new(cfg.online.api_key.clone())?)),
            Mode::Offline => {
                #[cfg(feature = "whisper")]
                {
                    Ok(SttEngine::Whisper(whisper::WhisperStt::from_config(cfg)?))
                }
                #[cfg(not(feature = "whisper"))]
                {
                    let _ = cfg;
                    Err(anyhow::anyhow!(
                        "offline mode requires building with `--features whisper`"
                    ))
                }
            }
        }
    }

    /// Transcribe mono `f32` audio. `sample_rate` is the rate of `mono`;
    /// `language` is "auto" or a BCP-47-ish code from the config.
    ///
    /// Takes `Arc<Self>` and owned buffers because the Whisper arm runs the
    /// blocking whisper.cpp inference on a dedicated blocking thread instead
    /// of stalling an async executor worker.
    pub async fn transcribe(
        self: Arc<Self>,
        mono: Vec<f32>,
        sample_rate: u32,
        language: String,
    ) -> Result<String> {
        match &*self {
            SttEngine::Google(g) => g.transcribe(&mono, sample_rate, &language).await,
            #[cfg(feature = "whisper")]
            SttEngine::Whisper(_) => {
                let this = Arc::clone(&self);
                tokio::task::spawn_blocking(move || match &*this {
                    SttEngine::Whisper(w) => w.transcribe(&mono, sample_rate, &language),
                    _ => unreachable!("variant checked before spawn_blocking"),
                })
                .await
                .map_err(|e| anyhow::anyhow!("whisper transcription task panicked: {e}"))?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::clean_transcript;

    #[test]
    fn drops_blank_audio_marker() {
        assert_eq!(clean_transcript("[BLANK_AUDIO]"), "");
        assert_eq!(clean_transcript("[blank_audio]"), "");
    }

    #[test]
    fn drops_tag_but_keeps_speech() {
        assert_eq!(
            clean_transcript("Hello [MUSIC PLAYING] world"),
            "Hello world"
        );
        assert_eq!(clean_transcript("(silence) start here"), "start here");
    }

    #[test]
    fn keeps_dictated_parentheses() {
        assert_eq!(
            clean_transcript("call foo (the helper) twice"),
            "call foo (the helper) twice"
        );
    }

    #[test]
    fn keeps_unbalanced_brackets_verbatim() {
        assert_eq!(clean_transcript("array[0 is fine"), "array[0 is fine");
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(clean_transcript("just some words"), "just some words");
    }
}
