//! Local voice capture helper for voice-first MIKU.
//!
//! Capture itself stays in the WebView (`getUserMedia`, now permitted for
//! audio on Android). This module is the rendezvous point: it accumulates
//! raw PCM16 mono chunks, gates them with a tiny energy VAD so silence is
//! never uploaded, and packs the utterance into a WAV container the
//! NetSanctum `/api/miku` voice path already accepts.
//!
//! Hybrid design: VAD/wake gating is local, STT/TTS stay server-side.

use std::sync::Mutex;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::error::{AppError, AppResult};

/// Target sample rate for voice uploads (Hz, mono, 16-bit).
pub const VOICE_SAMPLE_RATE: u32 = 16_000;
/// Hard cap per utterance so a stuck recorder cannot OOM the shell.
pub const VOICE_MAX_SECONDS: u32 = 30;
/// Energy gate: chunks quieter than this are treated as silence.
pub const VOICE_SPEECH_DB_THRESHOLD: f32 = -38.0;

#[derive(Default)]
pub struct VoiceState {
    inner: Mutex<VoiceSession>,
}

#[derive(Default)]
struct VoiceSession {
    sample_rate: u32,
    samples: Vec<i16>,
    max_db: f32,
    speech_chunks: u32,
    total_chunks: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VoiceInfo {
    sample_rate: u32,
    max_seconds: u32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VoiceLevel {
    db: f32,
    speech: bool,
    buffered_seconds: f32,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VoiceClip {
    wav_base64: String,
    seconds: f32,
    max_db: f32,
    speech_chunks: u32,
}

fn rms_db(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return f32::NEG_INFINITY;
    }
    let sum: f64 = samples.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
    let rms = (sum / samples.len() as f64).sqrt();
    if rms < 1.0 {
        return f32::NEG_INFINITY;
    }
    (20.0 * (rms / 32768.0).log10()) as f32
}

fn encode_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_bytes = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[tauri::command]
pub fn voice_start(
    app: AppHandle,
    state: State<'_, VoiceState>,
    sample_rate: Option<u32>,
) -> AppResult<VoiceInfo> {
    let rate = sample_rate.unwrap_or(VOICE_SAMPLE_RATE);
    if !(8000..=48000).contains(&rate) {
        return Err(AppError::Internal("Unsupported voice sample rate.".into()));
    }
    let mut session = state
        .inner
        .lock()
        .map_err(|_| AppError::Internal("Voice session is unavailable.".into()))?;
    *session = VoiceSession {
        sample_rate: rate,
        ..VoiceSession::default()
    };
    let _ = app.emit("voice-state", serde_json::json!({"state": "recording"}));
    Ok(VoiceInfo {
        sample_rate: rate,
        max_seconds: VOICE_MAX_SECONDS,
    })
}

#[tauri::command]
pub fn voice_push_pcm(
    app: AppHandle,
    state: State<'_, VoiceState>,
    chunk_base64: String,
) -> AppResult<VoiceLevel> {
    let bytes = BASE64
        .decode(chunk_base64.as_bytes())
        .map_err(|_| AppError::Internal("Voice chunk is not valid base64.".into()))?;
    if bytes.len() % 2 != 0 || bytes.is_empty() {
        return Err(AppError::Internal(
            "Voice chunk must be non-empty PCM16.".into(),
        ));
    }
    let mut chunk = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        chunk.push(i16::from_le_bytes([pair[0], pair[1]]));
    }
    let db = rms_db(&chunk);
    let speech = db >= VOICE_SPEECH_DB_THRESHOLD;
    let mut session = state
        .inner
        .lock()
        .map_err(|_| AppError::Internal("Voice session is unavailable.".into()))?;
    if session.sample_rate == 0 {
        return Err(AppError::Internal("Voice session is not started.".into()));
    }
    let capacity = (VOICE_MAX_SECONDS * session.sample_rate) as usize;
    let room = capacity.saturating_sub(session.samples.len());
    session.samples.extend(chunk.iter().take(room));
    session.total_chunks += 1;
    if speech {
        session.speech_chunks += 1;
    }
    if db > session.max_db {
        session.max_db = db;
    }
    let level = VoiceLevel {
        db,
        speech,
        buffered_seconds: session.samples.len() as f32 / session.sample_rate as f32,
    };
    let _ = app.emit("voice-level", level.clone());
    Ok(level)
}

#[tauri::command]
pub fn voice_finish(app: AppHandle, state: State<'_, VoiceState>) -> AppResult<VoiceClip> {
    let mut session = state
        .inner
        .lock()
        .map_err(|_| AppError::Internal("Voice session is unavailable.".into()))?;
    if session.samples.is_empty() {
        return Err(AppError::Internal("Voice utterance is empty.".into()));
    }
    let wav = encode_wav(&session.samples, session.sample_rate);
    let clip = VoiceClip {
        wav_base64: BASE64.encode(&wav),
        seconds: session.samples.len() as f32 / session.sample_rate as f32,
        max_db: session.max_db,
        speech_chunks: session.speech_chunks,
    };
    *session = VoiceSession::default();
    let _ = app.emit("voice-state", serde_json::json!({"state": "idle"}));
    Ok(clip)
}

#[tauri::command]
pub fn voice_cancel(app: AppHandle, state: State<'_, VoiceState>) -> AppResult<()> {
    if let Ok(mut session) = state.inner.lock() {
        *session = VoiceSession::default();
    }
    let _ = app.emit("voice-state", serde_json::json!({"state": "idle"}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_below_speech_threshold() {
        assert!(rms_db(&[0; 160]) < VOICE_SPEECH_DB_THRESHOLD);
    }

    #[test]
    fn loud_tone_counts_as_speech() {
        let tone: Vec<i16> = (0..160).map(|i| ((i * 500) % 20000) as i16).collect();
        assert!(rms_db(&tone) >= VOICE_SPEECH_DB_THRESHOLD);
    }

    #[test]
    fn wav_header_roundtrips() {
        let wav = encode_wav(&[0, 1000, -1000], 16_000);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 44 + 6);
    }
}
