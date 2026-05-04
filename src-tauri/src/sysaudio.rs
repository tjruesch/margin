//! macOS system-audio capture via ScreenCaptureKit.
//!
//! Captures whatever's playing through the system output (Zoom call audio,
//! browser meetings, video calls) at 16 kHz mono f32 PCM — directly in our
//! target format, so no resampling needed for this leg.
//!
//! Requires the user to grant Screen Recording permission on first use.
//! `with_excludes_current_process_audio(true)` keeps Margin's own UI sounds
//! (and any audio it ever plays back) out of the mix.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use screencapturekit::cm::CMSampleBuffer;
use screencapturekit::shareable_content::SCShareableContent;
use screencapturekit::stream::{
    configuration::SCStreamConfiguration, content_filter::SCContentFilter,
    output_trait::SCStreamOutputTrait, output_type::SCStreamOutputType, sc_stream::SCStream,
};
use tauri::{AppHandle, Emitter};

pub enum Cmd {
    Stop,
}

/// f32 LE bytes → Vec<f32>.
fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for c in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    out
}

struct AudioHandler {
    tx: Sender<Vec<f32>>,
    app: AppHandle,
    last_emit: Mutex<Instant>,
}

impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(
        &self,
        sample: CMSampleBuffer,
        of_type: SCStreamOutputType,
    ) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }
        let Some(list) = sample.audio_buffer_list() else {
            return;
        };

        // Configured for mono → expect a single buffer of interleaved (or
        // really just sequential, since 1 ch) f32 samples. Defensive: handle
        // multi-buffer planar too in case macOS ever gives us stereo despite
        // the request.
        if list.num_buffers() == 0 {
            return;
        }
        let samples: Vec<f32> = if list.num_buffers() == 1 {
            // Single buffer, native order.
            let Some(buf) = list.get(0) else { return };
            bytes_to_f32(buf.data())
        } else {
            // Planar fallback — average channels into mono.
            let chans: Vec<Vec<f32>> = list
                .iter()
                .map(|b| bytes_to_f32(b.data()))
                .collect();
            if chans.is_empty() {
                return;
            }
            let frames = chans.iter().map(|c| c.len()).min().unwrap_or(0);
            let n_chans = chans.len() as f32;
            let mut out = Vec::with_capacity(frames);
            for f in 0..frames {
                let mut sum = 0.0f32;
                for ch in &chans {
                    sum += ch[f];
                }
                out.push(sum / n_chans);
            }
            out
        };

        // ~30 Hz level emission, mirroring the mic side. RMS lets the JS
        // meter use the same `* 2.2` scaling for both bars.
        if let Ok(mut last) = self.last_emit.lock() {
            if last.elapsed() >= Duration::from_millis(33) && !samples.is_empty() {
                let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32)
                    .sqrt();
                let _ = self.app.emit("sysaudio-level", rms);
                *last = Instant::now();
            }
        }

        let _ = self.tx.try_send(samples);
    }
}

/// Spawn a dedicated thread that owns an `SCStream` for system-audio capture.
/// Emits 16 kHz mono f32 chunks on `tx`. Stop by sending `Cmd::Stop` on `ctrl_rx`.
///
/// Returns the join handle, or an error if SCK initialization or
/// `start_capture()` fails (typically because the user denied screen-recording
/// permission). The caller should fall back to mic-only on error.
pub fn spawn(
    app: AppHandle,
    tx: Sender<Vec<f32>>,
    ctrl_rx: Receiver<Cmd>,
) -> Result<std::thread::JoinHandle<Result<(), String>>, String> {
    // Channel used to surface init failures from inside the thread to the
    // caller — SCStream is `!Send` so we have to construct it on the worker
    // thread, but we need to know if `start_capture()` failed before we
    // return a handle to a thread that's already exited.
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let join = std::thread::Builder::new()
        .name("margin-sysaudio".into())
        .spawn(move || -> Result<(), String> {
            // Build a content filter that just selects the primary display.
            // We don't actually want video, but SCK requires a video target;
            // we suppress video by configuring 2x2 pixels and never adding a
            // video output handler.
            let content = match SCShareableContent::get() {
                Ok(c) => c,
                Err(e) => {
                    let _ = init_tx.send(Err(format!("SCShareableContent: {e:?}")));
                    return Err(format!("SCShareableContent: {e:?}"));
                }
            };
            let displays = content.displays();
            let Some(display) = displays.first() else {
                let _ = init_tx.send(Err("no display available".into()));
                return Err("no display available".into());
            };

            let filter = SCContentFilter::create()
                .with_display(display)
                .with_excluding_windows(&[])
                .build();

            let cfg = SCStreamConfiguration::new()
                .with_width(2)
                .with_height(2)
                .with_captures_audio(true)
                .with_sample_rate(16_000)
                .with_channel_count(1)
                .with_excludes_current_process_audio(true);

            let mut stream = SCStream::new(&filter, &cfg);
            stream.add_output_handler(
                AudioHandler {
                    tx,
                    app,
                    last_emit: Mutex::new(Instant::now()),
                },
                SCStreamOutputType::Audio,
            );

            if let Err(e) = stream.start_capture() {
                let msg = format!("SCK start_capture: {e:?}");
                let _ = init_tx.send(Err(msg.clone()));
                return Err(msg);
            }
            let _ = init_tx.send(Ok(()));

            // Block on stop signal.
            match ctrl_rx.recv() {
                Ok(Cmd::Stop) | Err(_) => {}
            }

            // stop_capture must be called from this thread (SCStream is !Send).
            if let Err(e) = stream.stop_capture() {
                eprintln!("[sysaudio] stop_capture err: {e:?}");
            }
            drop(stream);
            Ok(())
        })
        .map_err(|e| e.to_string())?;

    // Wait for init to succeed or fail before returning.
    match init_rx.recv() {
        Ok(Ok(())) => Ok(join),
        Ok(Err(msg)) => {
            let _ = join.join();
            Err(msg)
        }
        Err(_) => Err("sysaudio thread exited before reporting init result".into()),
    }
}
