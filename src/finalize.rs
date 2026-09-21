//! Drift fitting, resampling and final BWF production.
//!
//! A take is recorded at whatever rate the converter actually ran at — 47999.4 Hz
//! when it claims 48000, say — because fighting the hardware in real time would mean
//! resampling on the audio thread against a rate we do not yet know. Instead we
//! measure. Every second or so the capture path drops a `(sample_index, utc)` pair,
//! and by the end of the take those pairs describe a line whose slope *is* the
//! device's true sample rate. Correcting the file to exactly 48000 Hz afterwards is
//! what makes the sample count a time base in its own right: sample 480000 is one
//! second after sample 0, and can be trusted to be.
//!
//! Two things in here deserve more suspicion than the rest.
//!
//! The first is numerical. UTC in unix nanoseconds is about 1.8e18, which needs 61
//! bits; `f64` has 53. Feeding raw nanoseconds into a least-squares fit quantises
//! every observation to 256 ns steps before the arithmetic even starts, and the
//! textbook uncentered normal equations then subtract two numbers near 1e28 from each
//! other. Everything here is therefore fitted in *differences from the first
//! observation*, which are small, exact and well-conditioned. See
//! `subtracting_the_first_observation_is_what_keeps_the_fit_exact`.
//!
//! The second is the safety gate. The corrected file replaces the raw one, so a bad
//! correction is not a degraded take, it is a destroyed take — the original is gone
//! and the audio has been stretched by a ratio we made up. The gate therefore has to
//! be satisfied on every count before the raw is deleted, and when it is not we keep
//! the raw, ship the *unresampled* audio, and say why. A take flagged "not corrected"
//! is a take the operator can still rescue; a silently mis-stretched one is not.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use bwavfile::{WaveFmt, WaveReader, WaveWriter};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use crate::audio::TARGET_RATE;
use crate::audio::writer::DriftObservation;
use crate::bwf::{self, Provenance};
use crate::clock::RefStatus;

/// Accepted SNTP exchanges the clock must have behind it. Mirrors
/// [`crate::clock::SYNCED_THRESHOLD`]; checked separately so that loosening the
/// clock's own definition of "synced" cannot quietly loosen this.
///
/// Only applies to a reference that counts exchanges at all. An ethersync leader
/// is the reference and has nobody to exchange with, so the criterion is skipped
/// there rather than being satisfied with a made-up number — see
/// [`RefStatus::samples`].
pub const MIN_CLOCK_SAMPLES: u64 = 3;

/// Drift observations required before the fit is believed.
///
/// Marks arrive about once a second, so this is also, in practice, a floor on take
/// length: a fit through a handful of points spread over a few seconds measures the
/// NTP path's jitter, not the crystal.
pub const MIN_OBSERVATIONS: usize = 30;

/// How far the correction may stray from unity, in parts per million.
///
/// Real converter crystals live inside about +/-100 ppm. Past 200 ppm the honest
/// conclusion is that the fit is wrong, not that the hardware is exotic — and
/// stretching the audio by a wrong ratio is exactly the outcome the gate exists to
/// prevent.
pub const MAX_DRIFT_PPM: f64 = 200.0;

/// Ceiling on the scatter of the observations about the fitted line, in seconds.
///
/// Each observation's UTC comes from the clock model, so its error is that model's
/// dispersion: tens of microseconds against a LAN server, one to three milliseconds
/// against a public one on a busy link. 5 ms is generous against that and still a
/// hard wall against nonsense — it is more time than a 200 ppm rate error accumulates
/// in 25 seconds, so residuals that large swamp the very signal we are trying to
/// measure. At 48 kHz it is 240 samples.
pub const MAX_RESIDUAL_RMS_S: f64 = 5.0e-3;

/// Shortest observation span the gate will accept, in seconds.
///
/// Not one of the headline criteria, but the slope's standard error scales as
/// `residual / (span * sqrt(n))`: 30 points over two seconds can pass every other
/// check and still put the rate out by hundreds of ppm. It also guarantees the clip
/// is long enough for [`resample_interleaved`] to work at all.
pub const MIN_SPAN_S: f64 = 20.0;

/// Frames the resampler processes per call.
///
/// Rubato's whole-clip helper only trims its own startup delay from inside its
/// full-chunk loop, so a clip shorter than a chunk comes out as leading silence.
/// [`MIN_RESAMPLE_FRAMES`] keeps us well clear of that edge; the gate's span
/// requirement keeps real takes well clear of it too.
pub const RESAMPLE_CHUNK: usize = 1024;

/// Shortest clip [`resample_interleaved`] will touch. See [`RESAMPLE_CHUNK`].
pub const MIN_RESAMPLE_FRAMES: usize = 4 * RESAMPLE_CHUNK;

// ---------------------------------------------------------------------------
// The drift fit
// ---------------------------------------------------------------------------

/// A least-squares line through the drift observations.
///
/// The model is `utc_seconds = alpha + beta * sample_index`, fitted in differences
/// from the first observation. `beta` is seconds per sample, so its reciprocal is the
/// rate the converter was really running at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftFit {
    /// Seconds per device sample: the fitted slope, and the primary quantity. The
    /// rate is derived from this rather than the other way round.
    pub seconds_per_sample: f64,
    /// The rate the device actually ran at, in Hz.
    pub measured_rate: f64,
    /// `TARGET_RATE / measured_rate` — the factor the audio must be resampled by.
    ///
    /// Note this folds rate *conversion* in with drift correction: if the device had
    /// to fall back to 44.1 kHz, this is about 1.088, not about 1.000.
    pub drift_ratio: f64,
    /// Scatter of the observations about the line, in seconds (RMS, over the fit's
    /// degrees of freedom rather than the point count).
    pub residual_rms_s: f64,
    /// How many observations went into the fit.
    pub points: usize,
    /// First to last observation, in device samples.
    pub span_samples: u64,
    /// The same span in seconds, per the fitted rate.
    pub span_seconds: f64,
    /// UTC at sample index 0 according to the whole line.
    ///
    /// Read off the fit rather than off whichever mark happened to land first, so a
    /// single unlucky first observation cannot shift the entire file. Offered to the
    /// caller; `finalize` does not silently substitute it for the anchor the recorder
    /// already chose.
    pub t0_unix_nanos: i128,
}

impl DriftFit {
    /// The correction expressed the way clock people read it.
    pub fn drift_ppm(&self) -> f64 {
        (self.drift_ratio - 1.0) * 1.0e6
    }

    /// How far the converter's crystal was off *its own nominal rate*, in ppm.
    ///
    /// For a device running at 48 kHz this is (to within 4e-8) the same number as
    /// [`drift_ppm`](Self::drift_ppm). They part company only when the device could
    /// not give us 48 kHz at all, in which case `drift_ratio` is dominated by the
    /// rate conversion and says nothing about the hardware.
    pub fn crystal_ppm(&self, nominal_rate: u32) -> f64 {
        (self.measured_rate / nominal_rate as f64 - 1.0) * 1.0e6
    }

    /// UTC at an arbitrary sample index, per the fitted line.
    pub fn utc_nanos_at(&self, sample_index: u64) -> i128 {
        let dt = self.seconds_per_sample * sample_index as f64;
        self.t0_unix_nanos + (dt * 1.0e9).round() as i128
    }
}

/// Fit `utc_seconds = alpha + beta * sample_index` over the observations.
///
/// Returns `None` when there is nothing to fit: fewer than two points, every point
/// at the same sample index, or a slope that is not a positive finite number (time
/// running backwards across the take, which means the input is corrupt rather than
/// the crystal being unusual).
///
/// Both axes are reduced to differences from the first observation before any
/// floating-point arithmetic happens. On the index axis the subtraction is exact
/// integer work; on the time axis it happens in `i128` and only the small difference
/// is ever converted to `f64`. That is the whole trick, and it is the difference
/// between recovering 47999.4 Hz to 3e-8 Hz and recovering it to 2e-3 Hz.
pub fn fit_drift(observations: &[DriftObservation]) -> Option<DriftFit> {
    let n = observations.len();
    if n < 2 {
        return None;
    }

    let origin = observations[0];

    // Differences, not absolutes. `sample_index` is u64 and the subtraction is done
    // in i128 so that an out-of-order observation is a negative offset rather than a
    // wrapped one.
    let dx = |o: &DriftObservation| (o.sample_index as i128 - origin.sample_index as i128) as f64;
    let dy = |o: &DriftObservation| (o.utc_unix_nanos - origin.utc_unix_nanos) as f64 / 1.0e9;

    let inv = 1.0 / n as f64;
    let mx = observations.iter().map(dx).sum::<f64>() * inv;
    let my = observations.iter().map(dy).sum::<f64>() * inv;

    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for o in observations {
        let x = dx(o) - mx;
        sxx += x * x;
        sxy += x * (dy(o) - my);
    }
    if sxx.is_nan() || sxx <= 0.0 {
        // Every mark landed on the same sample index. There is no line.
        return None;
    }

    let beta = sxy / sxx;
    if !beta.is_finite() || beta <= 0.0 {
        return None;
    }
    let alpha = my - beta * mx;

    let measured_rate = 1.0 / beta;
    if !measured_rate.is_finite() || measured_rate <= 0.0 {
        return None;
    }

    // Two fitted parameters, so two degrees of freedom are spent. Dividing by `n`
    // instead would flatter a two-point fit into claiming zero scatter.
    let sse: f64 = observations
        .iter()
        .map(|o| {
            let r = dy(o) - (alpha + beta * dx(o));
            r * r
        })
        .sum();
    let dof = n.saturating_sub(2).max(1);

    let lo = observations.iter().map(|o| o.sample_index).min()?;
    let hi = observations.iter().map(|o| o.sample_index).max()?;
    let span_samples = hi - lo;

    // UTC at index 0: walk the line back from the origin observation, in seconds,
    // and only then return to absolute nanoseconds.
    let to_zero = alpha + beta * -(origin.sample_index as f64);
    let t0_unix_nanos = origin.utc_unix_nanos + (to_zero * 1.0e9).round() as i128;

    Some(DriftFit {
        seconds_per_sample: beta,
        measured_rate,
        drift_ratio: TARGET_RATE as f64 / measured_rate,
        residual_rms_s: (sse / dof as f64).sqrt(),
        points: n,
        span_samples,
        span_seconds: span_samples as f64 * beta,
        t0_unix_nanos,
    })
}

// ---------------------------------------------------------------------------
// The safety gate
// ---------------------------------------------------------------------------

/// One reason the correction was not trusted.
///
/// Rich rather than boolean because the operator can act on most of these: "only 11
/// drift observations" means record for longer, "clock never synced" means check the
/// network, and "fit too noisy" means the NTP path is unusable from here.
#[derive(Debug, Clone, PartialEq)]
pub enum GateFailure {
    /// The time reference never reached a state its timestamps could be trusted in.
    ClockNotSynced { state: String },
    /// Synced, but on fewer exchanges than we insist on.
    TooFewClockSamples { accepted: u64, required: u64 },
    /// Not enough `(sample, utc)` pairs to fit anything trustworthy.
    TooFewObservations { got: usize, required: usize },
    /// The observations exist but do not describe a line: no span, or time running
    /// backwards.
    NoUsableFit { observations: usize },
    /// The observations are bunched into too short a window to determine a rate.
    SpanTooShort { span_s: f64, required_s: f64 },
    /// The implied crystal error is beyond anything real hardware does.
    ImplausibleDrift { ppm: f64, limit_ppm: f64 },
    /// The points scatter too far off the line for the slope to mean anything.
    NoisyFit { residual_rms_s: f64, limit_s: f64 },
    /// The corrected file did not come back off disk the way we wrote it.
    OutputNotVerified { reason: String },
}

impl std::fmt::Display for GateFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClockNotSynced { state } => {
                write!(f, "clock never synced (state: {state})")
            }
            Self::TooFewClockSamples { accepted, required } => write!(
                f,
                "only {accepted} accepted NTP exchanges, {required} needed"
            ),
            Self::TooFewObservations { got, required } => {
                write!(f, "only {got} drift observations, {required} needed")
            }
            Self::NoUsableFit { observations } => write!(
                f,
                "{observations} observations do not describe a usable line"
            ),
            Self::SpanTooShort { span_s, required_s } => write!(
                f,
                "observations span only {span_s:.1} s, {required_s:.0} s needed"
            ),
            Self::ImplausibleDrift { ppm, limit_ppm } => write!(
                f,
                "implied drift {ppm:.1} ppm exceeds the {limit_ppm:.0} ppm limit"
            ),
            Self::NoisyFit {
                residual_rms_s,
                limit_s,
            } => write!(
                f,
                "fit residual {:.2} ms exceeds the {:.2} ms limit",
                residual_rms_s * 1.0e3,
                limit_s * 1.0e3
            ),
            Self::OutputNotVerified { reason } => {
                write!(f, "corrected file failed verification: {reason}")
            }
        }
    }
}

/// Everything the gate found wrong, or nothing at all.
///
/// Collects *all* the failures rather than short-circuiting on the first: an operator
/// told "not enough observations" who then records for longer, only to be told "clock
/// never synced", has been served badly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GateReport {
    failures: Vec<GateFailure>,
}

impl GateReport {
    /// True only when every criterion held.
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn failures(&self) -> &[GateFailure] {
        &self.failures
    }

    /// A one-line explanation for the UI, or `None` when nothing went wrong.
    pub fn reason(&self) -> Option<String> {
        if self.failures.is_empty() {
            return None;
        }
        Some(
            self.failures
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// The single most actionable reason, in a few words, for the status bar.
    ///
    /// `reason` lists every unmet condition, which is right for a log and far too
    /// much for a window. The failures are not independent — a short take trips the
    /// span check, the observation-count check and often the fit as well — so
    /// showing all of them buries the one thing the operator can act on.
    pub fn short_reason(&self) -> Option<&'static str> {
        if self.failures.is_empty() {
            return None;
        }
        // Ordered by what the operator would do about it, not by severity.
        let has = |pred: fn(&GateFailure) -> bool| self.failures.iter().any(pred);

        if has(|f| {
            matches!(
                f,
                GateFailure::SpanTooShort { .. }
                    | GateFailure::TooFewObservations { .. }
                    | GateFailure::NoUsableFit { .. }
            )
        }) {
            // Over a take this short the drift is a fraction of a millisecond
            // anyway, so this is a statement about the measurement, not a fault.
            return Some("too short to measure drift");
        }
        if has(|f| {
            matches!(
                f,
                GateFailure::ClockNotSynced { .. } | GateFailure::TooFewClockSamples { .. }
            )
        }) {
            return Some("clock was not synced");
        }
        if has(|f| matches!(f, GateFailure::ImplausibleDrift { .. })) {
            return Some("measured drift implausible");
        }
        if has(|f| matches!(f, GateFailure::NoisyFit { .. })) {
            return Some("clock too noisy to fit");
        }
        Some("could not verify the correction")
    }

    fn push(&mut self, failure: GateFailure) {
        self.failures.push(failure);
    }
}

/// Decide whether the measured correction may be applied destructively.
///
/// `nominal_rate` is the rate the device claimed to be running at, which is what the
/// plausibility check is measured against — see [`DriftFit::crystal_ppm`].
///
/// This covers everything knowable before the output file exists. `finalize` adds
/// [`GateFailure::OutputNotVerified`] afterwards if the write did not survive a
/// read-back.
pub fn evaluate_gate(
    clock: &RefStatus,
    observations: &[DriftObservation],
    fit: Option<&DriftFit>,
    nominal_rate: u32,
) -> GateReport {
    let mut report = GateReport::default();

    if !clock.synced {
        report.push(GateFailure::ClockNotSynced {
            state: clock.label.clone(),
        });
    }
    if let Some(accepted) = clock.samples
        && accepted < MIN_CLOCK_SAMPLES
    {
        report.push(GateFailure::TooFewClockSamples {
            accepted,
            required: MIN_CLOCK_SAMPLES,
        });
    }
    if observations.len() < MIN_OBSERVATIONS {
        report.push(GateFailure::TooFewObservations {
            got: observations.len(),
            required: MIN_OBSERVATIONS,
        });
    }

    let Some(fit) = fit else {
        report.push(GateFailure::NoUsableFit {
            observations: observations.len(),
        });
        return report;
    };

    if fit.span_seconds < MIN_SPAN_S {
        report.push(GateFailure::SpanTooShort {
            span_s: fit.span_seconds,
            required_s: MIN_SPAN_S,
        });
    }

    // The `is_finite` arms are not belt and braces: a NaN fails every comparison, so
    // written the other way round a NaN would sail through the gate.
    let ppm = fit.crystal_ppm(nominal_rate);
    if !ppm.is_finite() || ppm.abs() >= MAX_DRIFT_PPM {
        report.push(GateFailure::ImplausibleDrift {
            ppm,
            limit_ppm: MAX_DRIFT_PPM,
        });
    }

    if !fit.residual_rms_s.is_finite() || fit.residual_rms_s >= MAX_RESIDUAL_RMS_S {
        report.push(GateFailure::NoisyFit {
            residual_rms_s: fit.residual_rms_s,
            limit_s: MAX_RESIDUAL_RMS_S,
        });
    }

    report
}

// ---------------------------------------------------------------------------
// Reading, resampling, writing
// ---------------------------------------------------------------------------

/// The live scratch capture, pulled into memory.
#[derive(Debug)]
struct RawTake {
    fmt: WaveFmt,
    /// Interleaved `f32`, exactly as the capture path wrote it. Shorter than the
    /// header promises if the recorder was killed mid-take.
    samples: Vec<f32>,
}

/// Read `rec-N.raw.wav` whole.
///
/// The scratch file is 32-bit float at the device's own rate, so this is lossless and
/// the only quantisation in the pipeline happens on the way out. Holding the take in
/// memory costs about 700 MB per stereo hour, which is the price of rubato's
/// whole-clip path; a streaming resample would need to know the output length up
/// front and would still have to buffer the filter's history.
fn read_raw(path: &Path) -> Result<RawTake> {
    let mut reader =
        WaveReader::open(path).with_context(|| format!("opening raw take {}", path.display()))?;
    let fmt = reader
        .format()
        .with_context(|| format!("reading fmt chunk of {}", path.display()))?;
    let frames = reader
        .frame_length()
        .with_context(|| format!("reading frame count of {}", path.display()))?;

    let channels = fmt.channel_count as usize;
    ensure!(channels > 0, "{} declares zero channels", path.display());
    ensure!(
        fmt.sample_rate > 0,
        "{} declares a zero sample rate",
        path.display()
    );

    let total = (frames as usize)
        .checked_mul(channels)
        .ok_or_else(|| anyhow!("{} is implausibly large", path.display()))?;
    let mut samples = vec![0.0f32; total];

    let mut audio = reader
        .audio_frame_reader()
        .with_context(|| format!("opening the data chunk of {}", path.display()))?;

    // Read in blocks rather than one call, so a truncated file (the recorder was
    // killed mid-take) yields the frames that are there instead of an error.
    const BLOCK_FRAMES: usize = 16_384;
    let mut filled = 0usize;
    while filled < total {
        let end = (filled + BLOCK_FRAMES * channels).min(total);
        let got = audio
            .read_frames(&mut samples[filled..end])
            .with_context(|| format!("reading audio from {}", path.display()))?;
        if got == 0 {
            break;
        }
        filled += got as usize * channels;
    }
    samples.truncate(filled);

    Ok(RawTake { fmt, samples })
}

/// Resample an interleaved `f32` clip by `ratio`.
///
/// Asynchronous sinc interpolation, because the ratio is an arbitrary real number
/// like 1.0000125 rather than a ratio of small integers. A 256-tap Blackman-Harris²
/// windowed sinc with 256x oversampling and cubic interpolation between the stored
/// phases puts the resampling artefacts far below the 24-bit noise floor we are about
/// to quantise to, which is the only bar that matters here: this runs once, offline,
/// on a take that is already on disk, so there is no reason to trade quality for
/// speed.
///
/// Rejects clips shorter than [`MIN_RESAMPLE_FRAMES`]. Rubato's whole-clip helper
/// trims its own startup delay from inside its full-chunk loop, so a clip that never
/// fills a chunk would come back as leading silence — better to refuse than to
/// return something that looks like audio and is not.
///
/// The trim is to the nearest whole frame (`taps * ratio / 2`, truncated), so a
/// constant offset of a frame or so survives it. That is about 21 us at 48 kHz,
/// which sits an order of magnitude below the NTP dispersion the timestamps already
/// carry, so it is left alone rather than chased; see
/// `resampling_preserves_the_waveform_to_within_a_sample_of_alignment`.
pub fn resample_interleaved(input: &[f32], channels: u16, ratio: f64) -> Result<Vec<f32>> {
    let channels = channels as usize;
    ensure!(channels > 0, "cannot resample a zero-channel clip");
    ensure!(
        input.len().is_multiple_of(channels),
        "interleaved buffer of {} samples is not a whole number of {channels}-channel frames",
        input.len()
    );
    ensure!(
        ratio.is_finite() && ratio > 0.0,
        "resample ratio {ratio} is not a positive finite number"
    );

    let frames = input.len() / channels;
    ensure!(
        frames >= MIN_RESAMPLE_FRAMES,
        "clip of {frames} frames is too short to resample cleanly ({MIN_RESAMPLE_FRAMES} needed)"
    );

    let params = SincInterpolationParameters::new(256, WindowFunction::BlackmanHarris2)
        .oversampling_factor(256)
        .interpolation(SincInterpolationType::Cubic);

    // The ratio is fixed for the whole clip, so the relative range only has to be
    // >= 1.0; 1.1 costs a slightly larger internal buffer and nothing else.
    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        1.1,
        &params,
        RESAMPLE_CHUNK,
        channels,
        FixedAsync::Input,
    )
    .map_err(|e| anyhow!("building the resampler for ratio {ratio}: {e}"))?;

    let buffer_in = InterleavedSlice::new(input, channels, frames)
        .expect("input length is exactly channels * frames");
    let needed = resampler.process_all_needed_output_len(frames);
    let mut buffer_out = InterleavedOwned::<f32>::new(0.0, channels, needed);

    let (_consumed, produced) = resampler
        .process_all_into_buffer(&buffer_in, &mut buffer_out, frames, None)
        .map_err(|e| anyhow!("resampling {frames} frames by {ratio}: {e}"))?;

    // The buffer is sized for the worst case; the valid audio is the first
    // `produced` frames and the rest is padding.
    let mut data = buffer_out.take_data();
    data.truncate(produced * channels);
    Ok(data)
}

/// Write an interleaved `f32` buffer out as a 24-bit BWF.
///
/// Rate, channel count and metadata all come from `p`, so the provenance and the file
/// cannot disagree about what was written.
///
/// `bext` and iXML go in before the audio. bwavfile appends chunks in call order and
/// has no way to insert one later, and a `bext` sitting after a multi-gigabyte `data`
/// chunk is a `bext` that plenty of tools will never look far enough to find.
pub fn write_bwf(path: &Path, p: &Provenance, interleaved: &[f32]) -> Result<u64> {
    let channels = p.channels as usize;
    ensure!(channels > 0, "cannot write a zero-channel file");
    ensure!(
        interleaved.len().is_multiple_of(channels),
        "interleaved buffer of {} samples is not a whole number of {channels}-channel frames",
        interleaved.len()
    );

    let fmt = bwf::wave_fmt_pcm24(p.sample_rate, p.channels);
    let mut writer =
        WaveWriter::create(path, fmt).with_context(|| format!("creating {}", path.display()))?;

    writer
        .write_broadcast_metadata(&bwf::bext(p))
        .with_context(|| format!("writing bext to {}", path.display()))?;
    writer
        .write_ixml(bwf::ixml(p).as_bytes())
        .with_context(|| format!("writing iXML to {}", path.display()))?;

    let mut audio = writer
        .audio_frame_writer()
        .with_context(|| format!("opening the data chunk of {}", path.display()))?;

    // Copy through a scratch block rather than clamping the caller's buffer in place:
    // the caller may still want the unclamped audio, and the block keeps the writer's
    // own staging buffer bounded on a long take.
    const BLOCK_FRAMES: usize = 8192;
    let mut block = Vec::with_capacity(BLOCK_FRAMES * channels);
    for chunk in interleaved.chunks(BLOCK_FRAMES * channels) {
        block.clear();
        block.extend_from_slice(chunk);
        // Mandatory, not cosmetic: the f32 -> 24-bit conversion below us wraps
        // modulo, so an uncontained sample becomes a full-scale polarity flip.
        bwf::clamp_buffer_for_pcm24(&mut block);
        audio
            .write_frames(&block)
            .with_context(|| format!("writing audio to {}", path.display()))?;
    }

    // `end()` finalises the data chunk and hands back the writer; dropping that
    // flushes and closes the file.
    let closed = audio
        .end()
        .with_context(|| format!("finalising {}", path.display()))?;
    drop(closed);

    Ok((interleaved.len() / channels) as u64)
}

/// Re-open a file we just wrote and check it says what we meant it to say.
///
/// Cheap, and the only thing standing between a truncated write and `remove_file` on
/// the original.
fn verify_written(path: &Path, p: &Provenance, frames: u64) -> Result<()> {
    let mut reader =
        WaveReader::open(path).with_context(|| format!("reopening {}", path.display()))?;
    let fmt = reader
        .format()
        .with_context(|| format!("reading back the fmt chunk of {}", path.display()))?;
    let got = reader
        .frame_length()
        .with_context(|| format!("reading back the frame count of {}", path.display()))?;

    ensure!(
        fmt.sample_rate == p.sample_rate,
        "wrote {} Hz but read back {} Hz",
        p.sample_rate,
        fmt.sample_rate
    );
    ensure!(
        fmt.channel_count == p.channels,
        "wrote {} channels but read back {}",
        p.channels,
        fmt.channel_count
    );
    ensure!(
        fmt.bits_per_sample == 24,
        "wrote 24-bit but read back {}-bit",
        fmt.bits_per_sample
    );
    ensure!(got == frames, "wrote {frames} frames but read back {got}");
    Ok(())
}

// ---------------------------------------------------------------------------
// The whole job
// ---------------------------------------------------------------------------

/// What finalising a take actually did.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The drift fit, if the observations supported one. Present even when the gate
    /// failed, so the UI can show the measurement that was rejected.
    pub fit: Option<DriftFit>,
    /// Why the correction was or was not trusted.
    pub gate: GateReport,
    /// Whether the audio in `final_wav` was actually resampled.
    pub resampled: bool,
    /// Whether the raw scratch file was removed.
    pub raw_deleted: bool,
    /// Where the raw file still is, when it was kept.
    pub raw_kept: Option<PathBuf>,
    /// Frames in the file that shipped.
    pub frames_written: u64,
    /// Provenance as actually written into the file, with `sample_rate`,
    /// `measured_rate`, `drift_ratio`, `resampled`, `channels` and
    /// `bits_per_sample` filled in from what happened.
    pub provenance: Provenance,
}

impl Outcome {
    /// True when the take shipped corrected and the scratch file is gone.
    pub fn corrected(&self) -> bool {
        self.resampled && self.raw_deleted
    }
}

/// Turn `rec-N.raw.wav` into `rec-N.wav`, and delete the raw only if it is safe to.
///
/// `provenance` supplies everything finalising cannot know — the anchor timestamp,
/// the device name, the NTP server, the operator's latency trim. The fields that
/// describe what *this* pass did are overwritten from the file and the fit, so a
/// caller cannot accidentally stamp "resampled: yes" on a take that was not.
///
/// On a gate failure the output is still produced, but from the unresampled audio and
/// at the device's own rate — a 47999.4 Hz file honestly labelled 47999-ish is
/// recoverable, one labelled 48000 is a trap. The raw is left in place and
/// [`Outcome::gate`] says why.
///
/// Errors are reserved for "we could not produce an output file at all". Anything
/// short of that is reported through the outcome, because the raw take is still on
/// disk and the operator needs to be told, not thrown an error.
pub fn finalize(
    raw: &Path,
    out: &Path,
    observations: &[DriftObservation],
    clock: &RefStatus,
    provenance: &Provenance,
) -> Result<Outcome> {
    let take = read_raw(raw)?;
    let channels = take.fmt.channel_count;
    let device_rate = take.fmt.sample_rate;

    let fit = fit_drift(observations);
    let mut gate = evaluate_gate(clock, observations, fit.as_ref(), device_rate);

    // Resample only when everything checked out. `fit` is always `Some` here: the
    // gate reports `NoUsableFit` otherwise.
    let (audio, resampled) = match (gate.passed(), fit) {
        (true, Some(f)) => match resample_interleaved(&take.samples, channels, f.drift_ratio) {
            Ok(resampled) => (resampled, true),
            Err(e) => {
                // Refusing to resample is a gate failure, not a fatal error: the
                // unresampled take is still worth shipping.
                gate.push(GateFailure::OutputNotVerified {
                    reason: format!("{e:#}"),
                });
                (take.samples, false)
            }
        },
        _ => (take.samples, false),
    };

    let mut p = provenance.clone();
    p.channels = channels;
    p.bits_per_sample = 24;
    p.device_rate = device_rate;
    // An uncorrected file is still running at the device's rate. Labelling it 48000
    // would make its sample count lie about time, which is the one thing this
    // program exists to prevent.
    p.sample_rate = if resampled { TARGET_RATE } else { device_rate };
    p.measured_rate = fit.map(|f| f.measured_rate);
    p.drift_ratio = fit.map(|f| f.drift_ratio);
    p.resampled = resampled;

    let frames_written = write_bwf(out, &p, &audio)?;

    if let Err(e) = verify_written(out, &p, frames_written) {
        gate.push(GateFailure::OutputNotVerified {
            reason: format!("{e:#}"),
        });
    }

    // The point of no return. Everything above has to have held.
    let raw_deleted = if gate.passed() && resampled {
        std::fs::remove_file(raw)
            .with_context(|| format!("removing the raw take {}", raw.display()))?;
        true
    } else {
        false
    };

    Ok(Outcome {
        fit,
        gate,
        resampled,
        raw_deleted,
        raw_kept: (!raw_deleted).then(|| raw.to_path_buf()),
        frames_written,
        provenance: p,
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_short_take_reports_its_length_not_a_list_of_conditions() {
        // The case a user actually hits: NTP is fine, the take is 8 seconds.
        let clock = synced_clock();
        let obs = observations(8, 48_000.0);
        let fit = fit_drift(&obs);
        let gate = evaluate_gate(&clock, &obs, fit.as_ref(), 48_000);
        assert!(!gate.passed());
        assert_eq!(gate.short_reason(), Some("too short to measure drift"));
        // The full list is still available for the file and the log.
        assert!(gate.reason().unwrap().contains("span"));
    }

    #[test]
    fn a_passing_gate_has_no_short_reason() {
        let clock = synced_clock();
        let obs = observations(60, 47_999.4);
        let fit = fit_drift(&obs);
        let gate = evaluate_gate(&clock, &obs, fit.as_ref(), 48_000);
        assert!(gate.passed(), "{:?}", gate.reason());
        assert_eq!(gate.short_reason(), None);
    }

    #[test]
    fn an_unsynced_clock_outranks_other_complaints_once_the_take_is_long_enough() {
        let clock = clock_with(0);
        let obs = observations(60, 47_999.4);
        let fit = fit_drift(&obs);
        let gate = evaluate_gate(&clock, &obs, fit.as_ref(), 48_000);
        assert_eq!(gate.short_reason(), Some("clock was not synced"));
    }
    use super::*;
    use crate::clock::ClockModel;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// A realistic epoch: ~1.8e18 ns needs 61 bits, eight more than an f64 mantissa.
    const EPOCH: i128 = 1_800_000_000_123_456_789;

    // -- helpers ------------------------------------------------------------

    /// Observations for a device that claims 48000 and really runs at `rate`, marked
    /// once per nominal second for `secs` seconds.
    fn observations(secs: usize, rate: f64) -> Vec<DriftObservation> {
        (0..secs)
            .map(|k| {
                let sample_index = (k as u64) * 48_000;
                DriftObservation {
                    sample_index,
                    utc_unix_nanos: EPOCH + (sample_index as f64 / rate * 1.0e9).round() as i128,
                }
            })
            .collect()
    }

    /// The same, with deterministic pseudo-random jitter of up to `jitter_s`.
    fn jittered(secs: usize, rate: f64, jitter_s: f64) -> Vec<DriftObservation> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        observations(secs, rate)
            .into_iter()
            .map(|mut o| {
                // xorshift64*, so the test is reproducible without a dependency.
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                let unit =
                    (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
                let offset = (unit * 2.0 - 1.0) * jitter_s;
                o.utc_unix_nanos += (offset * 1.0e9).round() as i128;
                o
            })
            .collect()
    }

    /// An NTP reference with `accepted` good exchanges behind it.
    fn clock_with(accepted: usize) -> RefStatus {
        let base = Instant::now();
        let mut model = ClockModel::new(base);
        for k in 0..accepted {
            let x = k as f64 * 16.0;
            model.push(
                base + Duration::from_secs_f64(x),
                EPOCH + (x * 1.0e9) as i128,
                0.01,
                0.0,
            );
        }
        crate::clock::NtpReference::status_of("time.apple.com", &model.snapshot())
    }

    fn synced_clock() -> RefStatus {
        clock_with(8)
    }

    /// A reference that does not count exchanges, the way an ethersync leader
    /// does not: it is the clock, so there is nobody to exchange with.
    fn uncounted_clock(synced: bool) -> RefStatus {
        RefStatus {
            kind: "ethersync",
            source: "ethersync leader".into(),
            synced,
            label: if synced { "rolling" } else { "idle" }.into(),
            samples: None,
            discarded: None,
            dispersion_s: None,
            slope_ppm: None,
        }
    }

    fn provenance(channels: u16, device_rate: u32) -> Provenance {
        Provenance {
            t0_unix_nanos: EPOCH,
            sample_rate: device_rate,
            channels,
            bits_per_sample: 32,
            device_name: "Scarlett 2i2".into(),
            device_rate,
            measured_rate: None,
            drift_ratio: None,
            resampled: false,
            clock_source: "time.apple.com".into(),
            clock_dispersion_s: Some(0.004),
            sync_state: "synced".into(),
            slope_ppm: Some(-12.0),
            latency_offset_ms: 0.0,
            timecode: None,
        }
    }

    /// A scratch directory of our own, cleaned up by the caller.
    fn scratch_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "syncrec-finalize-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        std::fs::create_dir_all(&dir).expect("creating the scratch directory");
        dir
    }

    /// Write a 32-bit float scratch capture, the way the live recorder does.
    fn write_raw_f32(path: &Path, rate: u32, channels: u16, interleaved: &[f32]) {
        let writer = WaveWriter::create(path, bwf::wave_fmt_f32(rate, channels))
            .expect("creating the raw take");
        let mut audio = writer.audio_frame_writer().expect("opening the data chunk");
        audio.write_frames(interleaved).expect("writing raw audio");
        audio.end().expect("finalising the raw take");
    }

    /// Read a 24-bit BWF back as interleaved f32.
    fn read_back(path: &Path) -> (WaveFmt, Vec<f32>) {
        let mut reader = WaveReader::open(path).expect("opening the written file");
        let fmt = reader.format().expect("reading fmt");
        let frames = reader.frame_length().expect("reading frame count");
        let mut buf = vec![0.0f32; frames as usize * fmt.channel_count as usize];
        let mut audio = reader.audio_frame_reader().expect("opening data");
        audio.read_frames(&mut buf).expect("reading audio");
        (fmt, buf)
    }

    /// A stereo tone, loud but inside full scale, long enough to resample.
    fn tone(frames: usize, channels: u16) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * channels as usize);
        for i in 0..frames {
            let s = (i as f32 * 0.05).sin() * 0.5;
            for c in 0..channels {
                // Distinct per channel, so a channel swap is visible.
                v.push(s * (1.0 - 0.25 * c as f32));
            }
        }
        v
    }

    // -- the fit: numerics ---------------------------------------------------

    #[test]
    fn recovers_a_known_device_rate_from_realistic_utc_nanoseconds() {
        let fit = fit_drift(&observations(600, 47_999.4)).expect("600 points describe a line");
        assert!(
            (fit.measured_rate - 47_999.4).abs() < 0.01,
            "measured {} Hz",
            fit.measured_rate
        );
        // Far tighter than the bar, because the differencing costs us almost nothing.
        assert!(
            (fit.measured_rate - 47_999.4).abs() < 1.0e-4,
            "measured {} Hz",
            fit.measured_rate
        );
        assert!((fit.drift_ratio - 48_000.0 / 47_999.4).abs() < 1.0e-10);
        assert_eq!(fit.points, 600);
    }

    #[test]
    fn subtracting_the_first_observation_is_what_keeps_the_fit_exact() {
        // The naive version: fit raw f64 nanoseconds with the textbook uncentered
        // normal equations, which is what you write if you do not think about it.
        let obs = observations(60, 47_999.4);
        let n = obs.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for o in &obs {
            let x = o.sample_index as f64;
            let y = o.utc_unix_nanos as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let naive_beta_ns = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        let naive_rate = 1.0e9 / naive_beta_ns;
        let naive_t0 = (sy - naive_beta_ns * sx) / n;

        let fit = fit_drift(&obs).expect("60 points describe a line");

        let naive_rate_err = (naive_rate - 47_999.4).abs();
        let ours_rate_err = (fit.measured_rate - 47_999.4).abs();
        assert!(
            naive_rate_err > 1.0e-4,
            "the naive fit was supposed to be the bad one: err {naive_rate_err}"
        );
        assert!(
            ours_rate_err < naive_rate_err / 1000.0,
            "ours {ours_rate_err} Hz vs naive {naive_rate_err} Hz"
        );

        // The intercept is where the loss of precision really shows: 1.8e18 ns is
        // held to 256 ns steps in an f64, so the naive t0 cannot be better than that.
        let naive_t0_err = (naive_t0 - EPOCH as f64).abs();
        let ours_t0_err = (fit.t0_unix_nanos - EPOCH).unsigned_abs() as f64;
        assert!(naive_t0_err > 256.0, "naive t0 err {naive_t0_err} ns");
        assert!(ours_t0_err < 100.0, "our t0 err {ours_t0_err} ns");
    }

    #[test]
    fn a_perfect_crystal_measures_as_no_correction_needed() {
        let fit = fit_drift(&observations(120, 48_000.0)).expect("a line");
        assert!((fit.measured_rate - 48_000.0).abs() < 1.0e-4);
        assert!(fit.drift_ppm().abs() < 0.1, "ppm {}", fit.drift_ppm());
    }

    #[test]
    fn the_fit_reports_its_own_scatter() {
        let clean = fit_drift(&observations(300, 47_999.4)).expect("a line");
        assert!(clean.residual_rms_s < 1.0e-6, "{}", clean.residual_rms_s);

        // 2 ms of uniform jitter has an RMS of 2/sqrt(3) ~ 1.15 ms.
        let noisy = fit_drift(&jittered(300, 47_999.4, 0.002)).expect("a line");
        assert!(
            (0.8e-3..1.5e-3).contains(&noisy.residual_rms_s),
            "residual {} s",
            noisy.residual_rms_s
        );
    }

    #[test]
    fn jitter_averages_out_of_the_slope_over_a_long_take() {
        let fit = fit_drift(&jittered(600, 47_999.4, 0.001)).expect("a line");
        assert!(
            (fit.measured_rate - 47_999.4).abs() < 0.01,
            "measured {} Hz from jittered marks",
            fit.measured_rate
        );
    }

    #[test]
    fn a_non_nominal_device_rate_folds_conversion_into_the_same_ratio() {
        // A device that fell back to 44.1 kHz and is itself 20 ppm slow.
        let true_rate = 44_100.0 * (1.0 - 20.0e-6);
        let obs: Vec<_> = (0..120)
            .map(|k| {
                let sample_index = (k as u64) * 44_100;
                DriftObservation {
                    sample_index,
                    utc_unix_nanos: EPOCH
                        + (sample_index as f64 / true_rate * 1.0e9).round() as i128,
                }
            })
            .collect();
        let fit = fit_drift(&obs).expect("a line");
        assert!((fit.measured_rate - true_rate).abs() < 0.01);
        // The ratio is dominated by the rate conversion...
        assert!((fit.drift_ratio - 48_000.0 / true_rate).abs() < 1.0e-9);
        // ...but the crystal error against the device's own nominal rate is small.
        assert!(
            (fit.crystal_ppm(44_100) + 20.0).abs() < 0.1,
            "crystal {} ppm",
            fit.crystal_ppm(44_100)
        );
    }

    #[test]
    fn the_anchor_comes_off_the_whole_line_not_the_first_mark() {
        let mut obs = observations(120, 47_999.4);
        // Poison the first mark with 3 ms of error, as a congested exchange would.
        obs[0].utc_unix_nanos += 3_000_000;
        let fit = fit_drift(&obs).expect("a line");
        let err = (fit.t0_unix_nanos - EPOCH).unsigned_abs() as f64;
        assert!(
            err < 300_000.0,
            "one bad mark moved t0 by {err} ns; the line should have absorbed it"
        );
    }

    #[test]
    fn utc_at_an_arbitrary_sample_follows_the_fitted_rate() {
        let fit = fit_drift(&observations(120, 47_999.4)).expect("a line");
        // One nominal minute in, the true elapsed time is a shade over 60 s.
        let idx = 60 * 48_000;
        let want = EPOCH + (idx as f64 / 47_999.4 * 1.0e9).round() as i128;
        let got = fit.utc_nanos_at(idx);
        assert!((got - want).unsigned_abs() < 1_000, "got {got} want {want}");
    }

    // -- the fit: degenerate input -------------------------------------------

    #[test]
    fn no_observations_yields_no_fit() {
        assert!(fit_drift(&[]).is_none());
    }

    #[test]
    fn a_single_observation_yields_no_fit() {
        assert!(fit_drift(&observations(1, 48_000.0)).is_none());
    }

    #[test]
    fn a_degenerate_span_yields_no_fit() {
        // Every mark on the same sample: a vertical line, not a rate.
        let obs: Vec<_> = (0..50)
            .map(|k| DriftObservation {
                sample_index: 4_096,
                utc_unix_nanos: EPOCH + k as i128 * 1_000_000,
            })
            .collect();
        assert!(fit_drift(&obs).is_none());
    }

    #[test]
    fn time_running_backwards_yields_no_fit() {
        let obs: Vec<_> = (0..50)
            .map(|k| DriftObservation {
                sample_index: k as u64 * 48_000,
                utc_unix_nanos: EPOCH - k as i128 * 1_000_000_000,
            })
            .collect();
        assert!(fit_drift(&obs).is_none(), "a negative rate is not a rate");
    }

    #[test]
    fn two_observations_are_enough_to_define_a_line() {
        let fit = fit_drift(&observations(2, 47_999.4)).expect("two points define a line");
        assert_eq!(fit.points, 2);
        // Exactly determined, so there is no scatter to report.
        assert_eq!(fit.residual_rms_s, 0.0);
    }

    // -- the gate -------------------------------------------------------------

    #[test]
    fn a_clean_synced_take_passes_the_gate() {
        let obs = observations(120, 47_999.4);
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&synced_clock(), &obs, fit.as_ref(), 48_000);
        assert!(report.passed(), "{:?}", report.failures());
        assert_eq!(report.reason(), None);
    }

    #[test]
    fn refuses_to_delete_the_raw_when_the_clock_never_synced() {
        let obs = observations(120, 47_999.4);
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&clock_with(0), &obs, fit.as_ref(), 48_000);
        assert!(!report.passed());
        assert!(
            report
                .failures()
                .iter()
                .any(|f| matches!(f, GateFailure::ClockNotSynced { .. }))
        );
    }

    #[test]
    fn refuses_to_delete_the_raw_on_too_few_ntp_exchanges() {
        let obs = observations(120, 47_999.4);
        let fit = fit_drift(&obs);
        // Two accepted exchanges leaves the clock Coarse, which trips both checks.
        let report = evaluate_gate(&clock_with(2), &obs, fit.as_ref(), 48_000);
        assert!(!report.passed());
        assert!(report.failures().iter().any(|f| matches!(
            f,
            GateFailure::TooFewClockSamples {
                accepted: 2,
                required: 3
            }
        )));
    }

    #[test]
    fn exactly_the_minimum_ntp_exchanges_is_enough() {
        let obs = observations(120, 47_999.4);
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&clock_with(3), &obs, fit.as_ref(), 48_000);
        assert!(report.passed(), "{:?}", report.failures());
    }

    #[test]
    fn a_reference_that_counts_nothing_is_not_held_back_by_the_exchange_count() {
        // An ethersync leader generates the timeline it is being judged against.
        // There is no exchange to accumulate, so demanding three of them would
        // mean a leader could never correct a take at all.
        let obs = observations(120, 47_999.4);
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&uncounted_clock(true), &obs, fit.as_ref(), 48_000);
        assert!(report.passed(), "{:?}", report.failures());
    }

    #[test]
    fn a_reference_that_counts_nothing_is_still_held_to_being_synced() {
        // Skipping the exchange count must not become a way past the gate. A
        // follower that never locked is exactly as untrustworthy as an unsynced
        // NTP clock, and has to be reported as such.
        let obs = observations(120, 47_999.4);
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&uncounted_clock(false), &obs, fit.as_ref(), 48_000);
        assert!(!report.passed());
        assert!(
            report
                .failures()
                .iter()
                .any(|f| matches!(f, GateFailure::ClockNotSynced { .. }))
        );
        assert!(
            !report
                .failures()
                .iter()
                .any(|f| matches!(f, GateFailure::TooFewClockSamples { .. })),
            "there is no exchange count to complain about"
        );
    }

    #[test]
    fn refuses_to_delete_the_raw_on_too_few_observations() {
        let obs = observations(MIN_OBSERVATIONS - 1, 47_999.4);
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&synced_clock(), &obs, fit.as_ref(), 48_000);
        assert!(!report.passed());
        assert!(report.failures().iter().any(|f| matches!(
            f,
            GateFailure::TooFewObservations { got, required }
                if *got == MIN_OBSERVATIONS - 1 && *required == MIN_OBSERVATIONS
        )));
    }

    #[test]
    fn exactly_the_minimum_observations_is_enough() {
        let obs = observations(MIN_OBSERVATIONS, 47_999.4);
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&synced_clock(), &obs, fit.as_ref(), 48_000);
        assert!(report.passed(), "{:?}", report.failures());
    }

    #[test]
    fn refuses_a_drift_no_crystal_could_have() {
        // 1000 ppm slow: an order of magnitude past any real oscillator.
        let obs = observations(120, 48_000.0 * (1.0 - 1000.0e-6));
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&synced_clock(), &obs, fit.as_ref(), 48_000);
        assert!(!report.passed());
        assert!(
            report
                .failures()
                .iter()
                .any(|f| matches!(f, GateFailure::ImplausibleDrift { .. }))
        );
    }

    #[test]
    fn the_drift_limit_is_a_boundary_not_a_suggestion() {
        let inside = observations(120, 48_000.0 * (1.0 - 199.0e-6));
        let outside = observations(120, 48_000.0 * (1.0 - 201.0e-6));
        let clock = synced_clock();
        assert!(
            evaluate_gate(&clock, &inside, fit_drift(&inside).as_ref(), 48_000).passed(),
            "199 ppm is a plausible, if poor, crystal"
        );
        assert!(
            !evaluate_gate(&clock, &outside, fit_drift(&outside).as_ref(), 48_000).passed(),
            "201 ppm is a bad fit, not a bad crystal"
        );
    }

    #[test]
    fn refuses_to_delete_the_raw_when_the_fit_is_noisy() {
        // 20 ms of jitter: an NTP path nobody should be timestamping against.
        let obs = jittered(120, 47_999.4, 0.020);
        let fit = fit_drift(&obs).expect("a line");
        assert!(fit.residual_rms_s > MAX_RESIDUAL_RMS_S);
        let report = evaluate_gate(&synced_clock(), &obs, Some(&fit), 48_000);
        assert!(!report.passed());
        assert!(
            report
                .failures()
                .iter()
                .any(|f| matches!(f, GateFailure::NoisyFit { .. }))
        );
    }

    #[test]
    fn a_merely_imperfect_path_still_passes() {
        // 1 ms of jitter is an ordinary internet NTP server, not a fault.
        let obs = jittered(300, 47_999.4, 0.001);
        let fit = fit_drift(&obs).expect("a line");
        let report = evaluate_gate(&synced_clock(), &obs, Some(&fit), 48_000);
        assert!(report.passed(), "{:?}", report.failures());
    }

    #[test]
    fn refuses_when_there_is_no_fit_at_all() {
        let report = evaluate_gate(&synced_clock(), &[], None, 48_000);
        assert!(!report.passed());
        assert!(
            report
                .failures()
                .iter()
                .any(|f| matches!(f, GateFailure::NoUsableFit { observations: 0 }))
        );
    }

    #[test]
    fn refuses_observations_bunched_into_too_short_a_window() {
        // 40 marks, but all inside one second: enough points, no leverage.
        let obs: Vec<_> = (0..40)
            .map(|k| DriftObservation {
                sample_index: k as u64 * 1_200,
                utc_unix_nanos: EPOCH + (k as f64 * 1_200.0 / 47_999.4 * 1.0e9).round() as i128,
            })
            .collect();
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&synced_clock(), &obs, fit.as_ref(), 48_000);
        assert!(!report.passed());
        assert!(
            report
                .failures()
                .iter()
                .any(|f| matches!(f, GateFailure::SpanTooShort { .. }))
        );
    }

    #[test]
    fn every_reason_is_reported_not_just_the_first() {
        let obs = observations(5, 48_000.0 * (1.0 - 1000.0e-6));
        let fit = fit_drift(&obs);
        let report = evaluate_gate(&clock_with(0), &obs, fit.as_ref(), 48_000);
        assert!(report.failures().len() >= 3, "{:?}", report.failures());
        let reason = report.reason().expect("a failed gate has a reason");
        assert!(reason.contains("clock never synced"), "{reason}");
        assert!(reason.contains("drift observations"), "{reason}");
    }

    // -- resampling -----------------------------------------------------------

    #[test]
    fn resampling_stretches_the_clip_by_the_ratio() {
        let frames = 48_000;
        let input = tone(frames, 2);
        let ratio = 48_000.0 / 47_999.4;
        let out = resample_interleaved(&input, 2, ratio).expect("resampling");
        let out_frames = out.len() / 2;
        let want = (ratio * frames as f64).ceil() as usize;
        assert_eq!(out_frames, want);
        // A 12.5 ppm stretch of one second is 0.6 of a sample, so the count barely
        // moves; the point is that it moves in the right direction.
        assert!(out_frames >= frames);
    }

    #[test]
    fn resampling_preserves_the_waveform_to_within_a_sample_of_alignment() {
        let frames = 8_192;
        let input = tone(frames, 2);
        let out = resample_interleaved(&input, 2, 1.0 + 12.5e-6).expect("resampling");

        // Rubato trims `output_delay()` frames of startup silence, and that figure is
        // `taps * ratio / 2` truncated — a whole-frame estimate of a delay that is not
        // a whole number of frames. A fraction of a sample therefore survives the
        // trim, so the honest question is not "is the output aligned" but "how far
        // out is it, and is the waveform intact once you account for that". At 48 kHz
        // a sample is 21 us, an order of magnitude below the NTP dispersion the
        // timestamps already carry, so a sub-sample offset is not worth chasing — but
        // it is worth knowing about, and a *growing* one would mean the ratio itself
        // was wrong.
        //
        // The margin is a whole chunk: rubato's trim removes the leading silence but
        // the filter is still charging through the first chunk it emits, and that
        // transient — measured at ~0.03 full scale here — is not what this test is
        // about.
        let margin = RESAMPLE_CHUNK;
        let score = |shift: f64| {
            let mut worst = 0.0f32;
            for f in margin..(frames - margin) {
                let pos = f as f64 + shift;
                let i = pos.floor() as usize;
                let frac = (pos - pos.floor()) as f32;
                for c in 0..2 {
                    let b = out[i * 2 + c] * (1.0 - frac) + out[(i + 1) * 2 + c] * frac;
                    worst = worst.max((input[f * 2 + c] - b).abs());
                }
            }
            worst
        };
        // Coarse sweep, then refine around the winner.
        let search = |centre: f64, step: f64, n: i32| {
            (-n..=n)
                .map(|k| {
                    let s = centre + k as f64 * step;
                    (s, score(s))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .expect("a best alignment")
        };
        let (coarse, _) = search(0.0, 0.1, 20);
        let (best_shift, best) = search(coarse, 0.005, 20);

        assert!(
            best_shift.abs() <= 2.0,
            "residual delay of {best_shift} frames is more than a rounding error"
        );
        assert!(
            best < 0.002,
            "waveform differs by {best} once aligned at {best_shift} frames"
        );

        // Right is 0.75x left by construction, and that holds at any alignment; a
        // channel swap or a per-channel phase error would break it.
        for f in margin..(frames - margin) {
            let (l, r) = (out[f * 2], out[f * 2 + 1]);
            assert!(
                (r - l * 0.75).abs() < 0.005,
                "channels crossed at frame {f}: {l} vs {r}"
            );
        }
    }

    #[test]
    fn resampling_refuses_a_clip_too_short_to_trim_its_own_delay() {
        let input = tone(100, 2);
        let err = resample_interleaved(&input, 2, 1.0001).expect_err("should refuse");
        assert!(format!("{err:#}").contains("too short"), "{err:#}");
    }

    #[test]
    fn resampling_rejects_a_ragged_interleaved_buffer() {
        let input = vec![0.0f32; MIN_RESAMPLE_FRAMES * 2 + 1];
        assert!(resample_interleaved(&input, 2, 1.0001).is_err());
    }

    #[test]
    fn resampling_rejects_a_nonsense_ratio() {
        let input = tone(MIN_RESAMPLE_FRAMES, 1);
        assert!(resample_interleaved(&input, 1, 0.0).is_err());
        assert!(resample_interleaved(&input, 1, f64::NAN).is_err());
        assert!(resample_interleaved(&input, 1, -1.0).is_err());
    }

    // -- writing --------------------------------------------------------------

    #[test]
    fn over_full_scale_samples_clip_rather_than_flip_polarity() {
        let dir = scratch_dir("clip");
        let path = dir.join("clipped.wav");

        // What a hot input plus a resampler overshoot actually looks like.
        let audio = vec![1.0f32, 1.5, 2.0, -1.5, 0.5, -0.5, 0.999, -1.0];
        let mut p = provenance(1, 48_000);
        p.sample_rate = 48_000;
        p.channels = 1;
        p.bits_per_sample = 24;

        write_bwf(&path, &p, &audio).expect("writing");
        let (_, back) = read_back(&path);

        assert_eq!(back.len(), audio.len());
        for (wrote, got) in audio.iter().zip(&back) {
            if *wrote >= 1.0 {
                assert!(
                    *got > 0.99,
                    "wrote {wrote}, read back {got}: that is a polarity flip"
                );
            } else if *wrote <= -1.0 {
                assert!(*got < -0.99, "wrote {wrote}, read back {got}");
            } else {
                assert!(
                    (got - wrote).abs() < 1.0e-4,
                    "wrote {wrote}, read back {got}"
                );
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_resampled_buffer_that_overshoots_still_comes_back_clipped() {
        let dir = scratch_dir("overshoot");
        let path = dir.join("overshoot.wav");

        // A near-full-scale square edge: the sinc rings past 1.0 on both sides of it.
        let frames = MIN_RESAMPLE_FRAMES * 2;
        let input: Vec<f32> = (0..frames)
            .map(|i| if i % 64 < 32 { 0.995 } else { -0.995 })
            .collect();
        let resampled = resample_interleaved(&input, 1, 48_000.0 / 47_999.4).expect("resampling");
        assert!(
            resampled.iter().any(|s| s.abs() > 1.0),
            "the test signal was supposed to make the resampler overshoot"
        );

        let mut p = provenance(1, 48_000);
        p.bits_per_sample = 24;
        write_bwf(&path, &p, &resampled).expect("writing");
        let (_, back) = read_back(&path);

        // Every overshoot must have landed at full scale with its sign intact.
        for (i, (wrote, got)) in resampled.iter().zip(&back).enumerate() {
            if *wrote > 1.0 {
                assert!(*got > 0.99, "frame {i}: wrote {wrote}, read back {got}");
            } else if *wrote < -1.0 {
                assert!(*got < -0.99, "frame {i}: wrote {wrote}, read back {got}");
            }
            assert!(
                got.signum() == wrote.signum() || wrote.abs() < 1.0e-4,
                "frame {i}: sign flipped, wrote {wrote}, read back {got}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_written_file_carries_bext_and_ixml_ahead_of_the_audio() {
        let dir = scratch_dir("meta");
        let path = dir.join("meta.wav");
        let mut p = provenance(2, 48_000);
        p.bits_per_sample = 24;
        p.measured_rate = Some(47_999.4);
        p.drift_ratio = Some(48_000.0 / 47_999.4);
        p.resampled = true;

        write_bwf(&path, &p, &tone(1000, 2)).expect("writing");

        let mut reader = WaveReader::open(&path).expect("reopening");
        let bext = reader
            .broadcast_extension()
            .expect("reading bext")
            .expect("bext must be present");
        assert!(bext.coding_history.contains("measured_rate:47999.4000"));
        let mut ixml = Vec::new();
        reader.read_ixml(&mut ixml).expect("reading iXML");
        let ixml = String::from_utf8(ixml).expect("iXML is UTF-8");
        assert!(ixml.contains("<RESAMPLED>true</RESAMPLED>"), "{ixml}");

        // Both chunks precede `data`, which is what readers expect.
        let bytes = std::fs::read(&path).expect("rereading the file");
        let find = |needle: &[u8]| {
            bytes
                .windows(needle.len())
                .position(|w| w == needle)
                .unwrap_or_else(|| panic!("{:?} not found", std::str::from_utf8(needle)))
        };
        assert!(find(b"bext") < find(b"data"));
        assert!(find(b"iXML") < find(b"data"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // -- the whole job --------------------------------------------------------

    #[test]
    fn a_good_take_ships_corrected_and_the_raw_is_deleted() {
        let dir = scratch_dir("good");
        let raw = dir.join("rec-1.raw.wav");
        let out = dir.join("rec-1.wav");

        let frames = 48_000;
        write_raw_f32(&raw, 48_000, 2, &tone(frames, 2));

        let obs = observations(120, 47_999.4);
        let outcome = finalize(&raw, &out, &obs, &synced_clock(), &provenance(2, 48_000))
            .expect("finalising");

        assert!(outcome.gate.passed(), "gate: {:?}", outcome.gate.failures());
        assert!(outcome.corrected());
        assert!(!raw.exists(), "the raw take should have been deleted");
        assert!(out.exists());
        assert_eq!(outcome.raw_kept, None);

        assert_eq!(outcome.provenance.sample_rate, 48_000);
        assert!(outcome.provenance.resampled);
        let measured = outcome.provenance.measured_rate.expect("a measured rate");
        assert!((measured - 47_999.4).abs() < 0.01, "measured {measured}");

        let (fmt, audio) = read_back(&out);
        assert_eq!(fmt.sample_rate, 48_000);
        assert_eq!(fmt.channel_count, 2);
        assert_eq!(fmt.bits_per_sample, 24);
        assert_eq!(audio.len() / 2, outcome.frames_written as usize);
        // A 12.5 ppm stretch, so the length moves by well under a millisecond.
        let stretched = outcome.frames_written as i64 - frames as i64;
        assert!((0..=2).contains(&stretched), "grew by {stretched} frames");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_gate_keeps_the_raw_and_ships_unresampled_audio() {
        let dir = scratch_dir("ungated");
        let raw = dir.join("rec-2.raw.wav");
        let out = dir.join("rec-2.wav");

        let frames = 48_000;
        write_raw_f32(&raw, 48_000, 2, &tone(frames, 2));

        // Only five observations: nowhere near enough.
        let obs = observations(5, 47_999.4);
        let outcome = finalize(&raw, &out, &obs, &synced_clock(), &provenance(2, 48_000))
            .expect("finalising");

        assert!(!outcome.gate.passed());
        assert!(!outcome.resampled);
        assert!(!outcome.raw_deleted);
        assert!(raw.exists(), "the raw take must survive a failed gate");
        assert_eq!(outcome.raw_kept.as_deref(), Some(raw.as_path()));
        assert_eq!(outcome.frames_written, frames as u64);

        // The fit is still reported, so the UI can show what was rejected.
        assert!(outcome.fit.is_some());
        assert!(!outcome.provenance.resampled);

        let (fmt, _) = read_back(&out);
        assert_eq!(
            fmt.sample_rate, 48_000,
            "the device's own rate, uncorrected"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_uncorrected_non_48k_take_is_labelled_with_the_rate_it_really_has() {
        let dir = scratch_dir("44k");
        let raw = dir.join("rec-3.raw.wav");
        let out = dir.join("rec-3.wav");
        write_raw_f32(&raw, 44_100, 1, &tone(44_100, 1));

        // No observations at all, so nothing can be corrected.
        let outcome =
            finalize(&raw, &out, &[], &synced_clock(), &provenance(1, 44_100)).expect("finalising");

        assert!(!outcome.resampled);
        assert_eq!(
            outcome.provenance.sample_rate, 44_100,
            "claiming 48000 would make the sample count lie about time"
        );
        let (fmt, _) = read_back(&out);
        assert_eq!(fmt.sample_rate, 44_100);
        assert!(raw.exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_take_with_no_clock_is_still_shipped_rather_than_lost() {
        let dir = scratch_dir("noclock");
        let raw = dir.join("rec-4.raw.wav");
        let out = dir.join("rec-4.wav");
        write_raw_f32(&raw, 48_000, 2, &tone(12_000, 2));

        let outcome = finalize(
            &raw,
            &out,
            &observations(120, 47_999.4),
            &clock_with(0),
            &provenance(2, 48_000),
        )
        .expect("finalising");

        assert!(!outcome.gate.passed());
        assert!(out.exists(), "audio is never thrown away over a bad clock");
        assert!(raw.exists());
        assert_eq!(outcome.frames_written, 12_000);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_raw_take_is_an_error_not_a_silent_empty_file() {
        let dir = scratch_dir("missing");
        let raw = dir.join("nope.raw.wav");
        let out = dir.join("nope.wav");
        let err = finalize(&raw, &out, &[], &synced_clock(), &provenance(2, 48_000))
            .expect_err("should fail");
        assert!(format!("{err:#}").contains("opening raw take"), "{err:#}");
        assert!(!out.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn multichannel_takes_survive_the_round_trip() {
        let dir = scratch_dir("multi");
        let raw = dir.join("rec-5.raw.wav");
        let out = dir.join("rec-5.wav");
        write_raw_f32(&raw, 48_000, 4, &tone(24_000, 4));

        let outcome = finalize(
            &raw,
            &out,
            &observations(120, 47_999.4),
            &synced_clock(),
            &provenance(4, 48_000),
        )
        .expect("finalising");

        assert!(outcome.corrected(), "gate: {:?}", outcome.gate.failures());
        let (fmt, audio) = read_back(&out);
        assert_eq!(fmt.channel_count, 4);
        assert_eq!(audio.len() % 4, 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
