//! Audio capture pipeline.
//!
//! Three threads, optional second leg:
//!
//! ```
//!  ┌──────────────────┐                ┌────────────────────┐
//!  │ mic (cpal)       │──╮          ╭──│ sysaudio (SCK)     │
//!  │ thread           │  │          │  │ thread (optional)  │
//!  └──────────────────┘  │          │  └────────────────────┘
//!  emits 16k mono via    │          │  emits 16k mono native
//!  rubato resample       ▼          ▼  (SCK delivers our rate)
//!                  ╭──────────────────────╮
//!                  │  mixer / writer      │
//!                  │  thread              │
//!                  ╰──────────────────────╯
//!                          │
//!                          ▼
//!                       <id>.wav
//! ```
//!
//! Mic is always on. System audio is gated by `with_system_audio`. When
//! enabled, the mixer pulls from both rings, sums per-sample with light
//! attenuation to avoid clipping, and writes 16-bit PCM 16 kHz mono via
//! hound. When disabled, the mixer just passes mic through.
//!
//! The mic-side level meter (`audio-level` event) is unchanged — it
//! reflects the user's voice, not the mix, which is what the future
//! Meeting UI level meter wants.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use rubato::{FastFixedIn, PolynomialDegree, Resampler};
use tauri::{AppHandle, Emitter};

use crate::{paths, sysaudio};

const TARGET_SAMPLE_RATE: u32 = 16_000;
const RESAMPLE_CHUNK: usize = 1024; // mic-side input frames per resampler step
const MIX_GAIN: f32 = 0.7; // each leg's weight before sum/clamp
const MAX_RING_LAG_SAMPLES: usize = 16_000; // 1 sec drift tolerance

pub enum Cmd {
    Stop,
}

pub struct Recording {
    pub id: String,
    pub wav_path: PathBuf,
    mic_ctrl_tx: Sender<Cmd>,
    sys_ctrl_tx: Option<Sender<sysaudio::Cmd>>,
    mic_join: std::thread::JoinHandle<Result<(), String>>,
    sys_join: Option<std::thread::JoinHandle<Result<(), String>>>,
    mix_join: std::thread::JoinHandle<Result<(), String>>,
}

impl Recording {
    /// Stop all threads, finalize the WAV, return.
    pub fn stop(self) -> Result<PathBuf, String> {
        // Signal mic and sys threads to drop their streams.
        let _ = self.mic_ctrl_tx.send(Cmd::Stop);
        if let Some(tx) = &self.sys_ctrl_tx {
            let _ = tx.send(sysaudio::Cmd::Stop);
        }
        // Joining mic + sys closes their frame senders, which lets the mixer
        // drain remaining samples and finalize the WAV.
        self.mic_join
            .join()
            .map_err(|_| "mic thread panicked".to_string())??;
        if let Some(j) = self.sys_join {
            j.join()
                .map_err(|_| "sysaudio thread panicked".to_string())??;
        }
        self.mix_join
            .join()
            .map_err(|_| "mixer thread panicked".to_string())??;
        Ok(self.wav_path)
    }
}

pub fn start(
    app: AppHandle,
    _title: String,
    with_system_audio: bool,
) -> Result<Recording, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let wav_path = paths::meetings_dir().join(format!("{id}.wav"));

    // Channels carry 16 kHz mono f32 chunks from each source to the mixer.
    let (mic_tx, mic_rx) = bounded::<Vec<f32>>(64);
    let (sys_tx, sys_rx) = bounded::<Vec<f32>>(64);

    // Mic thread control.
    let (mic_ctrl_tx, mic_ctrl_rx) = unbounded::<Cmd>();

    // Mic capture thread (always on).
    let mic_app = app.clone();
    let mic_join = std::thread::Builder::new()
        .name("margin-audio-mic".into())
        .spawn(move || run_mic_thread(mic_app, mic_tx, mic_ctrl_rx))
        .map_err(|e| e.to_string())?;

    // System audio thread + control (optional, may fail to start on permission deny).
    let (sys_ctrl_tx, sys_join) = if with_system_audio {
        let (tx, rx) = unbounded::<sysaudio::Cmd>();
        match sysaudio::spawn(sys_tx.clone(), rx) {
            Ok(handle) => (Some(tx), Some(handle)),
            Err(e) => {
                eprintln!("[audio] system audio unavailable, falling back to mic-only: {e}");
                let _ = app.emit("sysaudio-unavailable", e);
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    // The mixer's sys_rx is only meaningful when SCK is actually running.
    // Drop the unused sender so the channel closes; then the mixer tracks
    // a `Some(rx)` pattern based on whether SCK is live.
    let mixer_sys_rx = if sys_join.is_some() { Some(sys_rx) } else { None };
    drop(sys_tx);

    let mix_app = app.clone();
    let wav_for_mixer = wav_path.clone();
    let mix_join = std::thread::Builder::new()
        .name("margin-audio-mixer".into())
        .spawn(move || run_mixer_thread(mix_app, &wav_for_mixer, mic_rx, mixer_sys_rx))
        .map_err(|e| e.to_string())?;

    Ok(Recording {
        id,
        wav_path,
        mic_ctrl_tx,
        sys_ctrl_tx,
        mic_join,
        sys_join,
        mix_join,
    })
}

// ---------- mic capture thread -------------------------------------------

fn run_mic_thread(
    app: AppHandle,
    out_tx: Sender<Vec<f32>>,
    ctrl_rx: Receiver<Cmd>,
) -> Result<(), String> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or("no input device")?;
    let cfg = device.default_input_config().map_err(|e| e.to_string())?;
    let sample_rate: u32 = cfg.sample_rate().into();
    let channels = cfg.channels() as usize;
    let fmt = cfg.sample_format();
    let stream_cfg: cpal::StreamConfig = cfg.into();

    eprintln!(
        "[audio] mic: {} Hz | {} ch | {:?}",
        sample_rate, channels, fmt
    );

    // cpal callbacks deliver raw frames at device rate. We resample + downmix
    // here so the mixer only deals with 16 kHz mono.
    let (raw_tx, raw_rx) = bounded::<Vec<f32>>(64);
    let stream = build_stream(&device, &stream_cfg, fmt, raw_tx)?;
    stream.play().map_err(|e| e.to_string())?;

    let mut shaper = MonoResampler::new(sample_rate, channels)?;
    let mut last_emit = Instant::now();

    loop {
        crossbeam_channel::select! {
            recv(raw_rx) -> msg => {
                match msg {
                    Ok(buf) => {
                        let mono = downmix(&buf, channels);
                        // ~30 Hz level meter on the pre-resample mono buffer.
                        if last_emit.elapsed() >= Duration::from_millis(33) {
                            let n = mono.len().max(1);
                            let rms = (mono.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
                            let _ = app.emit("audio-level", rms);
                            last_emit = Instant::now();
                        }
                        for chunk in shaper.process(mono)? {
                            let _ = out_tx.try_send(chunk);
                        }
                    }
                    Err(_) => break,
                }
            }
            recv(ctrl_rx) -> _ => break,
        }
    }

    drop(stream); // owning thread drops the !Send cpal::Stream
    drop(out_tx); // close mixer's mic_rx so it can finalize
    Ok(())
}

fn build_stream(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    fmt: cpal::SampleFormat,
    raw_tx: Sender<Vec<f32>>,
) -> Result<cpal::Stream, String> {
    let err_cb = |e: cpal::StreamError| eprintln!("[audio] stream err: {e}");
    match fmt {
        cpal::SampleFormat::F32 => {
            let tx = raw_tx;
            device
                .build_input_stream(
                    cfg,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        let _ = tx.try_send(data.to_vec());
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| e.to_string())
        }
        cpal::SampleFormat::I16 => {
            let tx = raw_tx;
            device
                .build_input_stream(
                    cfg,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let v: Vec<f32> = data.iter().map(|s| *s as f32 / 32768.0).collect();
                        let _ = tx.try_send(v);
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| e.to_string())
        }
        cpal::SampleFormat::U16 => {
            let tx = raw_tx;
            device
                .build_input_stream(
                    cfg,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        let v: Vec<f32> = data
                            .iter()
                            .map(|s| (*s as f32 - 32768.0) / 32768.0)
                            .collect();
                        let _ = tx.try_send(v);
                    },
                    err_cb,
                    None,
                )
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unsupported sample format: {other:?}")),
    }
}

fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        interleaved.to_vec()
    } else {
        interleaved
            .chunks_exact(channels)
            .map(|fr| fr.iter().sum::<f32>() / channels as f32)
            .collect()
    }
}

/// Buffers mono samples at the source rate and emits 16 kHz mono chunks.
struct MonoResampler {
    resampler: Option<FastFixedIn<f32>>,
    chunk: usize,
    leftover: Vec<f32>,
}

impl MonoResampler {
    fn new(src_rate: u32, _channels: usize) -> Result<Self, String> {
        let resampler = if src_rate == TARGET_SAMPLE_RATE {
            None
        } else {
            let ratio = TARGET_SAMPLE_RATE as f64 / src_rate as f64;
            let r = FastFixedIn::<f32>::new(
                ratio,
                1.0,
                PolynomialDegree::Septic,
                RESAMPLE_CHUNK,
                1,
            )
            .map_err(|e| e.to_string())?;
            Some(r)
        };
        Ok(Self {
            resampler,
            chunk: RESAMPLE_CHUNK,
            leftover: Vec::with_capacity(RESAMPLE_CHUNK * 4),
        })
    }

    fn process(&mut self, mono: Vec<f32>) -> Result<Vec<Vec<f32>>, String> {
        self.leftover.extend_from_slice(&mono);
        let mut out_chunks = Vec::new();
        while self.leftover.len() >= self.chunk {
            let input: Vec<f32> = self.leftover.drain(..self.chunk).collect();
            let processed: Vec<f32> = if let Some(r) = self.resampler.as_mut() {
                r.process(&[input], None)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .next()
                    .unwrap_or_default()
            } else {
                input
            };
            if !processed.is_empty() {
                out_chunks.push(processed);
            }
        }
        Ok(out_chunks)
    }
}

// ---------- mixer / writer thread ----------------------------------------

fn run_mixer_thread(
    _app: AppHandle,
    wav_path: &PathBuf,
    mic_rx: Receiver<Vec<f32>>,
    sys_rx: Option<Receiver<Vec<f32>>>,
    ) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: TARGET_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut wav = hound::WavWriter::create(wav_path, spec).map_err(|e| e.to_string())?;

    let mut mic_buf = VecDeque::<f32>::new();
    let mut sys_buf = VecDeque::<f32>::new();
    let mut mic_open = true;
    let mut sys_open = sys_rx.is_some();

    while mic_open || (sys_open && !mic_buf.is_empty()) {
        // Block on whichever leg has work or is alive.
        if let Some(srx) = sys_rx.as_ref() {
            if mic_open && sys_open {
                crossbeam_channel::select! {
                    recv(mic_rx) -> r => match r {
                        Ok(b) => mic_buf.extend(b),
                        Err(_) => mic_open = false,
                    },
                    recv(srx) -> r => match r {
                        Ok(b) => sys_buf.extend(b),
                        Err(_) => sys_open = false,
                    },
                }
            } else if mic_open {
                match mic_rx.recv() {
                    Ok(b) => mic_buf.extend(b),
                    Err(_) => mic_open = false,
                }
            } else if sys_open {
                match srx.recv() {
                    Ok(b) => sys_buf.extend(b),
                    Err(_) => sys_open = false,
                }
            } else {
                break;
            }
        } else {
            match mic_rx.recv() {
                Ok(b) => mic_buf.extend(b),
                Err(_) => mic_open = false,
            }
        }

        // Drain what we can mix. With both legs we drain min(both); with mic
        // only we drain everything mic has.
        let drain_n = if sys_rx.is_some() {
            mic_buf.len().min(sys_buf.len())
        } else {
            mic_buf.len()
        };

        for _ in 0..drain_n {
            let m = mic_buf.pop_front().unwrap_or(0.0);
            let mixed = if sys_rx.is_some() {
                let s = sys_buf.pop_front().unwrap_or(0.0);
                (MIX_GAIN * m + MIX_GAIN * s).clamp(-1.0, 1.0)
            } else {
                m.clamp(-1.0, 1.0)
            };
            let v = (mixed * 32767.0) as i16;
            wav.write_sample(v).map_err(|e| e.to_string())?;
        }

        // Drift handling: drop the front of any ring that's lagged > 1 sec
        // ahead of its partner. Acceptable for transcription; logs once.
        if sys_rx.is_some() {
            if mic_buf.len() > MAX_RING_LAG_SAMPLES {
                let drop_n = mic_buf.len() - MAX_RING_LAG_SAMPLES;
                eprintln!("[audio] dropping {drop_n} mic samples (sys leg behind)");
                mic_buf.drain(..drop_n);
            }
            if sys_buf.len() > MAX_RING_LAG_SAMPLES {
                let drop_n = sys_buf.len() - MAX_RING_LAG_SAMPLES;
                eprintln!("[audio] dropping {drop_n} sys samples (mic leg behind)");
                sys_buf.drain(..drop_n);
            }
        }
    }

    // Flush whatever mic samples remained after one or both senders closed.
    for &m in mic_buf.iter() {
        let v = (m.clamp(-1.0, 1.0) * 32767.0) as i16;
        wav.write_sample(v).map_err(|e| e.to_string())?;
    }

    wav.finalize().map_err(|e| e.to_string())
}
