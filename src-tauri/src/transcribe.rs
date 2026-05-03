use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::paths;

const MODEL_FILENAME: &str = "ggml-base.en.bin";
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin";

#[derive(Serialize, Deserialize, Clone)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Transcript {
    pub segments: Vec<Segment>,
    pub full_text: String,
    pub language: String,
    pub duration_ms: u64,
}

#[tauri::command]
pub async fn transcribe(app: AppHandle, audio_path: String) -> Result<Transcript, String> {
    let path = PathBuf::from(audio_path);
    let model_path = ensure_model(&app).await?;
    let app2 = app.clone();

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

        // Run Whisper.
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_translate(false);
        params.set_print_progress(false);
        params.set_print_special(false);
        params.set_print_realtime(false);

        let app3 = app2.clone();
        params.set_progress_callback_safe(move |pct: i32| {
            let _ = app3.emit("transcribe-progress", pct);
        });

        state.full(params, &pcm_f32).map_err(|e| e.to_string())?;

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
            });
        }

        let transcript = Transcript {
            segments,
            full_text,
            language: "en".into(),
            duration_ms,
        };

        // Sidecar JSON for re-summarization without re-transcribing.
        let json_path = path.with_extension("transcript.json");
        std::fs::write(&json_path, serde_json::to_vec_pretty(&transcript).unwrap())
            .map_err(|e| e.to_string())?;

        Ok(transcript)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Returns the local path to the Whisper model, downloading it from
/// Hugging Face on first use. Atomic via `.part` rename so a torn
/// download isn't silently loaded as a corrupt model on next run.
async fn ensure_model(app: &AppHandle) -> Result<PathBuf, String> {
    let path = paths::models_dir().join(MODEL_FILENAME);
    if path.exists() {
        return Ok(path);
    }

    let resp = reqwest::get(MODEL_URL)
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
