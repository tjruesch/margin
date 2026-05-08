//! On-the-fly audio chunker for the streaming meeting pipeline.
//!
//! Sits inside the mixer thread (`audio::run_mixer_thread`) and converts the
//! continuous 16 kHz mono mixed stream into discrete `AudioChunk`s suitable
//! for downstream Whisper transcription (#22). Cuts are aligned to silence
//! windows detected by sherpa-onnx Silero VAD when possible, with a hard cap
//! so a meeting that's all monologue still produces chunks.
//!
//! Foundation only — no transcription, no embedding, no UI surfacing. Those
//! land in #22, #23, #25 of epic #26.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crossbeam_channel::Sender;
use sherpa_onnx::{SileroVadModelConfig, VadModelConfig, VoiceActivityDetector};
use tauri::{AppHandle, Emitter};

use crate::paths;
use crate::transcribe::AudioSource;

const SAMPLE_RATE: u32 = 16_000;
const SAMPLES_PER_MS: u64 = 16; // 16_000 / 1000

const TARGET_CHUNK_MS: u64 = 120_000; // ~2 min — try to cut here on silence
const FORCED_CHUNK_MS: u64 = 180_000; // ~3 min — hard cap, cut even mid-word
const SILENCE_SCAN_WINDOW_MS: u64 = 10_000; // look back over last 10 s for a cut point
const MIN_SILENCE_MS: u64 = 400; // a cut-worthy silence must span at least this long

const VAD_MODEL_FILENAME: &str = "silero_vad.onnx";
const VAD_MODEL_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";

/// One emitted chunk of mixed mono 16 kHz audio.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub start_ms: u64,
    pub end_ms: u64,
    pub samples: Vec<f32>,
    pub boundary: BoundaryKind,
    /// Dominant audio channel during this chunk's window (#47). `None` when
    /// system audio wasn't enabled or we have no labeling for any other
    /// reason. Propagated to every segment whisper produces from this chunk
    /// so the reconcile prompt can use it as one signal among several.
    pub source: Option<AudioSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// Cut aligned to a VAD-detected silence window in the recent tail.
    Vad,
    /// Hard-capped cut — either past `FORCED_CHUNK_MS` with no silence found,
    /// VAD model unavailable (degraded mode), or final residual on Stop.
    Forced,
}

/// Streaming chunker. Owns a buffer of unemitted samples plus an optional VAD
/// instance and a parallel silence track recording per-batch speech state.
pub struct Chunker {
    out: Sender<AudioChunk>,
    vad: Option<VoiceActivityDetector>,
    /// Unemitted mixed samples since the last cut.
    buffer: Vec<f32>,
    /// Per-batch (buffer_offset_after_push, is_silence) records. Offsets are
    /// relative to the current buffer start (i.e., the last cut). Pruned and
    /// re-based on each emit.
    silence_track: VecDeque<(u64, bool)>,
    /// Absolute sample offset of the current chunk's start, since chunker
    /// creation. Drives `start_ms` / `end_ms` on emitted chunks.
    chunk_start_sample: u64,
    /// Most recent dominant-channel label observed via `push` since the last
    /// emit. Stamped on the next chunk emission and reset. `None` when no
    /// caller has supplied a label (system audio off, or pre-#47 plumbing).
    pending_source: Option<AudioSource>,
}

impl Chunker {
    pub fn new(out: Sender<AudioChunk>, vad_model: Option<&Path>) -> Self {
        let vad = vad_model.and_then(|path| build_vad(path).map_err(log_vad_err).ok());
        if vad.is_none() && vad_model.is_some() {
            eprintln!("[chunker] VAD init failed; falling back to time-only chunking");
        } else if vad_model.is_none() {
            eprintln!("[chunker] no VAD model; using time-only chunking");
        }
        Self {
            out,
            vad,
            buffer: Vec::with_capacity((FORCED_CHUNK_MS * SAMPLES_PER_MS) as usize),
            silence_track: VecDeque::new(),
            chunk_start_sample: 0,
            pending_source: None,
        }
    }

    /// Append mixed mono samples and emit any chunks that crossed thresholds.
    /// `source` is the dominant-channel label currently in effect (#47); the
    /// most recent non-None value is stamped on the next emitted chunk.
    pub fn push(&mut self, samples: &[f32], source: Option<AudioSource>) {
        if let Some(s) = source {
            self.pending_source = Some(s);
        }
        if samples.is_empty() {
            return;
        }
        self.buffer.extend_from_slice(samples);

        if let Some(vad) = self.vad.as_ref() {
            vad.accept_waveform(samples);
            // `detected()` reflects current-frame speech state. Recording
            // !detected at the buffer's new tail gives us a per-batch
            // silence track, granular to whatever batch size the mixer
            // hands us (typically 1024 samples = 64 ms).
            self.silence_track
                .push_back((self.buffer.len() as u64, !vad.detected()));
        }

        loop {
            let buffer_ms = (self.buffer.len() as u64) / SAMPLES_PER_MS;

            if buffer_ms >= FORCED_CHUNK_MS {
                let cut = FORCED_CHUNK_MS * SAMPLES_PER_MS;
                self.emit(cut, BoundaryKind::Forced);
                continue;
            }

            if buffer_ms < TARGET_CHUNK_MS {
                break;
            }

            // Past target. Try a VAD-aligned cut; otherwise wait for more.
            if self.vad.is_some() {
                let track: Vec<(u64, bool)> = self.silence_track.iter().copied().collect();
                let scan_back = SILENCE_SCAN_WINDOW_MS * SAMPLES_PER_MS;
                let min_silence = MIN_SILENCE_MS * SAMPLES_PER_MS;
                match pick_cut_point(&track, self.buffer.len() as u64, scan_back, min_silence) {
                    Some(cut) => {
                        self.emit(cut, BoundaryKind::Vad);
                        continue;
                    }
                    None => break,
                }
            } else {
                // Degraded: cut at exactly target.
                let cut = TARGET_CHUNK_MS * SAMPLES_PER_MS;
                self.emit(cut, BoundaryKind::Forced);
                continue;
            }
        }
    }

    /// Emit any residual buffer. Called once when the mixer thread is about
    /// to exit. Drops the channel sender on the way out so the downstream
    /// drain thread can terminate.
    pub fn flush(&mut self) {
        if !self.buffer.is_empty() {
            let cut = self.buffer.len() as u64;
            self.emit(cut, BoundaryKind::Forced);
        }
    }

    fn emit(&mut self, cut_samples: u64, boundary: BoundaryKind) {
        let cut = cut_samples as usize;
        debug_assert!(cut > 0 && cut <= self.buffer.len());

        let samples: Vec<f32> = self.buffer.drain(..cut).collect();
        let n = samples.len() as u64;

        let start_ms = self.chunk_start_sample / SAMPLES_PER_MS;
        let end_ms = (self.chunk_start_sample + n) / SAMPLES_PER_MS;

        // Re-base silence track to the new buffer start.
        let cut_u64 = n;
        self.silence_track.retain(|(off, _)| *off > cut_u64);
        for entry in self.silence_track.iter_mut() {
            entry.0 -= cut_u64;
        }

        self.chunk_start_sample += n;

        let chunk = AudioChunk {
            start_ms,
            end_ms,
            samples,
            boundary,
            source: self.pending_source,
        };
        // If the receiver is gone, the meeting is being torn down — drop.
        let _ = self.out.send(chunk);
    }
}

fn log_vad_err(e: String) -> String {
    eprintln!("[chunker] VAD init error: {e}");
    e
}

fn build_vad(model: &Path) -> Result<VoiceActivityDetector, String> {
    let cfg = VadModelConfig {
        silero_vad: SileroVadModelConfig {
            model: Some(model.to_string_lossy().into_owned()),
            threshold: 0.5,
            min_silence_duration: 0.25,
            min_speech_duration: 0.25,
            window_size: 512,
            max_speech_duration: 0.0,
        },
        sample_rate: SAMPLE_RATE as i32,
        num_threads: 1,
        ..Default::default()
    };
    VoiceActivityDetector::create(&cfg, 60.0)
        .ok_or_else(|| "sherpa-onnx: VoiceActivityDetector::create failed".to_string())
}

/// Pure helper. Given a silence track (sorted by offset), find the longest
/// contiguous silence run within `[buffer_samples - scan_back_samples,
/// buffer_samples]`. If it spans `>= min_silence_samples`, return its
/// midpoint. Otherwise `None`.
///
/// `silence_track` entries are `(buffer_offset, is_silence)` recorded after
/// each VAD-fed batch. Treats consecutive same-state entries as one run.
fn pick_cut_point(
    silence_track: &[(u64, bool)],
    buffer_samples: u64,
    scan_back_samples: u64,
    min_silence_samples: u64,
) -> Option<u64> {
    if silence_track.is_empty() {
        return None;
    }
    let scan_lo = buffer_samples.saturating_sub(scan_back_samples);

    // Walk entries in scan window. A run starts at the first silent entry's
    // offset and ends at the next non-silent entry's offset (or buffer_samples
    // if silence persists to the tail).
    let mut best: Option<(u64, u64)> = None; // (start, end)
    let mut cur_start: Option<u64> = None;
    let mut last_off: u64 = scan_lo;

    for &(off, is_silence) in silence_track {
        if off < scan_lo {
            // Before the scan window — but the *state* at scan_lo is set by
            // the most recent entry. Track last seen state implicitly.
            if is_silence {
                cur_start = Some(scan_lo);
            } else {
                cur_start = None;
            }
            last_off = scan_lo;
            continue;
        }
        if is_silence {
            if cur_start.is_none() {
                cur_start = Some(off.max(scan_lo));
            }
        } else if let Some(start) = cur_start.take() {
            let end = off;
            consider_run(&mut best, start, end);
        }
        last_off = off;
    }
    // Silence run extends to the buffer tail.
    if let Some(start) = cur_start {
        consider_run(&mut best, start, buffer_samples.max(last_off));
    }

    best.and_then(|(s, e)| {
        if e.saturating_sub(s) >= min_silence_samples {
            Some((s + e) / 2)
        } else {
            None
        }
    })
}

fn consider_run(best: &mut Option<(u64, u64)>, start: u64, end: u64) {
    let len = end.saturating_sub(start);
    match *best {
        Some((bs, be)) if be - bs >= len => {}
        _ => *best = Some((start, end)),
    }
}

/// Ensure the Silero VAD ONNX model exists locally. Mirrors the
/// `transcribe::ensure_model` and `diarize::download` patterns: atomic
/// `.part`-then-rename, progress events on `model-download-progress`.
pub async fn ensure_vad_model(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = paths::models_dir().join("vad");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create vad dir: {e}"))?;

    let target = dir.join(VAD_MODEL_FILENAME);
    if target.exists() {
        return Ok(target);
    }

    let resp = reqwest::get(VAD_MODEL_URL)
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let total = resp.content_length().unwrap_or(0);

    let tmp = target.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| e.to_string())?;

    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let mut stream = resp.bytes_stream();
    let mut downloaded: u64 = 0;
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
    tokio::fs::rename(&tmp, &target)
        .await
        .map_err(|e| e.to_string())?;

    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples_for_ms(ms: u64) -> u64 {
        ms * SAMPLES_PER_MS
    }

    #[test]
    fn pick_cut_point_finds_midpoint_of_longest_silence() {
        // Buffer at 130_000 ms. Two silence runs in the scan window
        // (last 10 s = 120_000..130_000): a short one (200 ms) and a
        // long one (1500 ms). Longest wins.
        let buffer = samples_for_ms(130_000);
        let track = vec![
            (samples_for_ms(120_500), true),
            (samples_for_ms(120_700), false), // 200 ms silence run
            (samples_for_ms(125_000), true),
            (samples_for_ms(126_500), false), // 1500 ms silence run — winner
            (samples_for_ms(129_000), false),
        ];
        let cut = pick_cut_point(
            &track,
            buffer,
            samples_for_ms(SILENCE_SCAN_WINDOW_MS),
            samples_for_ms(MIN_SILENCE_MS),
        )
        .expect("expected a cut");
        let cut_ms = cut / SAMPLES_PER_MS;
        assert_eq!(cut_ms, (125_000 + 126_500) / 2);
    }

    #[test]
    fn pick_cut_point_returns_none_when_no_silence_run_meets_minimum() {
        let buffer = samples_for_ms(125_000);
        // Only 100 ms silence — below MIN_SILENCE_MS=400.
        let track = vec![
            (samples_for_ms(124_000), true),
            (samples_for_ms(124_100), false),
        ];
        let cut = pick_cut_point(
            &track,
            buffer,
            samples_for_ms(SILENCE_SCAN_WINDOW_MS),
            samples_for_ms(MIN_SILENCE_MS),
        );
        assert!(cut.is_none());
    }

    #[test]
    fn pick_cut_point_ignores_silence_outside_scan_window() {
        let buffer = samples_for_ms(125_000);
        // Long silence at 100s, well outside the last 10s scan window.
        let track = vec![
            (samples_for_ms(100_000), true),
            (samples_for_ms(110_000), false),
        ];
        let cut = pick_cut_point(
            &track,
            buffer,
            samples_for_ms(SILENCE_SCAN_WINDOW_MS),
            samples_for_ms(MIN_SILENCE_MS),
        );
        assert!(cut.is_none());
    }

    #[test]
    fn pick_cut_point_handles_empty_track() {
        let buffer = samples_for_ms(125_000);
        let track: Vec<(u64, bool)> = vec![];
        let cut = pick_cut_point(
            &track,
            buffer,
            samples_for_ms(SILENCE_SCAN_WINDOW_MS),
            samples_for_ms(MIN_SILENCE_MS),
        );
        assert!(cut.is_none());
    }

    #[test]
    fn pick_cut_point_includes_silence_extending_to_buffer_tail() {
        // Silence starts at 128s and never ends before buffer_ms=130_000.
        let buffer = samples_for_ms(130_000);
        let track = vec![
            (samples_for_ms(127_000), false),
            (samples_for_ms(128_000), true),
            // No further entry — silence extends to buffer tail.
        ];
        let cut = pick_cut_point(
            &track,
            buffer,
            samples_for_ms(SILENCE_SCAN_WINDOW_MS),
            samples_for_ms(MIN_SILENCE_MS),
        )
        .expect("expected a cut");
        let cut_ms = cut / SAMPLES_PER_MS;
        assert_eq!(cut_ms, (128_000 + 130_000) / 2);
    }

    #[test]
    fn chunker_no_vad_emits_forced_chunks_at_target_ms() {
        let (tx, rx) = crossbeam_channel::unbounded::<AudioChunk>();
        let mut ck = Chunker::new(tx, None);

        // Push 250 s of zeros in 1-second slabs.
        let one_sec: Vec<f32> = vec![0.0; SAMPLE_RATE as usize];
        for _ in 0..250 {
            ck.push(&one_sec, None);
        }
        ck.flush();
        drop(ck);

        let chunks: Vec<AudioChunk> = rx.iter().collect();
        // Expect: 2 forced chunks at 120s + 240s, then a final flush
        // residual of 10 s.
        assert_eq!(chunks.len(), 3, "got {} chunks", chunks.len());
        assert_eq!(chunks[0].boundary, BoundaryKind::Forced);
        assert_eq!(chunks[0].start_ms, 0);
        assert_eq!(chunks[0].end_ms, 120_000);
        assert_eq!(chunks[1].boundary, BoundaryKind::Forced);
        assert_eq!(chunks[1].start_ms, 120_000);
        assert_eq!(chunks[1].end_ms, 240_000);
        assert_eq!(chunks[2].boundary, BoundaryKind::Forced);
        assert_eq!(chunks[2].start_ms, 240_000);
        assert_eq!(chunks[2].end_ms, 250_000);
    }

    #[test]
    fn chunker_flush_emits_nothing_when_buffer_empty() {
        let (tx, rx) = crossbeam_channel::unbounded::<AudioChunk>();
        let mut ck = Chunker::new(tx, None);
        ck.flush();
        drop(ck);
        let chunks: Vec<AudioChunk> = rx.iter().collect();
        assert!(chunks.is_empty());
    }
}
