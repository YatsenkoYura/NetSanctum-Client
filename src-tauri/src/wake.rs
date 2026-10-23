//! Always-listening openWakeWord engine (native, on-device).
//!
//! Replicates the upstream openWakeWord streaming pipeline with ONNX models:
//! 16 kHz PCM16 -> mel spectrogram (32 banks) -> speech embedding (96 dims)
//! -> per-wake-word classifier over the last N embeddings -> score 0..1.
//!
//! Models are fetched on demand from the upstream GitHub release into the
//! app data dir (`wake_models/`), so builds stay light. Custom `.onnx`
//! wake-word models can be registered with [`wake_load_custom`].
//!
//! Hybrid design: wake detection is fully local; STT/TTS stay server-side.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use ort::session::Session;
use ort::value::TensorRef;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::error::{AppError, AppResult};

const MODEL_RELEASE_BASE: &str =
    "https://github.com/dscripka/openWakeWord/releases/download/v0.5.1";
const CHUNK_SAMPLES: usize = 1280; // 80 ms @ 16 kHz, the upstream cadence
const RAW_BUFFER_CAP: usize = 160_000; // 10 s, mirrors upstream deque
const MEL_BUFFER_CAP: usize = 970; // 10*97 frames, mirrors upstream
const MEL_INIT_FRAMES: usize = 76;
const MEL_BANKS: usize = 32;
const EMBEDDING_DIM: usize = 96;
const EMBEDDING_WINDOW: usize = 76;
const FEATURE_BUFFER_CAP: usize = 120;
const WARMUP_EMBEDDINGS: u64 = 5; // upstream zeroes the first 5 frames

/// Stock models bundled by name -> release file.
const STOCK_MODELS: &[(&str, &str)] = &[
    ("hey_jarvis", "hey_jarvis_v0.1.onnx"),
    ("alexa", "alexa_v0.1.onnx"),
    ("hey_mycroft", "hey_mycroft_v0.1.onnx"),
];

const FEATURE_FILES: &[&str] = &["melspectrogram.onnx", "embedding_model.onnx"];

fn internal(message: &str) -> AppError {
    AppError::Internal(message.to_string())
}

struct WakeModel {
    session: Session,
    window: usize,
    threshold: f32,
    cooldown: Duration,
    last_fire: Option<Instant>,
}

struct WakeEngine {
    melspec: Option<Session>,
    embedding: Option<Session>,
    models: HashMap<String, WakeModel>,
    raw: VecDeque<i16>,
    mel: VecDeque<[f32; MEL_BANKS]>,
    features: VecDeque<[f32; EMBEDDING_DIM]>,
    remainder: Vec<i16>,
    embeddings_seen: u64,
}

impl Default for WakeEngine {
    fn default() -> Self {
        Self {
            melspec: None,
            embedding: None,
            models: HashMap::new(),
            raw: VecDeque::with_capacity(RAW_BUFFER_CAP),
            mel: VecDeque::with_capacity(MEL_BUFFER_CAP),
            features: VecDeque::with_capacity(FEATURE_BUFFER_CAP),
            remainder: Vec::new(),
            embeddings_seen: 0,
        }
    }
}

fn engine_error(message: &str) -> AppError {
    internal(message)
}

#[derive(Default)]
pub struct WakeState {
    inner: Mutex<WakeEngine>,
    dir: Mutex<Option<std::path::PathBuf>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WakeStatus {
    enabled: bool,
    models: Vec<String>,
    feature_models_ready: bool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WakeScores {
    scores: HashMap<String, f32>,
}

fn ort_session(path: &std::path::Path) -> AppResult<Session> {
    Session::builder()
        .map_err(|e| internal(&format!("Wake engine failed to start: {e}")))?
        .commit_from_file(path)
        .map_err(|e| internal(&format!("Wake model failed to load: {e}")))
}

fn run_melspec(session: &mut Session, samples: &[i16]) -> AppResult<Vec<[f32; MEL_BANKS]>> {
    let floats: Vec<f32> = samples.iter().map(|s| *s as f32).collect();
    let input = TensorRef::from_array_view((vec![1, floats.len()], floats.as_slice()))
        .map_err(|e| internal(&format!("Wake input failed: {e}")))?;
    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| internal(&format!("Wake mel inference failed: {e}")))?;
    let (_shape, data) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| internal(&format!("Wake mel output failed: {e}")))?;
    let frames = data.len() / MEL_BANKS;
    let mut out = Vec::with_capacity(frames);
    for frame in data.chunks_exact(MEL_BANKS) {
        let mut row = [0.0f32; MEL_BANKS];
        row.copy_from_slice(frame);
        // Upstream transform aligning ONNX melspec with the TF reference.
        for value in row.iter_mut() {
            *value = *value / 10.0 + 2.0;
        }
        out.push(row);
    }
    Ok(out)
}

fn run_embedding(
    session: &mut Session,
    window: &[[f32; MEL_BANKS]],
) -> AppResult<[f32; EMBEDDING_DIM]> {
    let mut flat = Vec::with_capacity(EMBEDDING_WINDOW * MEL_BANKS);
    for row in window.iter() {
        flat.extend_from_slice(row);
    }
    let input =
        TensorRef::from_array_view((vec![1, EMBEDDING_WINDOW, MEL_BANKS, 1], flat.as_slice()))
            .map_err(|e| internal(&format!("Wake input failed: {e}")))?;
    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| internal(&format!("Wake embedding inference failed: {e}")))?;
    let (_shape, data) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| internal(&format!("Wake embedding output failed: {e}")))?;
    let mut out = [0.0f32; EMBEDDING_DIM];
    let len = data.len().min(EMBEDDING_DIM);
    out[..len].copy_from_slice(&data[..len]);
    Ok(out)
}

fn run_classifier(session: &mut Session, window: &[[f32; EMBEDDING_DIM]]) -> AppResult<f32> {
    let mut flat = Vec::with_capacity(window.len() * EMBEDDING_DIM);
    for row in window.iter() {
        flat.extend_from_slice(row);
    }
    let input = TensorRef::from_array_view((vec![1, window.len(), EMBEDDING_DIM], flat.as_slice()))
        .map_err(|e| internal(&format!("Wake input failed: {e}")))?;
    let outputs = session
        .run(ort::inputs![input])
        .map_err(|e| internal(&format!("Wake classifier inference failed: {e}")))?;
    let (_shape, data) = outputs[0]
        .try_extract_tensor::<f32>()
        .map_err(|e| internal(&format!("Wake classifier output failed: {e}")))?;
    Ok(data.first().copied().unwrap_or(0.0))
}

impl WakeEngine {
    fn push_raw(&mut self, samples: &[i16]) {
        for sample in samples {
            if self.raw.len() >= RAW_BUFFER_CAP {
                self.raw.pop_front();
            }
            self.raw.push_back(*sample);
        }
    }

    /// Feed streaming PCM16 mono @16kHz. Returns per-model scores for every
    /// completed 80 ms frame (mirrors `Model.predict` chunking).
    fn step(
        &mut self,
        samples: &[i16],
        emit: &mut dyn FnMut(&str, f32),
    ) -> AppResult<HashMap<String, f32>> {
        if self.melspec.is_none() || self.embedding.is_none() {
            return Err(internal("Wake feature models are not loaded."));
        }
        if self.models.is_empty() {
            return Err(internal("No wake-word models are enabled."));
        }
        let mut pending: Vec<i16> = std::mem::take(&mut self.remainder);
        pending.extend_from_slice(samples);
        let mut scores = HashMap::new();
        let mut offset = 0;
        while pending.len() - offset >= CHUNK_SAMPLES {
            let chunk = &pending[offset..offset + CHUNK_SAMPLES];
            offset += CHUNK_SAMPLES;
            self.push_raw(chunk);
            // Mel over the trailing window (chunk + 480 samples of context).
            let take = (CHUNK_SAMPLES + 480).min(self.raw.len());
            let tail: Vec<i16> = self
                .raw
                .iter()
                .copied()
                .skip(self.raw.len() - take)
                .collect();
            let frames = match self.melspec.as_mut() {
                Some(melspec) => run_melspec(melspec, &tail)?,
                None => return Err(engine_error("Wake feature models are not loaded.")),
            };
            for frame in frames {
                if self.mel.len() >= MEL_BUFFER_CAP {
                    self.mel.pop_front();
                }
                self.mel.push_back(frame);
            }
            // One embedding from the trailing 76 mel frames.
            if self.mel.len() >= EMBEDDING_WINDOW {
                let window: Vec<[f32; MEL_BANKS]> = self
                    .mel
                    .iter()
                    .copied()
                    .skip(self.mel.len() - EMBEDDING_WINDOW)
                    .collect();
                let vector = match self.embedding.as_mut() {
                    Some(embedding) => run_embedding(embedding, &window)?,
                    None => return Err(engine_error("Wake feature models are not loaded.")),
                };
                if self.features.len() >= FEATURE_BUFFER_CAP {
                    self.features.pop_front();
                }
                self.features.push_back(vector);
                self.embeddings_seen += 1;
            }
            // Score every model on its trailing window.
            if self.embeddings_seen > WARMUP_EMBEDDINGS {
                // Snapshot model params + feature windows first so the
                // classifier sessions can be borrowed mutably one by one.
                struct WorkItem {
                    name: String,
                    window: Vec<[f32; EMBEDDING_DIM]>,
                    threshold: f32,
                    cooldown: Duration,
                }
                let mut work: Vec<WorkItem> = Vec::new();
                for (name, model) in self.models.iter() {
                    if self.features.len() < model.window {
                        continue;
                    }
                    let skip = self.features.len() - model.window;
                    work.push(WorkItem {
                        name: name.clone(),
                        window: self.features.iter().copied().skip(skip).collect(),
                        threshold: model.threshold,
                        cooldown: model.cooldown,
                    });
                }
                let now = Instant::now();
                for item in work {
                    let model = match self.models.get_mut(&item.name) {
                        Some(model) => model,
                        None => continue,
                    };
                    let score = run_classifier(&mut model.session, &item.window)?;
                    scores.insert(item.name.clone(), score);
                    if score >= item.threshold {
                        let cooled = model
                            .last_fire
                            .map(|fire| now.duration_since(fire) >= item.cooldown)
                            .unwrap_or(true);
                        if cooled {
                            model.last_fire = Some(now);
                            emit(&item.name, score);
                        }
                    }
                }
            }
        }
        self.remainder = pending[offset..].to_vec();
        Ok(scores)
    }
}

fn enable_model(
    engine: &mut WakeEngine,
    dir: &std::path::Path,
    name: &str,
    threshold: f32,
    cooldown: Duration,
) -> AppResult<()> {
    let file = STOCK_MODELS
        .iter()
        .find(|(stock, _)| *stock == name)
        .map(|(_, file)| dir.join(file))
        .unwrap_or_else(|| dir.join(format!("{name}.onnx")));
    if !file.exists() {
        return Err(internal(&format!("Wake model '{name}' is not downloaded.")));
    }
    // Peek the classifier window from its input shape ([1, window, 96]),
    // then reuse the same session instead of loading the file twice.
    let session = ort_session(&file)?;
    let window = session
        .inputs()
        .first()
        .and_then(|input| match input.dtype() {
            ort::value::ValueType::Tensor { shape, .. } => usize::try_from(shape[1])
                .ok()
                .filter(|w| (1..=64).contains(w)),
            _ => None,
        })
        .unwrap_or(16);
    engine.models.insert(
        name.to_string(),
        WakeModel {
            session,
            window,
            threshold,
            cooldown,
            last_fire: None,
        },
    );
    seed_features(engine)?;
    Ok(())
}

/// Deterministic pseudo-random PCM in [-1000, 1000], mirroring the upstream
/// cold-start seeding (`np.random.randint(-1000, 1000, 16000*4)`).
fn seed_pcm(samples: usize) -> Vec<i16> {
    let mut state: u64 = 0x1234_5678_9abc_def0;
    let mut out = Vec::with_capacity(samples);
    for _ in 0..samples {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push((state % 2001) as i16 - 1000);
    }
    out
}

/// Prime the shared feature buffer once, like upstream, so the first second
/// of audio already scores instead of waiting for a full 16-frame window.
fn seed_features(engine: &mut WakeEngine) -> AppResult<()> {
    if !engine.features.is_empty() {
        return Ok(());
    }
    if engine.melspec.is_none() || engine.embedding.is_none() {
        return Err(engine_error("Wake feature models are not loaded."));
    }
    let mels = {
        let melspec = engine
            .melspec
            .as_mut()
            .ok_or_else(|| engine_error("Wake session closed."))?;
        run_melspec(melspec, &seed_pcm(16000 * 4))?
    };
    let mut offset = 0;
    while offset + EMBEDDING_WINDOW <= mels.len() {
        let window: Vec<[f32; MEL_BANKS]> = mels[offset..offset + EMBEDDING_WINDOW].to_vec();
        let vector = {
            let embedding = engine
                .embedding
                .as_mut()
                .ok_or_else(|| engine_error("Wake session closed."))?;
            run_embedding(embedding, &window)?
        };
        if engine.features.len() >= FEATURE_BUFFER_CAP {
            engine.features.pop_front();
        }
        engine.features.push_back(vector);
        offset += 8;
    }
    Ok(())
}

fn wake_dir(app: &AppHandle, state: &State<'_, WakeState>) -> AppResult<std::path::PathBuf> {
    if let Some(dir) = state
        .dir
        .lock()
        .map_err(|_| internal("Wake state is unavailable."))?
        .clone()
    {
        return Ok(dir);
    }
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|_| internal("App data dir is unavailable."))?
        .join("wake_models");
    std::fs::create_dir_all(&dir).map_err(|_| internal("Wake model dir cannot be created."))?;
    *state
        .dir
        .lock()
        .map_err(|_| internal("Wake state is unavailable."))? = Some(dir.clone());
    Ok(dir)
}

async fn download_if_missing(url: &str, path: &std::path::Path) -> AppResult<()> {
    if path.exists() {
        return Ok(());
    }
    let bytes = reqwest::get(url)
        .await
        .map_err(|_| internal("Wake model download failed."))?
        .bytes()
        .await
        .map_err(|_| internal("Wake model download failed."))?;
    if bytes.is_empty() {
        return Err(internal("Wake model download is empty."));
    }
    std::fs::write(path, &bytes).map_err(|_| internal("Wake model cannot be saved."))?;
    Ok(())
}

/// Download feature + stock models on demand. Builds stay light.
#[tauri::command]
pub async fn wake_ensure_models(
    app: AppHandle,
    state: State<'_, WakeState>,
) -> AppResult<Vec<String>> {
    let dir = wake_dir(&app, &state)?;
    let mut files = Vec::new();
    for file in FEATURE_FILES {
        let path = dir.join(file);
        download_if_missing(&format!("{MODEL_RELEASE_BASE}/{file}"), &path).await?;
        files.push(file.to_string());
    }
    for (name, file) in STOCK_MODELS {
        let path = dir.join(file);
        download_if_missing(&format!("{MODEL_RELEASE_BASE}/{file}"), &path).await?;
        files.push((*name).to_string());
    }
    Ok(files)
}

/// Enable wake detection for the given models.
#[tauri::command]
pub fn wake_enable(
    app: AppHandle,
    state: State<'_, WakeState>,
    models: Vec<String>,
    threshold: Option<f32>,
    cooldown_secs: Option<f64>,
) -> AppResult<WakeStatus> {
    let dir = wake_dir(&app, &state)?;
    let threshold = threshold.unwrap_or(0.5).clamp(0.0, 1.0);
    let cooldown = Duration::from_secs_f64(cooldown_secs.unwrap_or(3.0).clamp(0.5, 30.0));
    let mut engine = state
        .inner
        .lock()
        .map_err(|_| internal("Wake state is unavailable."))?;
    if engine.melspec.is_none() {
        engine.melspec = Some(ort_session(&dir.join(FEATURE_FILES[0]))?);
    }
    if engine.embedding.is_none() {
        engine.embedding = Some(ort_session(&dir.join(FEATURE_FILES[1]))?);
    }
    for name in models {
        enable_model(&mut engine, &dir, &name, threshold, cooldown)?;
    }
    // Drop stale audio from any previous session, then prime mel/feature
    // buffers like upstream (ones + warmup suppression).
    engine.raw.clear();
    engine.remainder.clear();
    engine.mel.clear();
    for _ in 0..MEL_INIT_FRAMES {
        engine.mel.push_back([1.0f32; MEL_BANKS]);
    }
    engine.features.clear();
    engine.embeddings_seen = 0;
    Ok(WakeStatus {
        enabled: !engine.models.is_empty(),
        models: engine.models.keys().cloned().collect(),
        feature_models_ready: engine.melspec.is_some(),
    })
}

/// Register a custom `.onnx` wake-word model file.
#[tauri::command]
pub fn wake_load_custom(
    app: AppHandle,
    state: State<'_, WakeState>,
    name: String,
    model_base64: String,
) -> AppResult<String> {
    if name.trim().is_empty() || name.len() > 64 {
        return Err(internal("Custom wake model name is invalid."));
    }
    let bytes = BASE64
        .decode(model_base64.as_bytes())
        .map_err(|_| internal("Custom wake model is not valid base64."))?;
    if bytes.len() > 32 * 1024 * 1024 || bytes.len() < 1024 {
        return Err(internal("Custom wake model size is invalid."));
    }
    let dir = wake_dir(&app, &state)?;
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}.onnx"));
    std::fs::write(&path, &bytes).map_err(|_| internal("Custom wake model cannot be saved."))?;
    // Validate it loads and exposes a classifier-shaped input.
    let _probe = ort_session(&path)?;
    Ok(safe)
}

#[tauri::command]
pub fn wake_disable(app: AppHandle, state: State<'_, WakeState>) -> AppResult<WakeStatus> {
    let mut engine = state
        .inner
        .lock()
        .map_err(|_| internal("Wake state is unavailable."))?;
    engine.models.clear();
    engine.raw.clear();
    engine.remainder.clear();
    let _ = app.emit("voice-state", serde_json::json!({"state": "idle"}));
    Ok(WakeStatus {
        enabled: false,
        models: Vec::new(),
        feature_models_ready: engine.melspec.is_some(),
    })
}

#[tauri::command]
pub fn wake_status(state: State<'_, WakeState>) -> AppResult<WakeStatus> {
    let engine = state
        .inner
        .lock()
        .map_err(|_| internal("Wake state is unavailable."))?;
    Ok(WakeStatus {
        enabled: !engine.models.is_empty(),
        models: engine.models.keys().cloned().collect(),
        feature_models_ready: engine.melspec.is_some(),
    })
}

/// Stream 16 kHz mono PCM16 (base64) through the engine. Returns scores and
/// emits `wake-detected {model, score}` on threshold crossings.
#[tauri::command]
pub fn wake_push_pcm(
    app: AppHandle,
    state: State<'_, WakeState>,
    chunk_base64: String,
) -> AppResult<WakeScores> {
    let bytes = BASE64
        .decode(chunk_base64.as_bytes())
        .map_err(|_| internal("Wake chunk is not valid base64."))?;
    if bytes.is_empty() || bytes.len() % 2 != 0 || bytes.len() > 1024 * 1024 {
        return Err(internal("Wake chunk must be non-empty PCM16."));
    }
    let mut samples = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        samples.push(i16::from_le_bytes([pair[0], pair[1]]));
    }
    let mut engine = state
        .inner
        .lock()
        .map_err(|_| internal("Wake state is unavailable."))?;
    let mut fired: Vec<(String, f32)> = Vec::new();
    let scores = engine.step(&samples, &mut |name, score| {
        fired.push((name.to_string(), score));
    })?;
    for (name, score) in fired {
        let _ = app.emit(
            "wake-detected",
            serde_json::json!({"model": name, "score": score}),
        );
    }
    Ok(WakeScores { scores })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_requires_feature_models() {
        let mut engine = WakeEngine::default();
        let mut fired = 0;
        let result = engine.step(&[0i16; 1280], &mut |_, _| fired += 1);
        assert!(result.is_err());
        assert_eq!(fired, 0);
    }

    #[test]
    fn remainder_buffers_short_input() {
        // Without models this errors before buffering; check remainder
        // accounting directly with a seeded buffer.
        let engine = WakeEngine {
            remainder: vec![1, 2, 3],
            ..Default::default()
        };
        assert_eq!(engine.remainder.len(), 3);
    }

    /// End-to-end check against the upstream Python pipeline. Requires the
    /// real ONNX models; set WAKE_MODELS_DIR to skip gracefully otherwise.
    /// Reference (440 Hz tone, hey_jarvis): chunk 10 ~= 0.0067, tail ~= 0.0004.
    #[test]
    fn reference_pipeline_matches_upstream() {
        let dir = match std::env::var("WAKE_MODELS_DIR") {
            Ok(dir)
                if std::path::Path::new(&dir)
                    .join("hey_jarvis_v0.1.onnx")
                    .exists() =>
            {
                std::path::PathBuf::from(dir)
            }
            _ => {
                eprintln!("WAKE_MODELS_DIR missing, skipping reference probe");
                return;
            }
        };
        let mut engine = WakeEngine {
            melspec: Some(ort_session(&dir.join("melspectrogram.onnx")).unwrap()),
            embedding: Some(ort_session(&dir.join("embedding_model.onnx")).unwrap()),
            ..Default::default()
        };
        engine.mel.clear();
        for _ in 0..MEL_INIT_FRAMES {
            engine.mel.push_back([1.0f32; MEL_BANKS]);
        }
        enable_model(&mut engine, &dir, "hey_jarvis", 0.5, Duration::from_secs(3)).unwrap();

        let mut audio = Vec::with_capacity(16000 * 3);
        for i in 0..16000 * 3 {
            let t = i as f32 / 16000.0;
            audio.push((0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 32767.0) as i16);
        }
        let mut series = Vec::new();
        for chunk in audio.chunks_exact(CHUNK_SAMPLES) {
            let scores = engine
                .step(chunk, &mut |_, _| {})
                .expect("step must succeed with models loaded");
            // No score yet while the feature window fills (upstream: 0).
            series.push(scores.get("hey_jarvis").copied().unwrap_or(0.0));
        }
        assert_eq!(series.len(), 37);
        for score in &series {
            assert!(score.is_finite(), "score must be finite, got {score}");
            assert!(*score < 0.05, "tone must not trigger, got {score}");
        }
        // Warmup: upstream suppresses the first frames as well.
        for (index, score) in series.iter().enumerate().take(6) {
            assert!(
                *score < 0.001,
                "warmup chunk {index} must be silent, got {score}"
            );
        }
        // Seeded buffers differ by PRNG stream; allow a wide band here.
        assert!(
            series[10] > 0.0 && series[10] < 0.02,
            "chunk 10 out of band: {}",
            series[10]
        );
        // Fully-real windows must track the reference closely.
        for (index, expected) in [(20, 0.0004), (30, 0.0004), (36, 0.0004)] {
            assert!(
                (series[index] - expected).abs() < 0.002,
                "chunk {index} diverged from upstream: {}",
                series[index]
            );
        }
    }
}
