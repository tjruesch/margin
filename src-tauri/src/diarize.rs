//! Speaker diarization via sherpa-onnx.
//!
//! v1 batch pipeline: load full 16 kHz mono PCM, run pyannote segmentation +
//! NeMo titanet embedding + fast clustering in one call. Returns a list of
//! `DiarSpan` (timestamp + speaker index).
//!
//! Designed to be chunk-future-compatible. The chunked PR will replace the
//! one-shot `diarize` call with per-chunk `SpeakerEmbeddingExtractor` + an
//! end-of-meeting clustering pass — but `assign_speakers` is unchanged
//! across both worlds because it's a pure overlap match between two
//! independent timestamp sources.
//!
//! Models live under `~/.margin/models/diarize/` and are downloaded on
//! first use, mirroring the Whisper `ensure_model` pattern.
//!
//! - segmentation: pyannote-segmentation-3.0 (~6 MB)
//! - embedding:    nemo_en_titanet_small (~40 MB)

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sherpa_onnx::{
    FastClusteringConfig, OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    SpeakerEmbeddingExtractorConfig,
};
use tauri::{AppHandle, Emitter};

use crate::paths;
use crate::transcribe::Segment;

const SEGMENTATION_FILENAME: &str = "pyannote-segmentation-3.0.onnx";
const SEGMENTATION_URL: &str =
    "https://huggingface.co/csukuangfj/sherpa-onnx-pyannote-segmentation-3-0/resolve/main/model.onnx";

const EMBEDDING_FILENAME: &str = "nemo_en_titanet_small.onnx";
const EMBEDDING_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/nemo_en_titanet_small.onnx";

/// One diarization span: a contiguous stretch of audio attributed to a single
/// speaker by the clustering step. Timestamps are in milliseconds, matching
/// `transcribe::Segment`.
#[derive(Clone, Debug)]
pub struct DiarSpan {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: u32,
}

pub struct DiarPaths {
    pub segmentation: PathBuf,
    pub embedding: PathBuf,
}

/// Run the full pipeline on a 16 kHz mono PCM buffer. The buffer is the same
/// `pcm_f32` already loaded by transcribe.rs for Whisper — no second WAV read.
pub fn diarize(paths: &DiarPaths, samples: &[f32]) -> Result<Vec<DiarSpan>, String> {
    let cfg = OfflineSpeakerDiarizationConfig {
        segmentation: OfflineSpeakerSegmentationModelConfig {
            pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                model: Some(paths.segmentation.to_string_lossy().into_owned()),
            },
            ..Default::default()
        },
        embedding: SpeakerEmbeddingExtractorConfig {
            model: Some(paths.embedding.to_string_lossy().into_owned()),
            ..Default::default()
        },
        clustering: FastClusteringConfig {
            // -1 = auto-detect from embedding similarity; threshold gates merging.
            num_clusters: -1,
            threshold: 0.5,
        },
        ..Default::default()
    };

    let diar = OfflineSpeakerDiarization::create(&cfg)
        .ok_or_else(|| "sherpa-onnx: OfflineSpeakerDiarization::create failed".to_string())?;
    let result = diar
        .process(samples)
        .ok_or_else(|| "sherpa-onnx: OfflineSpeakerDiarization::process failed".to_string())?;

    let segs = result.sort_by_start_time();
    let spans = segs
        .into_iter()
        .filter(|s| s.speaker >= 0 && s.end > s.start)
        .map(|s| DiarSpan {
            start_ms: (s.start * 1000.0).round() as u64,
            end_ms: (s.end * 1000.0).round() as u64,
            speaker: s.speaker as u32,
        })
        .collect();
    Ok(spans)
}

/// For each transcript segment, attach the speaker of the diar span with the
/// largest temporal overlap. Pure: no shared state, so the chunked-future PR
/// can call this once at end-of-meeting with merged spans across chunks.
pub fn assign_speakers(segments: &mut [Segment], spans: &[DiarSpan]) {
    for seg in segments.iter_mut() {
        let mut best: Option<(u64, u32)> = None;
        for span in spans {
            let overlap = overlap_ms(seg.start_ms, seg.end_ms, span.start_ms, span.end_ms);
            if overlap == 0 {
                continue;
            }
            match best {
                Some((cur, _)) if cur >= overlap => {}
                _ => best = Some((overlap, span.speaker)),
            }
        }
        seg.speaker = best.map(|(_, sp)| sp);
    }
}

/// Count unique non-`None` speaker values across segments.
pub fn count_unique_speakers(segments: &[Segment]) -> u32 {
    let mut set = HashSet::new();
    for seg in segments {
        if let Some(sp) = seg.speaker {
            set.insert(sp);
        }
    }
    set.len() as u32
}

fn overlap_ms(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> u64 {
    let lo = a_start.max(b_start);
    let hi = a_end.min(b_end);
    if hi > lo {
        hi - lo
    } else {
        0
    }
}

/// Ensure both ONNX models are present locally. Downloads on first use with
/// progress events on `model-download-progress` (same channel as Whisper —
/// existing UI surfaces it without changes).
pub async fn ensure_diarization_models(app: &AppHandle) -> Result<DiarPaths, String> {
    let dir = paths::models_dir().join("diarize");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("create diarize dir: {e}"))?;

    let segmentation = dir.join(SEGMENTATION_FILENAME);
    let embedding = dir.join(EMBEDDING_FILENAME);

    if !segmentation.exists() {
        download(app, SEGMENTATION_URL, &segmentation).await?;
    }
    if !embedding.exists() {
        download(app, EMBEDDING_URL, &embedding).await?;
    }

    Ok(DiarPaths {
        segmentation,
        embedding,
    })
}

async fn download(app: &AppHandle, url: &str, target: &Path) -> Result<(), String> {
    let resp = reqwest::get(url)
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
    tokio::fs::rename(&tmp, target)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}
