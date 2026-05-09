//! Lightweight one-shot voice recording for the search palette (#57).
//!
//! Captures a short mic clip to a temporary 16-bit PCM 16 kHz mono WAV,
//! emits `voice-level` events for the palette's level meter, and tracks
//! peak amplitude so the caller can decide if the recording was silent.
//!
//! Why not reuse `audio::start()`? That path requires an owned-note
//! bundle, spawns a streaming Whisper worker, and mounts system-audio
//! capture — all wrong for a 5-second voice query. Voice mode collapses
//! the meeting recorder's three-thread pipeline (mic + sysaudio + mixer)
//! into a single thread that handles cpal capture, resampling, WAV
//! writing, and level emission. Reuses the cpal helpers from `audio.rs`
//! (`build_stream`, `downmix`, `MonoResampler`) via `pub(crate)`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use tauri::{AppHandle, Emitter};

use crate::audio::{build_stream, downmix, MonoResampler};

const TARGET_SAMPLE_RATE: u32 = 16_000;

pub enum Cmd {
    Stop,
}

pub struct VoiceRecording {
    ctrl_tx: Sender<Cmd>,
    join: std::thread::JoinHandle<Result<VoiceStopResult, String>>,
}

pub struct VoiceStopResult {
    pub wav_path: PathBuf,
    /// Peak |sample| over the full recording, post-downmix, on the
    /// 0..1 f32 scale. Used by the caller to decide if the recording
    /// was effectively silent before paying for Whisper inference.
    pub max_amplitude: f32,
}

pub fn start(app: AppHandle) -> Result<VoiceRecording, String> {
    let mut wav_path = std::env::temp_dir();
    wav_path.push(format!("margin-voice-{}.wav", uuid::Uuid::new_v4()));

    let (ctrl_tx, ctrl_rx) = unbounded::<Cmd>();
    let voice_app = app.clone();
    let join = std::thread::Builder::new()
        .name("margin-voice".into())
        .spawn(move || run_voice_thread(voice_app, wav_path, ctrl_rx))
        .map_err(|e| e.to_string())?;

    Ok(VoiceRecording { ctrl_tx, join })
}

impl VoiceRecording {
    pub fn stop(self) -> Result<VoiceStopResult, String> {
        let _ = self.ctrl_tx.send(Cmd::Stop);
        match self.join.join() {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("voice thread panicked".to_string()),
        }
    }
}

fn run_voice_thread(
    app: AppHandle,
    wav_path: PathBuf,
    ctrl_rx: Receiver<Cmd>,
) -> Result<VoiceStopResult, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| {
            "No input device available — check microphone access in System Settings."
                .to_string()
        })?;
    let cfg = device.default_input_config().map_err(|e| e.to_string())?;
    let sample_rate: u32 = cfg.sample_rate().into();
    let channels = cfg.channels() as usize;
    let fmt = cfg.sample_format();
    let stream_cfg: cpal::StreamConfig = cfg.into();

    let device_name = device
        .name()
        .unwrap_or_else(|_| "<unknown>".to_string());
    eprintln!(
        "[voice] mic: {} | {} Hz | {} ch | {:?}",
        device_name, sample_rate, channels, fmt
    );

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(&wav_path, spec).map_err(|e| e.to_string())?;

    // cpal callbacks → raw frames. Bounded so a slow consumer can drop
    // overflow rather than blocking the audio thread (matches audio.rs).
    let (raw_tx, raw_rx) = bounded::<Vec<f32>>(64);
    let stream = build_stream(&device, &stream_cfg, fmt, raw_tx)?;
    stream.play().map_err(|e| e.to_string())?;

    let mut shaper = MonoResampler::new(sample_rate, channels)?;
    let mut last_emit = Instant::now();
    let mut max_amplitude: f32 = 0.0;
    let mut frame_count: u64 = 0;

    loop {
        crossbeam_channel::select! {
            recv(raw_rx) -> msg => {
                match msg {
                    Ok(buf) => {
                        let mono = downmix(&buf, channels);

                        // Peak detector for silence detection. Run on
                        // the pre-resample mono buffer so the threshold
                        // doesn't drift with sample-rate conversion.
                        frame_count += mono.len() as u64;
                        for &s in &mono {
                            let a = s.abs();
                            if a > max_amplitude {
                                max_amplitude = a;
                            }
                        }

                        // ~30 Hz level meter, same shape as audio.rs's
                        // `audio-level` event so LevelMeter.tsx can
                        // consume it via its `eventName` prop.
                        if last_emit.elapsed() >= Duration::from_millis(33) {
                            let n = mono.len().max(1);
                            let rms =
                                (mono.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
                            let _ = app.emit("voice-level", rms);
                            last_emit = Instant::now();
                        }

                        for chunk in shaper.process(mono)? {
                            for &s in &chunk {
                                let i = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                                wav.write_sample(i).map_err(|e| e.to_string())?;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            recv(ctrl_rx) -> _ => break,
        }
    }

    drop(stream); // drop the !Send cpal::Stream on its owning thread
    wav.finalize().map_err(|e| e.to_string())?;

    eprintln!(
        "[voice] stop: frames={frame_count} (~{ms}ms @ {sample_rate} Hz), max_amplitude={max_amplitude:.4}",
        ms = frame_count * 1000 / sample_rate as u64,
    );

    Ok(VoiceStopResult {
        wav_path,
        max_amplitude,
    })
}
