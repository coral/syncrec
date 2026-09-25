//! A recording session: the thread that owns the stream and empties it onto disk.
//!
//! The live pass writes 32-bit float at whatever rate the device actually agreed to
//! run at. It deliberately carries no `bext` chunk, because `bext.TimeReference`
//! cannot be filled in until `t0` has been resolved, which needs a clock reading
//! that arrives after the file has already been opened. Writing a placeholder and
//! patching it later would leave a wrong timestamp in the file for the whole take.
//! Instead the metadata goes on the finished file, and `t0` is flushed to the
//! sidecar the moment it becomes known so a crash does not lose it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use bwavfile::{AudioFrameWriter, WaveWriter};

use super::capture::Capture;
use super::writer::{DriftLog, Sidecar, TakePaths};
use crate::bwf;
use crate::clock::{RefStatus, Reference, TimecodeFormat};
use crate::latency::LatencyCorrection;

/// How long the writer sleeps when there is nothing to do. Short enough that a
/// two-second ring never comes close to filling.
const IDLE_SLEEP: Duration = Duration::from_millis(10);
/// Frames pulled from the ring per pass, per channel.
const DRAIN_FRAMES: usize = 16_384;

pub struct SessionConfig {
    pub paths: TakePaths,
    pub device_name: String,
    /// Platform latency still unaccounted for, plus any manual trim.
    pub latency: LatencyCorrection,
}

/// What a finished take left behind.
pub struct TakeResult {
    pub paths: TakePaths,
    pub sidecar: Sidecar,
    /// What the time reference thought of itself when the take ended.
    ///
    /// Captured here rather than re-read afterwards so that finalising judges the
    /// take against the clock that actually timestamped it, not against whatever
    /// the clock has become by the time the resampler gets round to it.
    pub status: RefStatus,
    /// The frame rate the timecode was carried at. A follower reports what the
    /// leader was actually running, which need not be what this machine was set to.
    pub timecode: Option<TimecodeFormat>,
}

/// Live counters the UI can read without touching the writer thread.
#[derive(Default)]
pub struct Progress {
    frames: AtomicU64,
    overruns: AtomicU64,
    observations: AtomicU64,
}

impl Progress {
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }
    pub fn observations(&self) -> u64 {
        self.observations.load(Ordering::Relaxed)
    }
    /// Recorded duration so far, from the sample count rather than a wall clock.
    pub fn elapsed(&self, rate: u32) -> Duration {
        if rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.frames() as f64 / rate as f64)
    }
}

pub struct Recording {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<TakeResult>>>,
    pub progress: Arc<Progress>,
    pub rate: u32,
    pub channels: u16,
}

impl Recording {
    /// Stop capturing, flush everything still in the ring, and close the files.
    pub fn stop(mut self) -> Result<TakeResult> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .expect("handle is only taken here")
            .join()
            .map_err(|_| anyhow::anyhow!("the writer thread panicked"))?
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // A dropped-without-stop recording still has to release the device.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start capturing. The returned handle owns the stream until it is stopped.
///
/// `reference` is whatever this take is being timestamped against — the SNTP model
/// or an tidkod timeline — and the writer thread owns it outright for the life
/// of the take, because resolving a mark from an tidkod reader mutates it.
pub fn start(
    capture: Capture,
    reference: Box<dyn Reference>,
    config: SessionConfig,
) -> Result<Recording> {
    let rate = capture.negotiated.rate;
    let channels = capture.negotiated.channels;
    let stop = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Progress::default());

    capture.play()?;

    let handle = {
        let stop = Arc::clone(&stop);
        let progress = Arc::clone(&progress);
        thread::Builder::new()
            .name("syncrec-writer".into())
            .spawn(move || run(capture, reference, config, stop, progress))
            .context("spawning the writer thread")?
    };

    Ok(Recording {
        stop,
        handle: Some(handle),
        progress,
        rate,
        channels,
    })
}

fn run(
    mut capture: Capture,
    mut reference: Box<dyn Reference>,
    config: SessionConfig,
    stop: Arc<AtomicBool>,
    progress: Arc<Progress>,
) -> Result<TakeResult> {
    let rate = capture.negotiated.rate;
    let channels = capture.negotiated.channels;

    if let Some(parent) = config.paths.raw.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let writer = WaveWriter::create(&config.paths.raw, bwf::wave_fmt_f32(rate, channels))
        .with_context(|| format!("creating {}", config.paths.raw.display()))?;
    let mut frames = writer
        .audio_frame_writer()
        .context("opening the audio data chunk")?;

    let mut drift = DriftLog::new(capture.bridge, config.latency);
    let mut scratch = vec![0.0f32; DRAIN_FRAMES * channels as usize];
    let mut anchor_flushed = false;
    let mut stream_error: Option<String> = None;
    let mut raw_frames: u64 = 0;

    loop {
        let stopping = stop.load(Ordering::Relaxed);
        // Pause before the final drain so no new audio arrives mid-flush.
        if stopping {
            let _ = capture.pause();
        }

        // One refresh per pass, so every mark in a pass is resolved against the
        // same state rather than against a clock that moved mid-loop.
        reference.refresh();
        while let Ok(mark) = capture.marks.pop() {
            drift.observe(mark, &*reference);
        }
        drift.retry_pending(&*reference);
        progress
            .observations
            .store(drift.observations().len() as u64, Ordering::Relaxed);

        // Flush the anchor as soon as it exists, so a crash mid-take still
        // leaves behind the one fact that cannot be recovered afterwards.
        if !anchor_flushed && drift.t0_unix_nanos().is_some() {
            let partial = build_sidecar(&config, &drift, &*reference, rate, channels, raw_frames, 0);
            if partial.write(&config.paths.sidecar).is_ok() {
                anchor_flushed = true;
            }
        }

        let drained = drain_audio(&mut capture, &mut frames, &mut scratch, channels)?;
        raw_frames += drained;
        progress.frames.store(raw_frames, Ordering::Relaxed);
        progress
            .overruns
            .store(capture.meters.overruns(), Ordering::Relaxed);

        while let Ok(e) = capture.errors.try_recv() {
            stream_error.get_or_insert(e);
        }

        if stopping && drained == 0 && capture.marks.is_empty() {
            break;
        }
        if drained == 0 {
            thread::sleep(IDLE_SLEEP);
        }
    }

    frames.end().context("closing the audio data chunk")?;

    reference.refresh();
    let status = reference.status();
    let timecode = reference.timecode_format();
    let mut sidecar = build_sidecar(
        &config,
        &drift,
        &*reference,
        rate,
        channels,
        raw_frames,
        capture.meters.overruns(),
    );
    sidecar.stream_error = stream_error;
    sidecar.write(&config.paths.sidecar)?;

    Ok(TakeResult {
        paths: config.paths,
        sidecar,
        status,
        timecode,
    })
}

/// Move whatever is in the ring into the file. Returns frames written.
fn drain_audio(
    capture: &mut Capture,
    frames: &mut AudioFrameWriter<std::io::BufWriter<std::fs::File>>,
    scratch: &mut [f32],
    channels: u16,
) -> Result<u64> {
    let ch = channels as usize;
    let available = capture.audio.slots();
    if available < ch {
        return Ok(0);
    }
    // Always pop a whole number of frames. The producer only ever pushes whole
    // buffers, so staying frame-aligned here keeps the channel interleave correct
    // for the life of the stream.
    let take = available.min(scratch.len()) / ch * ch;
    if take == 0 {
        return Ok(0);
    }

    let (filled, _) = capture.audio.pop_partial_slice(&mut scratch[..take]);
    let n = filled.len() / ch * ch;
    if n == 0 {
        return Ok(0);
    }
    frames
        .write_frames(&filled[..n])
        .context("writing audio frames")?;
    Ok((n / ch) as u64)
}

#[allow(clippy::too_many_arguments)]
fn build_sidecar(
    config: &SessionConfig,
    drift: &DriftLog,
    reference: &dyn Reference,
    rate: u32,
    channels: u16,
    raw_frames: u64,
    overruns: u64,
) -> Sidecar {
    let status = reference.status();
    let t0 = drift.t0_unix_nanos();
    Sidecar {
        t0_unix_nanos: t0,
        device_name: config.device_name.clone(),
        device_rate: rate,
        channels,
        raw_frames,
        clock_source: status.source,
        sync_state: status.label,
        clock_dispersion_s: status.dispersion_s,
        clock_slope_ppm: status.slope_ppm,
        clock_samples_accepted: status.samples,
        clock_samples_rejected: status.discarded,
        start_timecode: t0
            .zip(reference.timecode_format())
            .and_then(|(t0, tc)| crate::tidkod::label_at(t0, tc)),
        session_id: reference.session_id(),
        latency_trim_ms: config.latency.trim_ms(),
        overruns,
        marks_abandoned: drift.abandoned_count(),
        observations: drift.observations().to_vec(),
        stream_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_comes_from_the_sample_count() {
        let p = Progress::default();
        p.frames.store(48_000, Ordering::Relaxed);
        assert_eq!(p.elapsed(48_000), Duration::from_secs(1));
        p.frames.store(72_000, Ordering::Relaxed);
        assert_eq!(p.elapsed(48_000), Duration::from_millis(1500));
    }

    #[test]
    fn elapsed_does_not_divide_by_a_zero_rate() {
        let p = Progress::default();
        p.frames.store(48_000, Ordering::Relaxed);
        assert_eq!(p.elapsed(0), Duration::ZERO);
    }
}
