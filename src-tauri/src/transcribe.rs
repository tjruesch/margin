use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::paths;

/// Whisper models we expose in the picker. All multilingual — language is
/// auto-detected at transcription time. Keep in sync with
/// `WhisperModel` in `src/settingsStore.ts`.
const ALLOWED_MODELS: &[&str] = &["medium", "large-v3-turbo", "large-v3"];
const DEFAULT_MODEL: &str = "large-v3-turbo";

fn model_filename(model: &str) -> String {
    format!("ggml-{model}.bin")
}

fn model_url(model: &str) -> String {
    format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
        model_filename(model)
    )
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
    /// Speaker index from diarization, when available. `None` for transcripts
    /// produced before diarization shipped, or when diarization fails / yields
    /// no overlapping span for this segment.
    #[serde(default)]
    pub speaker: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Transcript {
    pub segments: Vec<Segment>,
    pub full_text: String,
    pub language: String,
    pub duration_ms: u64,
    /// Number of distinct speakers detected by diarization. `None` if
    /// diarization didn't run.
    #[serde(default)]
    pub num_speakers: Option<u32>,
    /// Unix-ms timestamp set by reconcile_notes when Claude has produced
    /// reconciled output for this transcript. Drives the post-recording
    /// banner: if Some, the user has already generated notes once and
    /// the Generate CTA is suppressed.
    #[serde(default)]
    pub reconciled_at: Option<u64>,
}

/// Whisper's hard cap is `n_text_ctx / 2 = 224` tokens for `initial_prompt`.
/// 800 chars is a defensive char-based estimate (~200 tokens) leaving headroom.
const INITIAL_PROMPT_MAX_CHARS: usize = 800;

#[tauri::command]
pub async fn transcribe(
    app: AppHandle,
    audio_path: String,
    glossary: Vec<String>,
    model: Option<String>,
) -> Result<Transcript, String> {
    let path = PathBuf::from(audio_path);
    // Validate against the picker's allowlist; unknown values fall back to
    // the default rather than asking the OS to download an arbitrary URL.
    let model = model
        .as_deref()
        .filter(|m| ALLOWED_MODELS.contains(m))
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let model_path = ensure_model(&app, &model).await?;
    let diar_paths = crate::diarize::ensure_diarization_models(&app).await?;
    let app2 = app.clone();
    let initial_prompt = build_initial_prompt(&glossary);

    tauri::async_runtime::spawn_blocking(move || -> Result<Transcript, String> {
        // Load Whisper model + state (Metal acceleration on Apple Silicon).
        let mut ctx_params = WhisperContextParameters::default();
        ctx_params.use_gpu = true;
        let ctx = WhisperContext::new_with_params(
            model_path
                .to_str()
                .ok_or_else(|| "model path not utf-8".to_string())?,
            ctx_params,
        )
        .map_err(|e| e.to_string())?;
        let mut state = ctx.create_state().map_err(|e| e.to_string())?;

        // Read 16-bit PCM 16 kHz mono WAV (per audio.rs spec).
        let mut reader = hound::WavReader::open(&path).map_err(|e| e.to_string())?;
        let pcm_i16: Vec<i16> = reader.samples::<i16>().filter_map(|s| s.ok()).collect();
        let mut pcm_f32 = vec![0.0f32; pcm_i16.len()];
        whisper_rs::convert_integer_to_float_audio(&pcm_i16, &mut pcm_f32)
            .map_err(|e| e.to_string())?;

        let duration_ms = (pcm_f32.len() as u64 * 1000) / 16_000;

        // Run Whisper. Language is auto-detected per recording — passing
        // None tells whisper.cpp to run its built-in language ID on the
        // first 30s window and then transcribe with the detected
        // language's decoder head. Do NOT also set `detect_language: true`
        // — that's a detection-only mode in whisper.cpp that returns
        // before producing any segments. (Hit on a German meeting that
        // came back with language="de" but n_segments=0.)
        // `no_context` prevents prior-window text from bleeding into
        // subsequent windows; without this, a degraded window's output
        // can self-reinforce into a phrase loop that continues for the
        // rest of the meeting (observed catastrophic failure on the same
        // recording with the old base.en pipeline forcing English).
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(None);
        params.set_no_context(true);
        params.set_translate(false);
        params.set_print_progress(false);
        params.set_print_special(false);
        params.set_print_realtime(false);
        if let Some(p) = initial_prompt.as_deref() {
            params.set_initial_prompt(p);
        }

        let app3 = app2.clone();
        params.set_progress_callback_safe(move |pct: i32| {
            let _ = app3.emit("transcribe-progress", pct);
        });

        state.full(params, &pcm_f32).map_err(|e| e.to_string())?;

        // Whisper sets full_lang_id_from_state once detection completes.
        // -1 means "couldn't detect" — fall back to "und" (undetermined,
        // ISO 639-2) so the field is never an empty string.
        let lang_id = state.full_lang_id_from_state();
        let language = if lang_id >= 0 {
            whisper_rs::get_lang_str(lang_id)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "und".to_string())
        } else {
            "und".to_string()
        };

        // Collect segments — whisper.cpp t0/t1 are 10-ms ticks → multiply by 10 for ms.
        let n = state.full_n_segments();
        let mut segments = Vec::with_capacity(n as usize);
        let mut full_text = String::new();
        for i in 0..n {
            let seg = state
                .get_segment(i)
                .ok_or_else(|| format!("missing segment {i}"))?;
            let start_ms = seg.start_timestamp() as u64 * 10;
            let end_ms = seg.end_timestamp() as u64 * 10;
            let text = seg
                .to_str()
                .map_err(|e| e.to_string())?
                .to_string();
            full_text.push_str(&text);
            segments.push(Segment {
                start_ms,
                end_ms,
                text,
                speaker: None,
            });
        }

        // Diarization phase: run sherpa-onnx on the same PCM buffer Whisper
        // just consumed. The UI swaps the "Transcribing…" label to
        // "Identifying speakers…" when this fires.
        let _ = app2.emit("transcribe-phase", "diarizing");
        let num_speakers = match crate::diarize::diarize(&diar_paths, &pcm_f32) {
            Ok(spans) => {
                crate::diarize::assign_speakers(&mut segments, &spans);
                Some(crate::diarize::count_unique_speakers(&segments))
            }
            Err(e) => {
                // Diarization failure shouldn't kill the transcript — the
                // Whisper output is still useful without speaker labels.
                eprintln!("[diarize] failed, continuing without speaker labels: {e}");
                None
            }
        };

        let transcript = Transcript {
            segments,
            full_text,
            language,
            duration_ms,
            num_speakers,
            reconciled_at: None,
        };

        // Sidecar JSON for re-summarization without re-transcribing.
        let json_path = path.with_file_name(crate::notes::TRANSCRIPT_FILENAME);
        std::fs::write(&json_path, serde_json::to_vec_pretty(&transcript).unwrap())
            .map_err(|e| e.to_string())?;

        Ok(transcript)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Format a glossary into a sentence-form prompt that whisper.cpp's decoder
/// can use as prior context. Returns None for an empty glossary so the caller
/// can skip `set_initial_prompt` entirely (passing an empty string to the
/// binding is safe but pointless).
///
/// Truncation: drop terms from the end until under the char cap, preserving
/// the user's ordering from the textarea.
fn build_initial_prompt(glossary: &[String]) -> Option<String> {
    let terms: Vec<&str> = glossary
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if terms.is_empty() {
        return None;
    }
    let prefix = "Domain terms used in this recording: ";
    let mut included: Vec<&str> = Vec::with_capacity(terms.len());
    let mut len = prefix.len() + 1; // +1 for trailing "."
    for t in terms {
        let extra = t.len() + if included.is_empty() { 0 } else { 2 }; // ", "
        if len + extra > INITIAL_PROMPT_MAX_CHARS {
            break;
        }
        included.push(t);
        len += extra;
    }
    if included.is_empty() {
        return None;
    }
    Some(format!("{}{}.", prefix, included.join(", ")))
}

/// Returns the local path to the requested Whisper model, downloading it
/// from Hugging Face on first use. Atomic via `.part` rename so a torn
/// download isn't silently loaded as a corrupt model on next run.
async fn ensure_model(app: &AppHandle, model: &str) -> Result<PathBuf, String> {
    let path = paths::models_dir().join(model_filename(model));
    if path.exists() {
        return Ok(path);
    }

    let resp = reqwest::get(model_url(model))
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let total = resp.content_length().unwrap_or(0);

    let tmp = path.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| e.to_string())?;

    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        let _ = app.emit(
            "model-download-progress",
            serde_json::json!({ "downloaded": downloaded, "total": total }),
        );
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| e.to_string())?;

    Ok(path)
}
