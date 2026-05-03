use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use rubato::{FastFixedIn, PolynomialDegree, Resampler};
use tauri::{AppHandle, Emitter};

use crate::paths;

const TARGET_SAMPLE_RATE: u32 = 16_000;
const RESAMPLE_CHUNK: usize = 1024; // input frames per resampler step

pub enum Cmd {
    Stop,
}

pub struct Recording {
    pub id: String,
    pub wav_path: PathBuf,
    pub ctrl_tx: Sender<Cmd>,
    pub join: std::thread::JoinHandle<Result<(), String>>,
}

pub fn start(app: AppHandle, _title: String) -> Result<Recording, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let wav_path = paths::meetings_dir().join(format!("{id}.wav"));

    let (ctrl_tx, ctrl_rx) = unbounded::<Cmd>();
    let (frame_tx, frame_rx) = bounded::<Vec<f32>>(64);
    let wav_for_thread = wav_path.clone();
    let app_for_thread = app.clone();

    let join = std::thread::Builder::new()
        .name("margin-audio".into())
        .spawn(move || -> Result<(), String> {
            run_recording_thread(app_for_thread, &wav_for_thread, frame_tx, frame_rx, ctrl_rx)
        })
        .map_err(|e| e.to_string())?;

    Ok(Recording {
        id,
        wav_path,
        ctrl_tx,
        join,
    })
}

fn run_recording_thread(
    app: AppHandle,
    wav_path: &PathBuf,
    frame_tx: Sender<Vec<f32>>,
    frame_rx: Receiver<Vec<f32>>,
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
        "[audio] {} Hz | {} ch | {:?}",
        sample_rate, channels, fmt
    );

    let stream = build_stream(&device, &stream_cfg, fmt, frame_tx)?;
    stream.play().map_err(|e| e.to_string())?;

    let mut pipeline = WritePipeline::new(wav_path, sample_rate, channels, app)?;

    loop {
        crossbeam_channel::select! {
            recv(frame_rx) -> msg => {
                match msg {
                    Ok(buf) => pipeline.push(buf)?,
                    Err(_) => break, // sender dropped
                }
            }
            recv(ctrl_rx) -> _ => break,
        }
    }

    drop(stream);
    pipeline.finalize()
}

fn build_stream(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    fmt: cpal::SampleFormat,
    frame_tx: Sender<Vec<f32>>,
) -> Result<cpal::Stream, String> {
    let err_cb = |e: cpal::StreamError| eprintln!("[audio] stream err: {e}");
    match fmt {
        cpal::SampleFormat::F32 => {
            let tx = frame_tx;
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
            let tx = frame_tx;
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
            let tx = frame_tx;
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

struct WritePipeline {
    wav: Option<hound::WavWriter<BufWriter<File>>>,
    resampler: Option<FastFixedIn<f32>>,
    channels: usize,
    chunk: usize,
    leftover: Vec<f32>,
    app: AppHandle,
    last_emit: Instant,
}

impl WritePipeline {
    fn new(
        path: &PathBuf,
        src_rate: u32,
        channels: usize,
        app: AppHandle,
    ) -> Result<Self, String> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let wav = hound::WavWriter::create(path, spec).map_err(|e| e.to_string())?;

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
            wav: Some(wav),
            resampler,
            channels,
            chunk: RESAMPLE_CHUNK,
            leftover: Vec::with_capacity(RESAMPLE_CHUNK * 4),
            app,
            last_emit: Instant::now(),
        })
    }

    fn push(&mut self, interleaved: Vec<f32>) -> Result<(), String> {
        // 1. downmix to mono
        let mono: Vec<f32> = if self.channels == 1 {
            interleaved
        } else {
            interleaved
                .chunks_exact(self.channels)
                .map(|fr| fr.iter().sum::<f32>() / self.channels as f32)
                .collect()
        };

        // 2. ~30 Hz level meter
        if self.last_emit.elapsed() >= Duration::from_millis(33) {
            let n = mono.len().max(1);
            let rms = (mono.iter().map(|s| s * s).sum::<f32>() / n as f32).sqrt();
            let _ = self.app.emit("audio-level", rms);
            self.last_emit = Instant::now();
        }

        // 3. resample in fixed-size chunks (or pass through if rates match)
        self.leftover.extend_from_slice(&mono);
        let writer = self.wav.as_mut().ok_or("wav writer dropped")?;

        while self.leftover.len() >= self.chunk {
            let input: Vec<f32> = self.leftover.drain(..self.chunk).collect();
            let out: Vec<f32> = if let Some(r) = self.resampler.as_mut() {
                let processed = r.process(&[input], None).map_err(|e| e.to_string())?;
                processed.into_iter().next().unwrap_or_default()
            } else {
                input
            };
            for s in out {
                let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                writer.write_sample(v).map_err(|e| e.to_string())?;
            }
        }

        Ok(())
    }

    fn finalize(mut self) -> Result<(), String> {
        if let Some(w) = self.wav.take() {
            w.finalize().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
