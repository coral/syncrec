//! The realtime capture path.
//!
//! The audio callback is the one place in this program that must never block, never
//! allocate and never touch a lock. It does three things: convert the device's
//! samples to `f32`, hand them to a lock-free ring buffer, and fold them into the
//! meters. It also drops a timestamped mark roughly once a second so the writer can
//! later work out the device's true sample rate.
//!
//! Note what it does *not* do: read the wall clock. Per the design, we anchor once
//! and then count samples. Reading the clock per callback would inject the very
//! scheduling jitter we went to the trouble of avoiding.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, FromSample, InputCallbackInfo, Sample, SampleFormat, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};

use super::Negotiated;
use super::meters::Meters;
use crate::clock::bridge::Bridge;

/// How much audio the ring buffer can hold before the writer has to have drained it.
const RING_SECONDS: f64 = 2.0;
/// Time marks are tiny and drained at least once a second; this is generous.
const MARK_QUEUE_LEN: usize = 512;
/// Roughly one mark per second.
const MARK_INTERVAL_SECONDS: f64 = 1.0;
/// Fallback scratch size when the device does not report a maximum buffer size.
const DEFAULT_MAX_FRAMES: usize = 8192;
/// How long to wait for the backend to bring the stream up.
const BUILD_TIMEOUT: Duration = Duration::from_secs(5);

/// A device timestamp pinned to a sample index.
///
/// `frame_index` counts frames captured *before* this buffer, and `capture_nanos` is
/// cpal's estimate of when that frame hit the converter — already latency-corrected
/// on CoreAudio, corrected by us on WASAPI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeMark {
    pub frame_index: u64,
    pub capture_nanos: u128,
}

/// A running capture. Dropping this stops the stream.
pub struct Capture {
    stream: cpal::Stream,
    pub audio: Consumer<f32>,
    pub marks: Consumer<TimeMark>,
    pub meters: Arc<Meters>,
    pub errors: Receiver<String>,
    /// Correlates the callback's `StreamInstant` with `std::time::Instant`.
    pub bridge: Bridge,
    pub negotiated: Negotiated,
}

impl Capture {
    pub fn play(&self) -> Result<()> {
        self.stream.play().context("starting the input stream")?;
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        self.stream.pause().context("pausing the input stream")?;
        Ok(())
    }
}

/// Build (but do not start) an input stream for `device`.
pub fn build(device: &Device, negotiated: &Negotiated) -> Result<Capture> {
    let channels = negotiated.channels as usize;
    if channels == 0 {
        return Err(anyhow!("device reports zero input channels"));
    }

    let ring_len = ((negotiated.rate as f64 * RING_SECONDS) as usize).max(1) * channels;
    let (audio_tx, audio_rx) = RingBuffer::<f32>::new(ring_len);
    let (mark_tx, mark_rx) = RingBuffer::<TimeMark>::new(MARK_QUEUE_LEN);
    let (err_tx, err_rx) = channel();

    let meters = Arc::new(Meters::new(channels, negotiated.rate));
    let mark_every = (negotiated.rate as f64 * MARK_INTERVAL_SECONDS) as u64;

    // Size the conversion scratch from what the device says it may hand us, so the
    // callback never has to grow it.
    let max_frames = match negotiated.config.buffer_size {
        cpal::BufferSize::Fixed(n) => n as usize,
        cpal::BufferSize::Default => DEFAULT_MAX_FRAMES,
    };

    let ctx = CallbackCtx {
        channels,
        mark_every,
        producer: audio_tx,
        marks: mark_tx,
        meters: Arc::clone(&meters),
        frames_seen: 0,
        next_mark_at: 0,
        scratch: Vec::with_capacity(max_frames * channels),
    };

    let stream = build_for_format(device, negotiated, ctx, err_tx)?;
    let bridge = Bridge::measure(&stream);

    Ok(Capture {
        stream,
        audio: audio_rx,
        marks: mark_rx,
        meters,
        errors: err_rx,
        bridge,
        negotiated: negotiated.clone(),
    })
}

/// Everything the callback owns. Built once, mutated in place, never reallocated.
struct CallbackCtx {
    channels: usize,
    mark_every: u64,
    producer: Producer<f32>,
    marks: Producer<TimeMark>,
    meters: Arc<Meters>,
    frames_seen: u64,
    next_mark_at: u64,
    scratch: Vec<f32>,
}

impl CallbackCtx {
    /// The realtime callback body, shared by every sample format.
    fn on_data<T>(&mut self, data: &[T], info: &InputCallbackInfo)
    where
        T: Sample,
        f32: FromSample<T>,
    {
        if self.channels == 0 {
            return;
        }
        let frames = (data.len() / self.channels) as u64;
        if frames == 0 {
            return;
        }

        // Capacity was reserved at build time, so this does not allocate.
        self.scratch.clear();
        self.scratch
            .extend(data.iter().map(|s| s.to_sample::<f32>()));

        self.meters.ingest(&self.scratch, self.channels);

        // The timestamp describes the first frame of this buffer, which is exactly
        // the frame index we have not yet counted. The very first callback therefore
        // marks frame 0 and becomes the recording's anchor.
        if self.frames_seen >= self.next_mark_at {
            let mark = TimeMark {
                frame_index: self.frames_seen,
                capture_nanos: info.timestamp().capture.as_nanos(),
            };
            // Dropping a mark costs a little precision in the drift fit, never audio.
            let _ = self.marks.push(mark);
            self.next_mark_at = self.frames_seen + self.mark_every;
        }

        if self.producer.push_entire_slice(&self.scratch).is_err() {
            // The writer fell behind. Count it rather than block the audio thread.
            self.meters.note_overrun();
        }

        self.frames_seen += frames;
    }
}

/// Dispatch on the device's sample format, since cpal panics on a mismatch.
fn build_for_format(
    device: &Device,
    negotiated: &Negotiated,
    ctx: CallbackCtx,
    err_tx: Sender<String>,
) -> Result<cpal::Stream> {
    macro_rules! build {
        ($t:ty) => {
            build_typed::<$t>(device, negotiated, ctx, err_tx)
        };
    }
    match negotiated.sample_format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I8 => build!(i8),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        other => Err(anyhow!("unsupported capture sample format {other:?}")),
    }
}

fn build_typed<T>(
    device: &Device,
    negotiated: &Negotiated,
    mut ctx: CallbackCtx,
    err_tx: Sender<String>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream::<T, _, _>(
            negotiated.config,
            move |data: &[T], info: &InputCallbackInfo| ctx.on_data(data, info),
            move |err| {
                // The receiver may be gone if the UI already tore down; that is fine.
                let _ = err_tx.send(err.to_string());
            },
            Some(BUILD_TIMEOUT),
        )
        .context("building the input stream")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::{InputStreamTimestamp, StreamInstant};

    fn ctx(channels: usize, mark_every: u64) -> (CallbackCtx, Consumer<f32>, Consumer<TimeMark>) {
        let (p, c) = RingBuffer::<f32>::new(4096);
        let (mp, mc) = RingBuffer::<TimeMark>::new(64);
        (
            CallbackCtx {
                channels,
                mark_every,
                producer: p,
                marks: mp,
                meters: Arc::new(Meters::new(channels, 48_000)),
                frames_seen: 0,
                next_mark_at: 0,
                scratch: Vec::with_capacity(4096),
            },
            c,
            mc,
        )
    }

    fn info(nanos: u64) -> InputCallbackInfo {
        let t = StreamInstant::from_nanos(nanos);
        InputCallbackInfo::new(InputStreamTimestamp {
            callback: t,
            capture: t,
        })
    }

    #[test]
    fn audio_reaches_the_ring_in_order() {
        let (mut ctx, mut rx, _) = ctx(2, 1000);
        ctx.on_data(&[0.1f32, 0.2, 0.3, 0.4], &info(0));
        let got: Vec<f32> = std::iter::from_fn(|| rx.pop().ok()).collect();
        assert_eq!(got, vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn integer_input_is_converted_to_float() {
        let (mut ctx, mut rx, _) = ctx(1, 1000);
        ctx.on_data(&[i16::MAX, 0, i16::MIN], &info(0));
        let got: Vec<f32> = std::iter::from_fn(|| rx.pop().ok()).collect();
        assert!((got[0] - 1.0).abs() < 1e-3, "{got:?}");
        assert!(got[1].abs() < 1e-6, "{got:?}");
        assert!((got[2] + 1.0).abs() < 1e-3, "{got:?}");
    }

    #[test]
    fn the_first_callback_anchors_frame_zero() {
        let (mut ctx, _rx, mut marks) = ctx(1, 48_000);
        ctx.on_data(&[0.0f32; 128], &info(12_345));
        let m = marks.pop().unwrap();
        assert_eq!(m.frame_index, 0, "the anchor must be the very first frame");
        assert_eq!(m.capture_nanos, 12_345);
    }

    #[test]
    fn marks_are_throttled_to_the_requested_interval() {
        // One mark per 1000 frames, fed 128 frames at a time.
        let (mut ctx, _rx, mut marks) = ctx(1, 1000);
        for i in 0..40u64 {
            ctx.on_data(&[0.0f32; 128], &info(i * 1000));
        }
        let got: Vec<TimeMark> = std::iter::from_fn(|| marks.pop().ok()).collect();
        // 40 * 128 = 5120 frames. A mark fires on the first callback at or past each
        // threshold, so: 0, 1024, 2048, 3072, 4096. The final callback begins at
        // 4992, short of the next threshold at 5096, so there is no sixth mark.
        assert_eq!(got.len(), 5, "{got:?}");
        assert_eq!(got[0].frame_index, 0);
        for w in got.windows(2) {
            let step = w[1].frame_index - w[0].frame_index;
            assert!((1000..1128).contains(&step), "step {step} out of range");
        }
    }

    #[test]
    fn frame_indices_track_the_number_of_frames_not_samples() {
        // Four channels: 512 samples is 128 frames.
        let (mut ctx, _rx, _m) = ctx(4, 1_000_000);
        ctx.on_data(&[0.0f32; 512], &info(0));
        assert_eq!(ctx.frames_seen, 128);
    }

    #[test]
    fn a_full_ring_counts_an_overrun_and_keeps_going() {
        let (p, _c) = RingBuffer::<f32>::new(4);
        let (mp, _mc) = RingBuffer::<TimeMark>::new(64);
        let meters = Arc::new(Meters::new(1, 48_000));
        let mut ctx = CallbackCtx {
            channels: 1,
            mark_every: 1_000_000,
            producer: p,
            marks: mp,
            meters: Arc::clone(&meters),
            frames_seen: 0,
            next_mark_at: 0,
            scratch: Vec::with_capacity(64),
        };
        // Eight samples will not fit in a ring of four.
        ctx.on_data(&[0.5f32; 8], &info(0));
        assert_eq!(meters.overruns(), 1);
        // The frame counter must still advance, or the time base would silently skew.
        assert_eq!(ctx.frames_seen, 8);
    }

    #[test]
    fn meters_see_the_audio() {
        let (mut ctx, _rx, _m) = ctx(2, 1_000_000);
        ctx.on_data(&[0.25f32, -0.75, 0.1, 0.2], &info(0));
        let mut peaks = Vec::new();
        ctx.meters.take_peaks(&mut peaks);
        assert!((peaks[0] - 0.25).abs() < 1e-6, "{peaks:?}");
        assert!((peaks[1] - 0.75).abs() < 1e-6, "{peaks:?}");
    }

    #[test]
    fn a_ragged_buffer_is_ignored_rather_than_miscounted() {
        let (mut ctx, _rx, _m) = ctx(4, 1_000_000);
        // Fewer samples than one full frame.
        ctx.on_data(&[0.0f32; 2], &info(0));
        assert_eq!(ctx.frames_seen, 0);
    }
}
