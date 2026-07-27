//! Microphone capture via cpal.
//!
//! cpal's `Stream` is not `Send`, so the stream lives on a dedicated OS thread
//! that owns it for its whole lifetime. The rest of the app talks to that
//! thread over channels: send `Cmd::Start` to open the mic, `Cmd::Stop` to
//! close it and receive the captured PCM. While recording, per-buffer RMS
//! amplitude is pushed to `amp_tx` so the UI can animate the bubble ring.

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};

/// List all available input device names.
///
/// ALSA device enumeration can block indefinitely on systems with misconfigured
/// audio (e.g. JACK/OSS backends that refuse to open), so this runs on a
/// dedicated thread with a hard timeout and returns whatever we got if the
/// timeout fires.
pub fn list_input_devices() -> Vec<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let host = cpal::default_host();
        let mut names: Vec<String> = Vec::new();
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if let Ok(n) = d.name() {
                    if cfg!(target_os = "linux") && !is_useful_linux_device(&n) {
                        continue;
                    }
                    names.push(n);
                }
            }
        }
        let _ = tx.send(names);
    });
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap_or_default()
}

/// On Linux/ALSA, filter out internal pseudo-devices that are never useful
/// to end users (hw:, plughw:, dmix:, dsnoop:, etc.).  These devices cause
/// screenfuls of ALSA error spam and can't be opened on PipeWire systems.
#[cfg(target_os = "linux")]
fn is_useful_linux_device(name: &str) -> bool {
    const BAD_PREFIXES: &[&str] = &[
        "hw:", "plughw:", "dsnoop:", "dmix:", "plug:", "sysdefault:", "iec958:",
        "upmix:", "vdownmix:", "surround",
    ];
    const BAD_EXACT: &[&str] = &["null", "pipewire-jack"];
    !BAD_PREFIXES.iter().any(|p| name.starts_with(p)) && !BAD_EXACT.contains(&name)
}

#[cfg(not(target_os = "linux"))]
fn is_useful_linux_device(_name: &str) -> bool {
    true
}

/// Raw captured audio: interleaved `f32` samples at the device's native rate.
#[derive(Debug, Clone)]
pub struct Recording {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

enum Cmd {
    /// Start recording with an optional device name (empty = default).
    Start(String),
    /// Stop and send the captured recording back.
    Stop(SyncSender<Result<Recording>>),
}

/// Handle to the audio capture thread.
pub struct AudioEngine {
    tx: Sender<Cmd>,
}

impl AudioEngine {
    /// Spawn the capture thread. `amp_tx` receives an RMS amplitude in `0.0..=1.0`
    /// for every captured buffer while recording is active.
    pub fn spawn(amp_tx: Sender<f32>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        std::thread::Builder::new()
            .name("spoke-audio".into())
            .spawn(move || audio_thread(rx, amp_tx))
            .expect("spawn audio thread");
        Self { tx }
    }

    /// Request the audio thread to start recording.  `device` is an optional
    /// device name; pass an empty string to use the system default.
    pub fn start(&self, device: &str) -> Result<()> {
        self.tx
            .send(Cmd::Start(device.to_string()))
            .map_err(|_| anyhow!("audio thread gone"))
    }

    /// Stop recording and return the captured PCM (blocks until the thread replies).
    pub fn stop(&self) -> Result<Recording> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(Cmd::Stop(reply_tx))
            .map_err(|_| anyhow!("audio thread gone"))?;
        reply_rx.recv().map_err(|_| anyhow!("audio thread dropped reply"))?
    }
}

fn audio_thread(rx: Receiver<Cmd>, amp_tx: Sender<f32>) {
    let mut active: Option<(cpal::Stream, Arc<Mutex<Vec<f32>>>, u32, u16)> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Start(device) => {
                if active.is_some() {
                    continue;
                }
                match open_stream(&device, amp_tx.clone()) {
                    Ok(state) => {
                        if let Err(e) = state.0.play() {
                            eprintln!("[audio] failed to start stream: {e}");
                        } else {
                            active = Some(state);
                        }
                    }
                    Err(e) => eprintln!("[audio] failed to open input: {e}"),
                }
            }
            Cmd::Stop(reply) => {
                let result = match active.take() {
                    Some((stream, buf, sample_rate, channels)) => {
                        drop(stream);
                        let samples = std::mem::take(&mut *buf.lock().unwrap());
                        Ok(Recording {
                            samples,
                            sample_rate,
                            channels,
                        })
                    }
                    None => Err(anyhow!("not recording")),
                };
                let _ = reply.send(result);
            }
        }
    }
}

fn open_stream(
    device_name: &str,
    amp_tx: Sender<f32>,
) -> Result<(cpal::Stream, Arc<Mutex<Vec<f32>>>, u32, u16)> {
    let host = cpal::default_host();
    let mut tried_devices: Vec<String> = Vec::new();

    // Collect candidate devices: if a specific device was requested, try it
    // first; then the default; then all others as fallback.
    let mut candidates: Vec<cpal::Device> = Vec::new();
    if !device_name.is_empty() {
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if d.name().ok().as_deref() == Some(device_name) {
                    candidates.push(d);
                    break;
                }
            }
        }
    }
    if let Some(d) = host.default_input_device() {
        if !candidates.iter().any(|c| c.name().ok() == d.name().ok()) {
            candidates.push(d);
        }
    }
    // On Linux, enumerating every ALSA pseudo-device floods stderr with JACK/OSS/dmix
    // errors and never succeeds on PipeWire systems.  Only try the known-good names.
    #[cfg(target_os = "linux")]
    {
        let linux_prio = ["pulse", "pipewire", "default"];
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                let n = d.name().unwrap_or_default();
                if linux_prio.contains(&n.as_str())
                    && !candidates.iter().any(|c| c.name().ok().as_deref() == Some(&n))
                {
                    candidates.push(d);
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if !candidates.iter().any(|c| c.name().ok() == d.name().ok()) {
                candidates.push(d);
            }
        }
    }

    for device in &candidates {
        let name = device.name().unwrap_or_else(|_| "(unknown)".into());
        tried_devices.push(name.clone());
        let config = match device.default_input_config() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[audio] {name}: skipping (bad config: {e})");
                continue;
            }
        };
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));

        let err_fn = |e| eprintln!("[audio] stream error: {e}");

        let buf = buffer.clone();
        let tx = amp_tx.clone();
        let push_fn = move |data: &[f32]| {
            if !data.is_empty() {
                let sum_sq: f32 = data.iter().map(|s| s * s).sum();
                let rms = (sum_sq / data.len() as f32).sqrt();
                let _ = tx.send(rms.clamp(0.0, 1.0));
            }
            buf.lock().unwrap().extend_from_slice(data);
        };

        let result = match config.sample_format() {
            SampleFormat::F32 => {
                let push = push_fn;
                device.build_input_stream(
                    &config.into(),
                    move |d: &[f32], _| push(d),
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let push = push_fn;
                device.build_input_stream(
                    &config.into(),
                    move |d: &[i16], _| {
                        let f: Vec<f32> =
                            d.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                        push(&f)
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let push = push_fn;
                device.build_input_stream(
                    &config.into(),
                    move |d: &[u16], _| {
                        let f: Vec<f32> = d
                            .iter()
                            .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                            .collect();
                        push(&f)
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                eprintln!("[audio] {name}: unsupported format {other:?}");
                continue;
            }
        };

        match result {
            Ok(stream) => return Ok((stream, buffer, sample_rate, channels)),
            Err(e) => {
                eprintln!("[audio] {name}: failed to open ({e})");
            }
        }
    }

    Err(anyhow!(
        "no usable input device (tried: {})",
        tried_devices.join(", ")
    ))
}

/// Downmix interleaved audio to a single mono channel by averaging.
pub fn to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return samples.to_vec();
    }
    let ch = channels as usize;
    samples
        .chunks(ch)
        .map(|frame| frame.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// Resample mono audio to `target_rate` with linear interpolation.
/// Whisper requires 16 kHz mono. (Only used by the `whisper` feature + tests.)
#[allow(dead_code)]
pub fn resample_mono(mono: &[f32], from_rate: u32, target_rate: u32) -> Vec<f32> {
    if from_rate == target_rate || mono.is_empty() {
        return mono.to_vec();
    }
    let ratio = target_rate as f64 / from_rate as f64;
    let out_len = ((mono.len() as f64) * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 / ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = mono[idx.min(mono.len() - 1)];
        let b = mono[(idx + 1).min(mono.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

/// Scale quiet recordings up to a usable level before transcription.
///
/// Many built-in mics (and any mic with the OS input slider turned down) deliver
/// speech peaking around -30 dBFS. Whisper's no-speech detector treats audio
/// that quiet as silence and returns `[BLANK_AUDIO]` for a perfectly good
/// recording, so the level is normalized here instead of trusting the device.
///
/// The reference level is the 99th-percentile magnitude rather than the raw
/// peak, so one click or pop can't suppress the gain the rest of the take needs.
/// Gain is only applied when the recording is loud enough to plausibly contain
/// speech (`MIN_REFERENCE`); below that it is room tone, and amplifying it just
/// feeds the model noise to hallucinate on. Gain is never below 1.0 — already
/// loud audio is returned untouched.
pub fn normalize_gain(mono: &[f32]) -> Vec<f32> {
    /// Level the 99th percentile is raised to. Leaves headroom so the few
    /// samples above the percentile don't clip hard.
    const TARGET: f32 = 0.5;
    /// Below this the recording is noise, not quiet speech — leave it alone.
    const MIN_REFERENCE: f32 = 0.003;
    /// Ceiling on amplification, so a near-silent take isn't blown up into
    /// full-scale hiss.
    const MAX_GAIN: f32 = 15.0;

    if mono.is_empty() {
        return Vec::new();
    }

    let mut mags: Vec<f32> = mono.iter().map(|s| s.abs()).collect();
    let idx = ((mags.len() as f32 * 0.99) as usize).min(mags.len() - 1);
    mags.select_nth_unstable_by(idx, |a, b| a.total_cmp(b));
    let reference = mags[idx];

    if reference < MIN_REFERENCE {
        return mono.to_vec();
    }
    let gain = (TARGET / reference).clamp(1.0, MAX_GAIN);
    if gain <= 1.0 {
        return mono.to_vec();
    }
    mono.iter().map(|s| (s * gain).clamp(-1.0, 1.0)).collect()
}

/// Strip leading silence from mono audio to reduce STT hallucinations.
///
/// Algorithm:
/// 1. Classify 20 ms frames as speech (RMS > threshold) or silence.
/// 2. Dilate each speech frame by 50 ms pre-roll / 500 ms post-roll to avoid
///    clipping plosives and trailing phonemes.
/// 3. Find the first speech sample and trim everything before it.
/// 4. Within the remaining audio, compress any silence run longer than 300 ms
///    down to 300 ms — keeps natural rhythm without feeding long dead air to
///    the model.
///
/// The end of the audio is NOT trimmed, so trailing speech (even if quiet
/// enough to fall below the RMS threshold) is never cut off. Any trailing
/// silence is compressed to at most 300 ms.
///
/// Returns an empty Vec when no speech is detected.
pub fn strip_internal_silence(mono: &[f32], sample_rate: u32) -> Vec<f32> {
    const THRESHOLD: f32 = 0.01;
    const FRAME_MS: usize = 20;
    const MAX_SILENCE_MS: usize = 300;
    const PRE_ROLL_MS: usize = 50;
    const POST_ROLL_MS: usize = 500;

    let frame_len = (sample_rate as usize * FRAME_MS) / 1000;
    let max_silence_len = (sample_rate as usize * MAX_SILENCE_MS) / 1000;
    let pre_roll = (sample_rate as usize * PRE_ROLL_MS) / 1000;
    let post_roll = (sample_rate as usize * POST_ROLL_MS) / 1000;

    if mono.len() < frame_len * 2 {
        return mono.to_vec();
    }

    let rms = |chunk: &[f32]| -> f32 {
        let sq: f32 = chunk.iter().map(|s| s * s).sum();
        (sq / chunk.len() as f32).sqrt()
    };

    // Per-sample speech mask, dilated by pre/post roll.
    let mut speech = vec![false; mono.len()];
    let n_frames = mono.len() / frame_len;
    for i in 0..n_frames {
        let s = i * frame_len;
        let e = (s + frame_len).min(mono.len());
        if rms(&mono[s..e]) > THRESHOLD {
            let lo = s.saturating_sub(pre_roll);
            let hi = (e + post_roll).min(mono.len());
            speech[lo..hi].iter_mut().for_each(|x| *x = true);
        }
    }

    let first = match speech.iter().position(|&x| x) {
        Some(i) => i,
        None => return Vec::new(),
    };

    // Copy from first speech to the end; compress silence runs.
    let mut out = Vec::with_capacity(mono.len() - first);
    let mut silence_run = 0usize;
    for i in first..mono.len() {
        if speech[i] {
            silence_run = 0;
            out.push(mono[i]);
        } else {
            if silence_run < max_silence_len {
                out.push(mono[i]);
            }
            silence_run += 1;
        }
    }

    out
}

/// Convert mono `f32` (`-1.0..=1.0`) to little-endian 16-bit PCM bytes.
pub fn mono_to_pcm16_le(mono: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(mono.len() * 2);
    for &s in mono {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

/// Write a recording to a 16-bit PCM WAV file.
pub fn save_wav(path: &std::path::Path, rec: &Recording) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let spec = hound::WavSpec {
        channels: rec.channels,
        sample_rate: rec.sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in &rec.samples {
        writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Write mono PCM samples to a 16-bit mono WAV file at the given sample rate.
pub fn save_wav_mono(path: &std::path::Path, samples: &[f32], sample_rate: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_averages_stereo() {
        let stereo = [1.0, 0.0, 0.5, 0.5]; // 2 frames, 2 channels
        assert_eq!(to_mono(&stereo, 2), vec![0.5, 0.5]);
    }

    #[test]
    fn mono_passthrough_when_single_channel() {
        let s = [0.1, 0.2, 0.3];
        assert_eq!(to_mono(&s, 1), s.to_vec());
    }

    #[test]
    fn resample_noop_when_rates_match() {
        let s = vec![0.1, 0.2, 0.3];
        assert_eq!(resample_mono(&s, 16000, 16000), s);
    }

    #[test]
    fn resample_halves_length_when_downsampling_2x() {
        let s: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = resample_mono(&s, 32000, 16000);
        assert_eq!(out.len(), 50);
    }

    #[test]
    fn pcm16_encodes_full_scale() {
        let bytes = mono_to_pcm16_le(&[1.0, -1.0, 0.0]);
        assert_eq!(bytes.len(), 6);
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), i16::MAX);
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -i16::MAX);
        assert_eq!(i16::from_le_bytes([bytes[4], bytes[5]]), 0);
    }

    #[test]
    fn normalize_lifts_quiet_speech_above_silence_threshold() {
        // A take peaking at ~0.04 — the level a low-gain built-in mic produces,
        // which whisper otherwise reports as [BLANK_AUDIO].
        let quiet: Vec<f32> = (0..16_000)
            .map(|i| (i as f32 / 20.0).sin() * 0.04)
            .collect();
        let out = normalize_gain(&quiet);
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.4, "peak {peak} not lifted");
        // And the lifted audio now survives silence stripping.
        assert!(!strip_internal_silence(&out, 16_000).is_empty());
    }

    #[test]
    fn normalize_leaves_loud_audio_untouched() {
        let loud: Vec<f32> = (0..16_000).map(|i| (i as f32 / 20.0).sin() * 0.9).collect();
        assert_eq!(normalize_gain(&loud), loud);
    }

    #[test]
    fn normalize_does_not_amplify_room_tone() {
        let noise: Vec<f32> = (0..16_000)
            .map(|i| (i as f32 / 7.0).sin() * 0.001)
            .collect();
        assert_eq!(normalize_gain(&noise), noise);
    }

    #[test]
    fn normalize_ignores_isolated_click() {
        // One full-scale sample must not stop the rest of the take being lifted.
        let mut audio: Vec<f32> = (0..16_000).map(|i| (i as f32 / 20.0).sin() * 0.04).collect();
        audio[0] = 1.0;
        let out = normalize_gain(&audio);
        let lifted = out[100..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(lifted > 0.4, "click suppressed gain (peak {lifted})");
    }

    #[test]
    fn strip_silence_removes_pure_silence() {
        // 1 second of silence at 16 kHz → no speech detected → empty output.
        let silence = vec![0.0f32; 16_000];
        let out = strip_internal_silence(&silence, 16_000);
        assert!(out.is_empty());
    }

    #[test]
    fn strip_silence_preserves_speech() {
        // 0.5 s silence, 0.5 s loud speech, 0.5 s silence.
        let rate = 16_000u32;
        let half = rate as usize / 2;
        let mut audio = vec![0.0f32; half];
        audio.extend(vec![0.5f32; half]); // speech above threshold
        audio.extend(vec![0.0f32; half]);
        let out = strip_internal_silence(&audio, rate);
        // Output must contain the speech samples (plus padding); all zeros trimmed.
        assert!(!out.is_empty());
        assert!(out.len() < audio.len()); // silence stripped
        assert!(out.iter().any(|&s| s > 0.1));
    }

    #[test]
    fn strip_silence_compresses_long_internal_pause() {
        // 0.2 s speech, 1 s silence, 0.2 s speech — the pause should be compressed.
        let rate = 16_000u32;
        let speech_len = (rate as usize * 200) / 1000;
        let pause_len = rate as usize; // 1 second
        let mut audio = vec![0.5f32; speech_len];
        audio.extend(vec![0.0f32; pause_len]);
        audio.extend(vec![0.5f32; speech_len]);
        let out = strip_internal_silence(&audio, rate);
        // 1 s pause compressed to ≤ 300 ms; total shorter than input.
        assert!(out.len() < audio.len());
        // Still contains both speech bursts.
        assert!(out.iter().any(|&s| s > 0.1));
    }

    #[test]
    fn strip_silence_short_audio_passthrough() {
        // Audio shorter than 2 frames → returned unchanged.
        let tiny = vec![0.5f32; 10];
        let out = strip_internal_silence(&tiny, 16_000);
        assert_eq!(out, tiny);
    }
}
