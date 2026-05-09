use std::path::{Path, PathBuf};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::chunker::AudioChunk;
use crate::paths;

/// Whisper models we expose in the picker. All multilingual — language is
/// auto-detected at transcription time. Keep in sync with
/// `WhisperModel` in `src/settingsStore.ts`.
const ALLOWED_MODELS: &[&str] = &["medium", "large-v3-turbo", "large-v3"];
const DEFAULT_MODEL: &str = "large-v3-turbo";

fn model_filename(model: &str) -> String {
    format!("ggml-{model}.bin")
}

/// Validate a frontend-supplied model name against the picker allowlist,
/// falling back to the default. Used by both the streaming worker preload
/// and the existing single-shot transcribe command.
pub fn resolve_model(model: Option<&str>) -> String {
    model
        .filter(|m| ALLOWED_MODELS.contains(m))
        .unwrap_or(DEFAULT_MODEL)
        .to_string()
}

fn model_url(model: &str) -> String {
    format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{}",
        model_filename(model)
    )
}

/// Audio capture channel for a segment. Used as a hint by the reconcile
/// prompt — `Mic` audio usually but not always comes from the user; `System`
/// audio usually but not always comes from remote participants. Mic-bleed,
/// echo, speakerphone, and shared-room recordings all break that mapping,
/// so the prompt treats this as one signal among several, not ground truth.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AudioSource {
    Mic,
    System,
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
    /// Dominant audio channel during this segment's chunk window (#47). `None`
    /// for transcripts produced before #47, when system audio wasn't enabled
    /// and no labeling happened, or when the segment came from the whole-WAV
    /// fallback path (per-channel RMS history isn't available there).
    #[serde(default)]
    pub source: Option<AudioSource>,
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
    /// True if any chunk transcription failed during the streaming pipeline
    /// (#22). Read by #24's router to decide whether to fall back to a
    /// whole-WAV re-transcribe at Stop. Default false — single-shot and
    /// pre-#24 transcripts never set it.
    #[serde(default)]
    pub had_errors: bool,
}

/// Whisper's hard cap is `n_text_ctx / 2 = 224` tokens for `initial_prompt`.
/// 800 chars is a defensive char-based estimate (~200 tokens) leaving headroom.
const INITIAL_PROMPT_MAX_CHARS: usize = 800;

/// Stop-time entry point. Routes between the streaming finalize path
/// (#22's `transcript-partial.json` is complete → atomic-rename, return
/// in <1 s) and the legacy whole-WAV transcribe (the indefinite safety
/// net for streaming failures, missing models, mid-meeting crashes, and
/// pre-#24 bundles).
#[tauri::command]
pub async fn transcribe(
    app: AppHandle,
    audio_path: String,
    glossary: Vec<String>,
    model: Option<String>,
) -> Result<Transcript, String> {
    if let Some(t) = try_finalize_streaming(&audio_path)? {
        eprintln!("[transcribe] finalized streaming partial");
        return Ok(t);
    }
    eprintln!("[transcribe] no usable streaming partial; running whole-WAV");
    full_transcribe(app, audio_path, glossary, model).await
}

async fn full_transcribe(
    app: AppHandle,
    audio_path: String,
    glossary: Vec<String>,
    model: Option<String>,
) -> Result<Transcript, String> {
    let path = PathBuf::from(&audio_path);
    let initial_prompt = build_initial_prompt(&glossary, None);
    let transcript = transcribe_wav_to_transcript(
        app,
        path.clone(),
        model,
        initial_prompt,
        Some("transcribe-progress"),
    )
    .await?;

    // Sidecar JSON for re-summarization without re-transcribing.
    let json_path = path.with_file_name(crate::notes::TRANSCRIPT_FILENAME);
    std::fs::write(&json_path, serde_json::to_vec_pretty(&transcript).unwrap())
        .map_err(|e| e.to_string())?;

    Ok(transcript)
}

/// Run Whisper inference on a 16-bit PCM 16 kHz mono WAV (the format
/// produced by `audio.rs` and `voice.rs`). Returns the full `Transcript`
/// (segments, full_text, language, duration_ms). Does not write the
/// sidecar JSON — caller decides whether to persist.
///
/// `progress_event`, when `Some`, names the Tauri event channel that
/// receives 0-100 progress integers from whisper.cpp; pass `None` for
/// silent inference (used by the voice-query path which doesn't want to
/// pollute the meeting-transcribe progress UI).
pub async fn transcribe_wav_to_transcript(
    app: AppHandle,
    audio_path: PathBuf,
    model: Option<String>,
    initial_prompt: Option<String>,
    progress_event: Option<&'static str>,
) -> Result<Transcript, String> {
    // Validate against the picker's allowlist; unknown values fall back to
    // the default rather than asking the OS to download an arbitrary URL.
    let model = resolve_model(model.as_deref());
    let model_path = ensure_model(&app, &model).await?;
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
        let mut reader = hound::WavReader::open(&audio_path).map_err(|e| e.to_string())?;
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

        if let Some(event_name) = progress_event {
            let app3 = app2.clone();
            params.set_progress_callback_safe(move |pct: i32| {
                let _ = app3.emit(event_name, pct);
            });
        }

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
                source: None,
            });
        }

        // Diarization is disabled until the chunked clustering pipeline
        // (issue #23) ships with a tuned threshold. The current pyannote
        // + nemo-titanet + 0.5-threshold combo over-segments dramatically
        // on real meeting audio (40+ phantom speakers on tested files),
        // which makes the transcript view harder to read and adds zero
        // signal to the reconcile pass — Claude doesn't get useful
        // attribution from anonymous "Speaker N" tags. The diarize module
        // stays compiled and reachable so #23 can re-enable it.
        let _ = (&mut segments, &app2);

        Ok(Transcript {
            segments,
            full_text,
            language,
            duration_ms,
            num_speakers: None,
            reconciled_at: None,
            had_errors: false,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- streaming finalize (issue #24) ---------------------------------

/// How far behind the WAV's true duration we'll tolerate `partial.duration_ms`
/// before declaring the streaming pipeline incomplete. 2 s covers the in-flight
/// tail at Stop and minor accounting drift; bigger gaps mean we lost a chunk.
const FINALIZE_DURATION_TOLERANCE_MS: u64 = 2_000;

/// Try to finalize a streaming-produced `transcript-partial.json` into the
/// final `transcript.json`. Returns `Ok(Some(_))` on a clean atomic rename,
/// `Ok(None)` to indicate the caller should fall through to whole-WAV. On
/// `Ok(None)` paths, any incomplete/corrupt partial is deleted so retries
/// don't re-evaluate stale data.
fn try_finalize_streaming(audio_path: &str) -> Result<Option<Transcript>, String> {
    let audio = Path::new(audio_path);
    let partial = audio.with_file_name(crate::notes::TRANSCRIPT_PARTIAL_FILENAME);
    let final_path = audio.with_file_name(crate::notes::TRANSCRIPT_FILENAME);

    if !partial.exists() {
        return Ok(None);
    }

    let bytes = match std::fs::read(&partial) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[transcribe] partial read failed ({e}); deleting and falling through");
            let _ = std::fs::remove_file(&partial);
            return Ok(None);
        }
    };
    let transcript: Transcript = match serde_json::from_slice(&bytes) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[transcribe] partial parse failed ({e}); deleting and falling through");
            let _ = std::fs::remove_file(&partial);
            return Ok(None);
        }
    };

    if transcript.had_errors {
        eprintln!("[transcribe] partial has had_errors=true; deleting and falling through");
        let _ = std::fs::remove_file(&partial);
        return Ok(None);
    }

    let wav_ms = wav_duration_ms(audio).map_err(|e| format!("wav header: {e}"))?;
    if transcript.duration_ms + FINALIZE_DURATION_TOLERANCE_MS < wav_ms {
        eprintln!(
            "[transcribe] partial duration {} ms < wav {} ms (tolerance {}); falling through",
            transcript.duration_ms, wav_ms, FINALIZE_DURATION_TOLERANCE_MS
        );
        let _ = std::fs::remove_file(&partial);
        return Ok(None);
    }

    std::fs::rename(&partial, &final_path)
        .map_err(|e| format!("promote partial: {e}"))?;
    Ok(Some(transcript))
}

/// Read just the WAV header to get the recording duration in milliseconds.
/// Microsecond-cheap — used on the Stop-blocking path.
fn wav_duration_ms(path: &Path) -> Result<u64, String> {
    let reader = hound::WavReader::open(path).map_err(|e| e.to_string())?;
    let frames = reader.duration() as u64;
    let sr = reader.spec().sample_rate as u64;
    if sr == 0 {
        return Err("wav sample_rate is zero".into());
    }
    Ok((frames * 1000) / sr)
}

/// Format a glossary (and optionally the prior chunk's transcript tail) into
/// a sentence-form prompt for whisper.cpp's decoder context.
///
/// For the single-shot fallback path, `prev_tail` is `None` and the entire
/// `INITIAL_PROMPT_MAX_CHARS` budget is available for glossary terms.
///
/// For the streaming worker (#22), `prev_tail` carries the last few hundred
/// characters of the accumulated transcript so cross-chunk continuity is
/// preserved. Budget split: 60% glossary / 40% tail. If glossary is short,
/// the unused budget rolls over to tail (and vice versa).
///
/// Returns `None` when there's nothing to feed Whisper — the caller can then
/// skip `set_initial_prompt` entirely.
fn build_initial_prompt(glossary: &[String], prev_tail: Option<&str>) -> Option<String> {
    const GLOSSARY_PREFIX: &str = "Domain terms used in this recording: ";
    const TAIL_SEP: &str = " ";
    const GLOSSARY_BUDGET: usize = INITIAL_PROMPT_MAX_CHARS * 60 / 100; // 480
    const TAIL_BUDGET: usize = INITIAL_PROMPT_MAX_CHARS - GLOSSARY_BUDGET; // 320

    let terms: Vec<&str> = glossary
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    let glossary_part = if terms.is_empty() {
        None
    } else {
        let mut included: Vec<&str> = Vec::with_capacity(terms.len());
        let mut len = GLOSSARY_PREFIX.len() + 1; // +1 for trailing "."
        for t in terms {
            let extra = t.len() + if included.is_empty() { 0 } else { 2 }; // ", "
            if len + extra > GLOSSARY_BUDGET {
                break;
            }
            included.push(t);
            len += extra;
        }
        if included.is_empty() {
            None
        } else {
            Some((format!("{}{}.", GLOSSARY_PREFIX, included.join(", ")), len))
        }
    };

    let tail_text = prev_tail.map(str::trim).filter(|s| !s.is_empty());
    let glossary_used = glossary_part.as_ref().map(|(_, l)| *l).unwrap_or(0);
    // Roll unused glossary budget into tail.
    let tail_budget = if glossary_used < GLOSSARY_BUDGET {
        TAIL_BUDGET + (GLOSSARY_BUDGET - glossary_used)
    } else {
        TAIL_BUDGET
    };

    let tail_trimmed = tail_text.and_then(|t| {
        // Need to leave room for a single space separator.
        let max = tail_budget.saturating_sub(TAIL_SEP.len());
        if max == 0 {
            return None;
        }
        if t.len() <= max {
            return Some(t.to_string());
        }
        // Cut from the head, then snap forward to a whitespace boundary so we
        // don't feed Whisper a half-word fragment.
        let start = t.len() - max;
        let suffix = &t[start..];
        let snapped = match suffix.find(char::is_whitespace) {
            Some(i) => &suffix[i + 1..],
            None => suffix, // single long word — pass as-is
        };
        if snapped.is_empty() {
            None
        } else {
            Some(snapped.to_string())
        }
    });

    match (glossary_part, tail_trimmed) {
        (None, None) => None,
        (Some((g, _)), None) => Some(g),
        (None, Some(t)) => Some(t),
        (Some((g, _)), Some(t)) => Some(format!("{g}{TAIL_SEP}{t}")),
    }
}

/// Returns the local path to the requested Whisper model, downloading it
/// from Hugging Face on first use. Atomic via `.part` rename so a torn
/// download isn't silently loaded as a corrupt model on next run.
pub async fn ensure_model(app: &AppHandle, model: &str) -> Result<PathBuf, String> {
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

// ---------- streaming worker (issue #22) -----------------------------------

/// Tail length carried into the next chunk's `set_initial_prompt`. Matches
/// the tail budget in `build_initial_prompt` so we never over-buffer.
const STREAMING_TAIL_CHARS: usize = INITIAL_PROMPT_MAX_CHARS - (INITIAL_PROMPT_MAX_CHARS * 60 / 100);

/// Backlog above which we log a warning per `recv()` — the worker is
/// falling behind realtime and finalize will be slower at Stop.
const BACKLOG_WARN_THRESHOLD: usize = 3;

/// Long-running thread spawned by `audio::start`. Drains `chunk_rx`, runs
/// Whisper per chunk, accumulates segments, persists `transcript-partial.json`
/// after each chunk, and emits Tauri events for the live UI (#25).
///
/// Exits cleanly when the chunker drops its sender (mixer thread on Stop).
/// On any internal error, logs and continues — recording must never fail
/// because the streaming pipeline broke. #24's fallback path will pick up
/// the master `audio.wav` in that case.
pub fn run_streaming_worker(
    app: AppHandle,
    chunk_rx: Receiver<AudioChunk>,
    bundle_dir: PathBuf,
    model_path: Option<PathBuf>,
    glossary: Vec<String>,
) {
    let model_path = match model_path {
        Some(p) => p,
        None => {
            eprintln!("[transcribe-worker] no model preloaded; streaming disabled");
            // Drain so the channel doesn't block the mixer; #24 falls back to
            // whole-WAV transcription at Stop.
            for _ in chunk_rx.iter() {}
            return;
        }
    };

    // Load Whisper context once. Heavy (~1 GB on large-v3-turbo); the first
    // chunk doesn't arrive for ~120 s, so latency is hidden.
    let mut ctx_params = WhisperContextParameters::default();
    ctx_params.use_gpu = true;
    let ctx = match model_path
        .to_str()
        .ok_or_else(|| "model path not utf-8".to_string())
        .and_then(|p| WhisperContext::new_with_params(p, ctx_params).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[transcribe-worker] WhisperContext init failed: {e}");
            for _ in chunk_rx.iter() {}
            return;
        }
    };

    let partial_path = bundle_dir.join(crate::notes::TRANSCRIPT_PARTIAL_FILENAME);

    let mut acc = Transcript {
        segments: Vec::new(),
        full_text: String::new(),
        language: String::new(),
        duration_ms: 0,
        num_speakers: None,
        reconciled_at: None,
        had_errors: false,
    };
    let mut prev_tail: Option<String> = None;
    let mut chunk_index: u32 = 0;

    while let Ok(chunk) = chunk_rx.recv() {
        let backlog = chunk_rx.len();
        if backlog > BACKLOG_WARN_THRESHOLD {
            eprintln!(
                "[transcribe-worker] backlog of {} chunks; finalize will be slower",
                backlog
            );
        }

        let initial_prompt = build_initial_prompt(&glossary, prev_tail.as_deref());
        match transcribe_one_chunk(&ctx, &chunk.samples, initial_prompt.as_deref(), chunk.start_ms)
        {
            Ok((mut segments, chunk_text, lang_id)) => {
                if acc.language.is_empty() {
                    acc.language = lang_id
                        .filter(|id| *id >= 0)
                        .and_then(whisper_rs::get_lang_str)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "und".to_string());
                }
                acc.full_text.push_str(&chunk_text);
                // Stamp the chunk's dominant-channel label onto every segment
                // it produced. Sub-chunk channel switches collapse to one
                // label; acceptable for a hint (#47).
                for seg in segments.iter_mut() {
                    seg.source = chunk.source;
                }
                let segments_for_event = segments.clone();
                acc.segments.extend(segments);
                acc.duration_ms = chunk.end_ms;

                prev_tail = Some(tail_chars(&acc.full_text, STREAMING_TAIL_CHARS));

                if let Err(e) = write_partial(&partial_path, &acc) {
                    eprintln!("[transcribe-worker] partial write failed: {e}");
                }

                let _ = app.emit(
                    "chunk-processed",
                    serde_json::json!({
                        "chunk_index": chunk_index,
                        "end_ms": chunk.end_ms,
                    }),
                );
                let _ = app.emit(
                    "chunk-transcribed",
                    serde_json::json!({
                        "chunk_index": chunk_index,
                        "segments": segments_for_event,
                    }),
                );
                eprintln!(
                    "[transcribe-worker] chunk {} done ({}..{} ms, {:?}, {} segments, lang={})",
                    chunk_index,
                    chunk.start_ms,
                    chunk.end_ms,
                    chunk.boundary,
                    segments_for_event.len(),
                    acc.language,
                );
            }
            Err(e) => {
                eprintln!(
                    "[transcribe-worker] chunk {} ({}..{} ms) failed: {e}",
                    chunk_index, chunk.start_ms, chunk.end_ms
                );
                acc.had_errors = true;
                // Persist the flag so #24's router falls back to whole-WAV
                // even if the worker crashes before the next successful
                // chunk would have flushed it to disk.
                if let Err(e) = write_partial(&partial_path, &acc) {
                    eprintln!("[transcribe-worker] partial write failed: {e}");
                }
            }
        }
        chunk_index += 1;
    }
}

/// Run Whisper on one chunk's samples. Returns the segments (with absolute
/// timestamps offset by `chunk_offset_ms`), the chunk's contribution to
/// `full_text`, and the detected language id (if any).
fn transcribe_one_chunk(
    ctx: &WhisperContext,
    pcm_f32: &[f32],
    initial_prompt: Option<&str>,
    chunk_offset_ms: u64,
) -> Result<(Vec<Segment>, String, Option<i32>), String> {
    let mut state = ctx.create_state().map_err(|e| e.to_string())?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(None);
    params.set_no_context(true);
    params.set_translate(false);
    params.set_print_progress(false);
    params.set_print_special(false);
    params.set_print_realtime(false);
    if let Some(p) = initial_prompt {
        params.set_initial_prompt(p);
    }

    state.full(params, pcm_f32).map_err(|e| e.to_string())?;

    let lang_id = state.full_lang_id_from_state();
    let lang = if lang_id >= 0 { Some(lang_id) } else { None };

    let n = state.full_n_segments();
    let mut segments = Vec::with_capacity(n as usize);
    let mut chunk_text = String::new();
    for i in 0..n {
        let seg = state
            .get_segment(i)
            .ok_or_else(|| format!("missing segment {i}"))?;
        // whisper.cpp t0/t1 are 10-ms ticks relative to the chunk's t=0.
        let start_ms = (seg.start_timestamp() as u64 * 10) + chunk_offset_ms;
        let end_ms = (seg.end_timestamp() as u64 * 10) + chunk_offset_ms;
        let text = seg.to_str().map_err(|e| e.to_string())?.to_string();
        chunk_text.push_str(&text);
        segments.push(Segment {
            start_ms,
            end_ms,
            text,
            speaker: None,
            source: None,
        });
    }

    Ok((segments, chunk_text, lang))
}

fn write_partial(path: &Path, transcript: &Transcript) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(transcript).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Take the last `max` chars of `s`, snapped forward to a whitespace boundary
/// so we don't carry a half-word into the next chunk's prompt.
fn tail_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.len() - max;
    let suffix = &s[start..];
    match suffix.find(char::is_whitespace) {
        Some(i) => suffix[i + 1..].to_string(),
        None => suffix.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_initial_prompt_glossary_only() {
        let g: Vec<String> = vec!["foo".into(), "bar".into()];
        let p = build_initial_prompt(&g, None).unwrap();
        assert_eq!(p, "Domain terms used in this recording: foo, bar.");
    }

    #[test]
    fn build_initial_prompt_returns_none_when_empty() {
        let g: Vec<String> = vec![];
        assert!(build_initial_prompt(&g, None).is_none());
        assert!(build_initial_prompt(&g, Some("   ")).is_none());
    }

    #[test]
    fn build_initial_prompt_with_tail_appends() {
        let g: Vec<String> = vec!["foo".into()];
        let tail = "and then we discussed the roadmap";
        let p = build_initial_prompt(&g, Some(tail)).unwrap();
        assert!(p.starts_with("Domain terms used in this recording: foo."));
        assert!(p.ends_with(tail));
    }

    #[test]
    fn build_initial_prompt_tail_only() {
        let g: Vec<String> = vec![];
        let tail = "previous chunk text";
        let p = build_initial_prompt(&g, Some(tail)).unwrap();
        assert_eq!(p, tail);
    }

    #[test]
    fn build_initial_prompt_tail_truncates_at_word_boundary() {
        // Force tail truncation by choosing a tail longer than the full
        // budget (since glossary is empty, tail gets the whole budget).
        let g: Vec<String> = vec![];
        let long_tail: String = (0..200).map(|i| format!("word{i} ")).collect();
        let p = build_initial_prompt(&g, Some(&long_tail)).unwrap();
        assert!(p.len() <= INITIAL_PROMPT_MAX_CHARS);
        // No leading partial word — must start with "wordN " (full token).
        assert!(p.starts_with("word"));
        let first_word_end = p.find(' ').unwrap();
        let first_word = &p[..first_word_end];
        // Confirm the first word is a complete "wordN" token.
        assert!(first_word.starts_with("word"));
        assert!(first_word[4..].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn build_initial_prompt_glossary_overflow_does_not_starve_tail() {
        // Glossary that fits inside its 60% budget (480 chars), plus a
        // short tail — both should appear in the result.
        let big_term: String = "x".repeat(400);
        let g: Vec<String> = vec![big_term];
        let tail = "carry over";
        let p = build_initial_prompt(&g, Some(tail)).unwrap();
        assert!(p.contains("xxxxxxxxxx"));
        assert!(p.ends_with("carry over"));
    }

    #[test]
    fn tail_chars_returns_whole_when_short() {
        assert_eq!(tail_chars("hello world", 100), "hello world");
    }

    #[test]
    fn tail_chars_snaps_forward_to_whitespace() {
        // max=7 puts the cut at index 8 ("baz qux"); the suffix starts
        // mid-word at "ar baz qux" if max=10 → snap past "ar" to "baz qux".
        assert_eq!(tail_chars("foo bar baz qux", 10), "baz qux");
        // max=6 → suffix "z qux", snap past "z" → "qux".
        assert_eq!(tail_chars("foo bar baz qux", 6), "qux");
    }

    #[test]
    fn tail_chars_returns_suffix_when_no_whitespace() {
        let s = "abcdefghij";
        assert_eq!(tail_chars(s, 4), "ghij");
    }

    // ---- try_finalize_streaming (issue #24) -----------------------------

    use tempfile::TempDir;

    /// Write a 16 kHz mono int16 WAV of `duration_ms` of silence — gives the
    /// tests a real header to read for the duration check.
    fn write_silent_wav(path: &Path, duration_ms: u64) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let n = (16_000 * duration_ms / 1000) as usize;
        for _ in 0..n {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();
    }

    fn make_transcript(duration_ms: u64, had_errors: bool) -> Transcript {
        Transcript {
            segments: vec![Segment {
                start_ms: 0,
                end_ms: duration_ms,
                text: "hello".into(),
                speaker: None,
                source: None,
            }],
            full_text: "hello".into(),
            language: "en".into(),
            duration_ms,
            num_speakers: None,
            reconciled_at: None,
            had_errors,
        }
    }

    #[test]
    fn try_finalize_returns_none_when_partial_missing() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("audio.wav");
        write_silent_wav(&audio, 30_000);
        let result = try_finalize_streaming(audio.to_str().unwrap()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn try_finalize_returns_none_when_had_errors_true() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("audio.wav");
        let partial = dir.path().join(crate::notes::TRANSCRIPT_PARTIAL_FILENAME);
        write_silent_wav(&audio, 30_000);
        let t = make_transcript(30_000, true);
        std::fs::write(&partial, serde_json::to_vec_pretty(&t).unwrap()).unwrap();

        let result = try_finalize_streaming(audio.to_str().unwrap()).unwrap();
        assert!(result.is_none());
        assert!(!partial.exists(), "stale partial should be deleted");
    }

    #[test]
    fn try_finalize_returns_none_when_duration_short() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("audio.wav");
        let partial = dir.path().join(crate::notes::TRANSCRIPT_PARTIAL_FILENAME);
        write_silent_wav(&audio, 30_000);
        // Partial covers only 20 s of a 30 s WAV — outside the 2 s tolerance.
        let t = make_transcript(20_000, false);
        std::fs::write(&partial, serde_json::to_vec_pretty(&t).unwrap()).unwrap();

        let result = try_finalize_streaming(audio.to_str().unwrap()).unwrap();
        assert!(result.is_none());
        assert!(!partial.exists());
    }

    #[test]
    fn try_finalize_returns_none_on_corrupt_partial() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("audio.wav");
        let partial = dir.path().join(crate::notes::TRANSCRIPT_PARTIAL_FILENAME);
        write_silent_wav(&audio, 30_000);
        std::fs::write(&partial, b"not json {{{{").unwrap();

        let result = try_finalize_streaming(audio.to_str().unwrap()).unwrap();
        assert!(result.is_none());
        assert!(!partial.exists());
    }

    #[test]
    fn try_finalize_promotes_when_complete() {
        let dir = TempDir::new().unwrap();
        let audio = dir.path().join("audio.wav");
        let partial = dir.path().join(crate::notes::TRANSCRIPT_PARTIAL_FILENAME);
        let final_path = dir.path().join(crate::notes::TRANSCRIPT_FILENAME);
        write_silent_wav(&audio, 30_000);
        // Within tolerance: partial 29.5s, wav 30s.
        let t = make_transcript(29_500, false);
        std::fs::write(&partial, serde_json::to_vec_pretty(&t).unwrap()).unwrap();

        let result = try_finalize_streaming(audio.to_str().unwrap()).unwrap();
        let promoted = result.expect("expected Some(Transcript)");
        assert_eq!(promoted.duration_ms, 29_500);
        assert!(!partial.exists(), "partial should be renamed away");
        assert!(final_path.exists(), "final transcript should exist");
    }
}
