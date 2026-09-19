//! The input level meter: what the operator actually stares at while setting gain.
//!
//! Three decisions drive everything below.
//!
//! **The scale is dBFS, not linear.** Halving a signal's amplitude moves it 6 dB,
//! but only halves a linear bar; by the time a linear meter looks "low" the signal
//! is already 40 dB down and the noise floor is eating the take. Ear and console
//! both work in decibels, so the meter does too. The mapping is the shared
//! [`dbfs_to_fraction`], which spans [`FLOOR_DBFS`] at the left edge of the track to
//! 0 dBFS at the right edge, so the whole application agrees on where a level sits.
//!
//! **Peaks are held, then released at a fixed rate.** A digital peak lasts one
//! buffer — a few milliseconds — which is far below the ~100 ms it takes a human to
//! notice a flash. Without a hold, the transient that clipped the take is invisible.
//! So the peak marker jumps instantly (a meter that lags upward lies about
//! headroom), dwells for [`PEAK_HOLD_SECS`], and then falls at a constant
//! [`PEAK_FALL_DB_PER_SEC`]. Constant *dB* per second, not pixels per second, so the
//! release looks identical wherever it happens on the scale.
//!
//! **The colour zones are about headroom, not aesthetics.** Green ends at
//! [`CAUTION_DBFS`] (-18 dBFS), the SMPTE/EBU alignment level and the level a
//! well-set recording sits at. Between there and [`DANGER_DBFS`] you are spending
//! headroom, which is fine but worth watching. The last 3 dB are red because a
//! sampled peak is a floor, not a ceiling: the reconstructed waveform overshoots
//! between samples, so a meter reading -1 dBFS can already be clipping the
//! converter. Clipping itself latches — see [`MeterData::clipped`] — because the one
//! sample that went over happened while the operator was looking at the talent.
//!
//! The bars are horizontal: level grows left to right, channels stack downward as
//! rows. The scale is the thing that has to be readable, and a window is far wider
//! than the strip of height a meter can claim, so spending the long axis on dB is
//! what makes the crowded top 18 dB — where every gain decision is made — legible.
//! Stacking channels downward also matches how they are listed everywhere else in
//! the app, and a row is a shape a channel *name* could eventually sit in.
//!
//! Rows are capped at [`MAX_ROW_HEIGHT`] and top-aligned rather than sharing out all
//! the height available. A single mono channel stretched to fill the panel is a
//! solid slab with no discernible level; the eye reads a bar's *length*, and past
//! about 30 px of thickness the extra pixels only make it harder to see where the
//! bar ends. Parents should ask [`preferred_height`] for a height instead of
//! guessing one.

use iced::alignment;
// `canvas::Text` aligns horizontally with the text crate's own `Alignment` (which
// has a `Justified` variant) rather than the layout `Alignment` re-exported at the
// root of `iced`, so it has to be named through the text widget module.
use iced::time::Instant;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Stroke, Text};
use iced::widget::text::Alignment as TextAlignment;
use iced::{Color, Length, Pixels, Point, Rectangle, Renderer, Size, Theme, mouse, window};

use crate::audio::meters::{FLOOR_DBFS, dbfs_to_fraction, to_dbfs};

/// How long the peak marker sits at its maximum before it starts to fall.
///
/// Long enough to survive a glance away from the screen, short enough that a stale
/// peak is not still on display when the next loud moment arrives.
pub const PEAK_HOLD_SECS: f32 = 1.5;

/// Release rate of the peak marker once the hold expires, in dB per second.
///
/// Borrowed from the IEC 60268-10 PPM family: fast enough to track a performance,
/// slow enough that the eye can follow the marker down rather than losing it.
pub const PEAK_FALL_DB_PER_SEC: f32 = 20.0;

/// Top of the green zone. Alignment level: a take that lives here has ~18 dB of
/// headroom for the transient nobody warned you about.
pub const CAUTION_DBFS: f32 = -18.0;

/// Top of the amber zone. Above this, inter-sample overshoot means the converter may
/// already be clipping even though no sample has reached full scale.
pub const DANGER_DBFS: f32 = -3.0;

/// Gridlines, loudest first. Dense near 0 where gain decisions are made, sparse at
/// the quiet end where the only question is "is there signal at all?".
pub const TICKS_DBFS: [f32; 8] = [0.0, -6.0, -12.0, -18.0, -24.0, -36.0, -48.0, -60.0];

/// The height one channel row wants, and therefore the unit [`preferred_height`]
/// budgets with. Comfortably readable without making an 8-channel meter tall enough
/// to crowd the transport out of the window.
pub const ROW_HEIGHT: f32 = 24.0;

/// The most height a single row will take when the parent is generous.
///
/// The cap, not the row count, is what stops a mono meter becoming a slab: past this
/// the bar stops reading as a bar and starts reading as a filled panel.
pub const MAX_ROW_HEIGHT: f32 = 30.0;

/// A frame longer than this is a stall — a dragged window, a sleeping laptop — not
/// elapsed musical time. Clamping stops the peak marker teleporting to the floor.
const MAX_FRAME_SECS: f32 = 0.5;

/// Width of the left-hand gutter that carries the channel numbers.
const CHANNEL_LABEL_WIDTH: f32 = 22.0;
/// Width of the per-row clip indicator down the right-hand edge.
const CLIP_STRIP_WIDTH: f32 = 7.0;
/// Height of the dB scale strip along the bottom, under the last row.
const LABEL_STRIP_HEIGHT: f32 = 13.0;
/// Breathing room between the strips and the track.
const STRIP_GAP: f32 = 3.0;
/// Vertical gap between channel rows.
const ROW_GAP: f32 = 3.0;
/// Text size for scale and channel labels.
const LABEL_SIZE: f32 = 10.0;
/// Minimum horizontal gap between two labelled ticks. Sized against the *width* of
/// the widest label ("-60" at [`LABEL_SIZE`]) plus enough air to keep neighbouring
/// numbers from reading as one.
const MIN_LABEL_SPACING: f32 = 26.0;
/// Thickness of the peak marker.
const PEAK_TICK_WIDTH: f32 = 2.0;

/// One frame's worth of levels, borrowed from the caller's scratch buffers.
///
/// Borrowed rather than owned because the caller already reuses `Vec`s across frames
/// (see `Meters::take_peaks` and `Meters::rms`) and a meter should not allocate 60
/// times a second to say nothing has changed.
///
/// All three slices are indexed by channel. They are read defensively: a short or
/// empty `clipped` simply means "no channel has clipped", which is the right answer
/// while the device is being reconfigured and the slices disagree for a frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeterData<'a> {
    /// Per-channel peak magnitude, linear, as taken from `Meters::take_peaks`.
    pub peak: &'a [f32],
    /// Per-channel RMS magnitude, linear, as taken from `Meters::rms`.
    pub rms: &'a [f32],
    /// Per-channel latching clip flags, from `Meters::clipped`.
    pub clipped: &'a [bool],
}

impl<'a> MeterData<'a> {
    pub fn new(peak: &'a [f32], rms: &'a [f32], clipped: &'a [bool]) -> Self {
        Self { peak, rms, clipped }
    }

    /// How many bars to draw.
    ///
    /// The larger of the two level slices, so a channel is never silently dropped
    /// because one of the buffers was refilled a frame later than the other.
    pub fn channels(&self) -> usize {
        self.peak.len().max(self.rms.len())
    }

    fn peak_dbfs(&self, ch: usize) -> f32 {
        to_dbfs(self.peak.get(ch).copied().unwrap_or(0.0))
    }

    fn rms_dbfs(&self, ch: usize) -> f32 {
        to_dbfs(self.rms.get(ch).copied().unwrap_or(0.0))
    }

    fn clipped(&self, ch: usize) -> bool {
        self.clipped.get(ch).copied().unwrap_or(false)
    }
}

/// The height the meter wants for `channels` channels, in logical pixels.
///
/// Exported so the parent sizes the panel from the same constants the layout uses,
/// instead of a magic number that silently stops matching the moment a strip
/// changes height. Zero channels still asks for one row: the empty, scaled track is
/// what the UI shows between picking a device and the stream starting, and the panel
/// should not visibly resize when audio arrives.
pub fn preferred_height(channels: usize) -> f32 {
    let rows = channels.max(1);

    STRIP_GAP
        + rows as f32 * ROW_HEIGHT
        + (rows - 1) as f32 * ROW_GAP
        + STRIP_GAP
        + LABEL_STRIP_HEIGHT
}

/// Which colour a level reads as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// At or below alignment level. Plenty of headroom.
    Safe,
    /// Into the headroom. Legitimate, but the operator should be watching.
    Caution,
    /// Close enough to full scale that the converter may already be clipping.
    Danger,
}

/// The zone a dBFS level falls in.
pub fn zone_for(dbfs: f32) -> Zone {
    if dbfs >= DANGER_DBFS {
        Zone::Danger
    } else if dbfs >= CAUTION_DBFS {
        Zone::Caution
    } else {
        Zone::Safe
    }
}

/// The x coordinate, in the track's own space, of a given level.
///
/// No inversion here, unlike the vertical meter this replaced: canvas x and level
/// both grow to the right. Levels outside the scale clamp to the ends via
/// [`dbfs_to_fraction`], so a +6 dBFS peak pins to the right edge rather than
/// drawing off the widget.
fn x_for_dbfs(dbfs: f32, track: Rectangle) -> f32 {
    track.x + track.width * dbfs_to_fraction(dbfs)
}

/// How tall each row is when `channels` of them share `available` height.
///
/// Capped at [`MAX_ROW_HEIGHT`] so surplus height is left empty rather than inflating
/// the bars, and floored at one pixel so a row that has been squeezed is still a
/// visible line instead of nothing at all.
fn row_height(available: f32, channels: usize) -> f32 {
    let rows = channels.max(1);
    let total_gap = ROW_GAP * (rows - 1) as f32;

    ((available - total_gap) / rows as f32).clamp(1.0, MAX_ROW_HEIGHT)
}

/// The row rectangles for `channels` bars inside `area`, top-aligned.
///
/// Returns an empty `Vec` for zero channels, which is a real state: it is what the
/// UI shows between selecting a device and the stream starting.
fn rows(area: Rectangle, channels: usize) -> Vec<Rectangle> {
    if channels == 0 || area.width <= 0.0 || area.height <= 0.0 {
        return Vec::new();
    }

    let height = row_height(area.height, channels);

    (0..channels)
        .map(|i| Rectangle {
            x: area.x,
            y: area.y + i as f32 * (height + ROW_GAP),
            width: area.width,
            height,
        })
        .collect()
}

/// Where every piece of the meter goes, resolved once per frame from the widget size.
///
/// Computed as a value rather than inline in `draw` because the geometry is the part
/// most likely to be wrong and the part a test can actually check.
#[derive(Debug, Clone, PartialEq)]
struct Layout {
    /// The scaled region: background, gridlines and border. Exactly as tall as the
    /// rows that exist, so leftover height stays empty instead of being framed.
    track: Rectangle,
    /// One rectangle per channel, top to bottom.
    rows: Vec<Rectangle>,
    /// Top edge of the dB scale strip, which follows the stack rather than sitting at
    /// the bottom of the widget: labels far below the last row read as unrelated.
    labels_y: f32,
}

fn layout(size: Size, channels: usize) -> Layout {
    let x = CHANNEL_LABEL_WIDTH;
    let width = (size.width - x - STRIP_GAP - CLIP_STRIP_WIDTH).max(0.0);
    let y = STRIP_GAP;
    let available = (size.height - y - STRIP_GAP - LABEL_STRIP_HEIGHT).max(0.0);

    let area = Rectangle {
        x,
        y,
        width,
        height: available,
    };
    let rows = rows(area, channels);

    // With no channels the track is still drawn, one row tall: a bordered, scaled,
    // empty meter reads as "no input yet", whereas a blank rectangle reads as broken.
    let used = rows
        .last()
        .map_or(row_height(available, 0), |last| {
            last.y + last.height - area.y
        })
        .min(available);

    let track = Rectangle {
        height: used,
        ..area
    };

    Layout {
        track,
        rows,
        labels_y: track.y + track.height + STRIP_GAP,
    }
}

/// Which ticks get a text label at a given track width.
///
/// Every tick always gets a gridline; only the labels are thinned, and only when
/// they would overlap. 0 dBFS is labelled first and unconditionally because it is
/// the reference every other reading is relative to — and because it anchors the
/// right-hand end of the scale, the walk proceeds leftwards from there.
fn labelled_ticks(track_width: f32) -> Vec<f32> {
    let mut labelled = Vec::new();
    let mut last_x = f32::INFINITY;

    for db in TICKS_DBFS {
        let x = track_width * dbfs_to_fraction(db);
        if labelled.is_empty() || last_x - x >= MIN_LABEL_SPACING {
            labelled.push(db);
            last_x = x;
        }
    }

    labelled
}

/// One channel's peak marker: where it is and how much dwell it has left.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Hold {
    dbfs: f32,
    dwell_secs: f32,
}

impl Hold {
    const IDLE: Self = Self {
        dbfs: FLOOR_DBFS,
        dwell_secs: 0.0,
    };

    /// Advance the marker by `dt` seconds against a freshly measured peak.
    ///
    /// Pure, because this is the only part of the widget worth testing and it is far
    /// easier to reason about as a value transformation than as mutation buried in
    /// an event handler.
    fn advance(self, peak_dbfs: f32, dt: f32) -> Self {
        // Attack is instantaneous: a meter that eases upward under-reports headroom,
        // which is the one error that costs you the take.
        if peak_dbfs >= self.dbfs {
            return Self {
                dbfs: peak_dbfs,
                dwell_secs: PEAK_HOLD_SECS,
            };
        }

        // Split the frame at the moment the dwell expires. Without this, a frame
        // longer than the remaining dwell would both hold *and* fall for its whole
        // duration, so the release rate would depend on the frame rate.
        let falling_secs = (dt - self.dwell_secs).max(0.0);

        Self {
            // Never fall below the live signal: the marker is a maximum, so it must
            // sit at or beyond the end of the bar it decorates.
            dbfs: (self.dbfs - falling_secs * PEAK_FALL_DB_PER_SEC).max(peak_dbfs),
            dwell_secs: (self.dwell_secs - dt).max(0.0),
        }
    }

    fn is_active(self) -> bool {
        self.dbfs > FLOOR_DBFS
    }
}

/// Everything the meter remembers between frames.
///
/// Owned by the canvas widget tree (see `canvas::Program::State`), which is the only
/// place iced lets a widget keep mutable state, and the reason the peak hold lives
/// here rather than in [`MeterData`].
#[derive(Debug, Default)]
pub struct MeterState {
    holds: Vec<Hold>,
    /// The timestamp of the previous redraw, which is how we know `dt`.
    last_frame: Option<Instant>,
}

impl MeterState {
    /// Fold one frame's peaks into the held markers.
    fn advance(&mut self, data: MeterData<'_>, dt: f32) {
        // Channel count changes when the operator picks a different device; growing
        // starts the new channels at the floor rather than at another channel's peak.
        self.holds.resize(data.channels(), Hold::IDLE);

        for (ch, hold) in self.holds.iter_mut().enumerate() {
            *hold = hold.advance(data.peak_dbfs(ch), dt);
        }
    }

    /// Whether any marker still has somewhere to fall.
    ///
    /// When nothing is moving we stop asking for frames, so an idle recorder sitting
    /// in silence does not spin the GPU.
    fn is_animating(&self) -> bool {
        self.holds.iter().any(|h| h.is_active())
    }

    fn hold_dbfs(&self, ch: usize) -> f32 {
        self.holds.get(ch).map_or(FLOOR_DBFS, |h| h.dbfs)
    }
}

/// The colours the meter draws with, resolved from the active [`Theme`].
///
/// Pulled from the palette rather than hardcoded so the meter stays legible on light
/// and dark themes alike; only the alpha scaling is ours, and that is applied to
/// palette colours so it follows the theme too.
struct Palette {
    track: Color,
    border: Color,
    grid: Color,
    text: Color,
    safe: Color,
    caution: Color,
    danger: Color,
    led_off: Color,
}

impl Palette {
    fn resolve(theme: &Theme) -> Self {
        let p = theme.extended_palette();

        Self {
            track: p.background.weak.color,
            border: p.background.strong.color,
            // Gridlines have to read over both the empty track and a lit bar, so they
            // are the text colour knocked back rather than a fixed grey that would
            // vanish against one or the other.
            grid: p.background.base.text.scale_alpha(0.35),
            text: p.background.base.text,
            safe: p.success.base.color,
            caution: p.warning.base.color,
            danger: p.danger.base.color,
            led_off: p.background.strong.color,
        }
    }

    fn zone_color(&self, zone: Zone) -> Color {
        match zone {
            Zone::Safe => self.safe,
            Zone::Caution => self.caution,
            Zone::Danger => self.danger,
        }
    }
}

/// The multi-channel input meter.
///
/// Deliberately not generic over `Message`: it is a read-only display and emits
/// nothing. The `Program` impl is generic instead, so the widget slots into any
/// parent's message type without the caller having to name one or the struct having
/// to carry a `PhantomData` for a message it will never send.
#[derive(Debug, Clone, Copy)]
pub struct Meter<'a> {
    data: MeterData<'a>,
}

impl<'a> Meter<'a> {
    pub fn new(data: MeterData<'a>) -> Self {
        Self { data }
    }

    /// Draw one channel's RMS bar, coloured by the zones it passes through.
    ///
    /// The bar is segmented rather than tinted as a whole: colouring the entire bar
    /// by its loudest value makes a quiet moment after a loud one flash the whole row
    /// green, which reads as a level change that did not happen.
    fn draw_bar(
        &self,
        frame: &mut Frame,
        row: Rectangle,
        track: Rectangle,
        top_dbfs: f32,
        palette: &Palette,
    ) {
        let segments = [
            (FLOOR_DBFS, CAUTION_DBFS, Zone::Safe),
            (CAUTION_DBFS, DANGER_DBFS, Zone::Caution),
            (DANGER_DBFS, 0.0, Zone::Danger),
        ];

        for (from, to, zone) in segments {
            let to = to.min(top_dbfs);
            if to <= from {
                continue;
            }

            let left = x_for_dbfs(from, track);
            let right = x_for_dbfs(to, track);

            frame.fill_rectangle(
                Point::new(left, row.y),
                Size::new(right - left, row.height),
                palette.zone_color(zone),
            );
        }
    }
}

impl<Message> canvas::Program<Message> for Meter<'_> {
    type State = MeterState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        // `RedrawRequested` carries the frame's timestamp, which is the only elapsed
        // time iced offers a canvas. Taking it from the event rather than calling
        // `Instant::now()` keeps the decay tied to the frame the user will actually
        // see, not to whenever this code happened to run.
        let iced::Event::Window(window::Event::RedrawRequested(now)) = event else {
            return None;
        };

        let dt = state
            .last_frame
            .map_or(0.0, |prev| (*now - prev).as_secs_f32())
            .min(MAX_FRAME_SECS);
        state.last_frame = Some(*now);

        // `self` is the program built by the previous `view`, so the peaks folded in
        // here are one frame old. At 60 Hz that is 16 ms of lag on a marker that
        // dwells for 1.5 s, which is not visible; it is called out because it is the
        // kind of thing that looks like a bug later.
        state.advance(self.data, dt);

        // Keep frames coming while anything is still falling, and only then.
        state.is_animating().then(canvas::Action::request_redraw)
    }

    fn draw(
        &self,
        state: &Self::State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let palette = Palette::resolve(theme);
        let mut frame = Frame::new(renderer, bounds.size());

        let Layout {
            track,
            rows,
            labels_y,
        } = layout(bounds.size(), self.data.channels());

        if track.width <= 0.0 || track.height <= 0.0 {
            return vec![frame.into_geometry()];
        }

        frame.fill_rectangle(
            Point::new(track.x, track.y),
            Size::new(track.width, track.height),
            palette.track,
        );

        for (ch, row) in rows.iter().enumerate() {
            let rms = self.data.rms_dbfs(ch);
            self.draw_bar(&mut frame, *row, track, rms, &palette);

            // The peak marker rides at the leading end of the bar. Its colour comes
            // from its own level, not the bar's, because a transient into the red is
            // exactly the thing the operator needs to see while the RMS still looks
            // fine.
            let hold = state.hold_dbfs(ch);
            if hold > FLOOR_DBFS {
                let x = x_for_dbfs(hold, track);
                // Pull it in at the very right so a 0 dBFS marker stays inside the
                // track instead of straddling the border.
                let x = x.min(track.x + track.width - PEAK_TICK_WIDTH).max(track.x);
                frame.fill_rectangle(
                    Point::new(x, row.y),
                    Size::new(PEAK_TICK_WIDTH, row.height),
                    palette.zone_color(zone_for(hold)),
                );
            }

            // Clip indicator, latched upstream. It sits past the 0 dBFS end of its own
            // row, which is where the eye already is when a level is in trouble, and
            // is drawn unlit rather than hidden so its position is familiar before it
            // ever matters.
            frame.fill_rectangle(
                Point::new(track.x + track.width + STRIP_GAP, row.y),
                Size::new(CLIP_STRIP_WIDTH, row.height),
                if self.data.clipped(ch) {
                    palette.danger
                } else {
                    palette.led_off
                },
            );

            // Channel numbers are 1-based: that is how the patchbay, the console and
            // the file's channel list are all numbered.
            if row.height >= LABEL_SIZE {
                frame.fill_text(Text {
                    content: format!("{}", ch + 1),
                    position: Point::new(
                        CHANNEL_LABEL_WIDTH - STRIP_GAP * 2.0,
                        row.y + row.height / 2.0,
                    ),
                    color: palette.text,
                    size: Pixels(LABEL_SIZE),
                    align_x: TextAlignment::Right,
                    align_y: alignment::Vertical::Center,
                    ..Text::default()
                });
            }
        }

        // Gridlines go on top of the bars: a scale you cannot read against the signal
        // is decoration. Knocked-back alpha keeps them from cutting the bars up.
        let labels = labelled_ticks(track.width);
        for db in TICKS_DBFS {
            let x = x_for_dbfs(db, track);
            // 0 dBFS is the clipping line, so it gets the danger colour at full
            // strength: it is a limit, not a gradation.
            let color = if db >= 0.0 {
                palette.danger
            } else {
                palette.grid
            };

            frame.fill_rectangle(
                Point::new(
                    (x - 0.5).clamp(track.x, track.x + track.width - 1.0),
                    track.y,
                ),
                Size::new(1.0, track.height),
                color,
            );

            if labels.contains(&db) {
                frame.fill_text(Text {
                    content: format!("{db:.0}"),
                    position: Point::new(x, labels_y),
                    color: palette.text,
                    size: Pixels(LABEL_SIZE),
                    align_x: TextAlignment::Center,
                    align_y: alignment::Vertical::Top,
                    ..Text::default()
                });
            }
        }

        frame.stroke_rectangle(
            Point::new(track.x, track.y),
            Size::new(track.width, track.height),
            Stroke::default().with_color(palette.border).with_width(1.0),
        );

        vec![frame.into_geometry()]
    }
}

/// The meter as a widget, ready to drop into a `view`.
///
/// Returns the `Canvas` rather than an `Element` so the caller can still size it; it
/// fills whatever it is given by default, and [`preferred_height`] says what that
/// height should be.
pub fn meter<'a, Message>(data: MeterData<'a>) -> Canvas<Meter<'a>, Message> {
    Canvas::new(Meter::new(data))
        .width(Length::Fill)
        .height(Length::Fill)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A track 720 px wide makes the arithmetic exact: 72 dB of scale, 10 px per dB.
    const TRACK: Rectangle = Rectangle {
        x: 0.0,
        y: 0.0,
        width: 720.0,
        height: 100.0,
    };

    /// A realistic window-width panel: wide, and only as tall as the meter asked for.
    fn panel(channels: usize) -> Size {
        Size::new(900.0, preferred_height(channels))
    }

    #[test]
    fn a_new_peak_snaps_up_instantly_and_restarts_the_dwell() {
        let held = Hold::IDLE.advance(-10.0, 0.016);
        assert_eq!(held.dbfs, -10.0, "attack must be instantaneous");
        assert_eq!(held.dwell_secs, PEAK_HOLD_SECS);
    }

    #[test]
    fn peak_hold_does_not_move_during_the_dwell() {
        let mut held = Hold::IDLE.advance(-10.0, 0.0);

        // Step to just short of the full hold time.
        let mut elapsed = 0.0;
        while elapsed < PEAK_HOLD_SECS - 0.1 {
            held = held.advance(FLOOR_DBFS, 0.05);
            elapsed += 0.05;
        }

        assert_eq!(held.dbfs, -10.0, "the marker must not fall while dwelling");
        assert!(held.dwell_secs > 0.0);
    }

    #[test]
    fn peak_hold_decays_at_the_documented_rate() {
        let held = Hold::IDLE.advance(-10.0, 0.0);

        // One second past the end of the dwell.
        let after = held.advance(FLOOR_DBFS, PEAK_HOLD_SECS + 1.0);

        let expected = -10.0 - PEAK_FALL_DB_PER_SEC;
        assert!(
            (after.dbfs - expected).abs() < 1e-4,
            "expected {expected}, got {}",
            after.dbfs
        );
    }

    #[test]
    fn decay_is_independent_of_how_the_frames_are_chopped_up() {
        // The release rate must be a property of the meter, not of the frame rate.
        let coarse = Hold::IDLE
            .advance(-10.0, 0.0)
            .advance(FLOOR_DBFS, PEAK_HOLD_SECS + 1.0);

        let mut fine = Hold::IDLE.advance(-10.0, 0.0);
        let steps = ((PEAK_HOLD_SECS + 1.0) / 0.01).round() as usize;
        for _ in 0..steps {
            fine = fine.advance(FLOOR_DBFS, 0.01);
        }

        assert!(
            (coarse.dbfs - fine.dbfs).abs() < 0.05,
            "coarse {} vs fine {}",
            coarse.dbfs,
            fine.dbfs
        );
    }

    #[test]
    fn a_frame_straddling_the_dwell_boundary_only_falls_for_the_remainder() {
        // 0.5 s of dwell left, a 1.5 s frame: exactly 1.0 s of fall, not 1.5 s.
        let held = Hold {
            dbfs: -20.0,
            dwell_secs: 0.5,
        };
        let after = held.advance(FLOOR_DBFS, 1.5);

        let expected = -20.0 - PEAK_FALL_DB_PER_SEC;
        assert!(
            (after.dbfs - expected).abs() < 1e-4,
            "expected {expected}, got {}",
            after.dbfs
        );
        assert_eq!(after.dwell_secs, 0.0);
    }

    #[test]
    fn peak_hold_never_falls_below_the_live_peak() {
        let held = Hold {
            dbfs: -20.0,
            dwell_secs: 0.0,
        };
        // Enough time to fall 200 dB, but the signal is sitting at -30.
        let after = held.advance(-30.0, 10.0);
        assert_eq!(
            after.dbfs, -30.0,
            "the marker is a maximum, never a minimum"
        );
    }

    #[test]
    fn peak_hold_settles_at_the_floor_and_stops_animating() {
        let mut held = Hold::IDLE.advance(-6.0, 0.0);
        for _ in 0..200 {
            held = held.advance(FLOOR_DBFS, 0.1);
        }
        assert_eq!(held.dbfs, FLOOR_DBFS);
        assert!(
            !held.is_active(),
            "a floored marker must not request frames"
        );
    }

    #[test]
    fn full_scale_is_the_right_edge_of_the_track_and_the_floor_is_the_left() {
        assert!((x_for_dbfs(0.0, TRACK) - (TRACK.x + TRACK.width)).abs() < 1e-4);
        assert!((x_for_dbfs(FLOOR_DBFS, TRACK) - TRACK.x).abs() < 1e-4);
    }

    #[test]
    fn six_db_is_the_same_distance_anywhere_on_the_scale() {
        // The whole point of a dB scale: equal ratios take equal space.
        let near_full_scale = x_for_dbfs(0.0, TRACK) - x_for_dbfs(-6.0, TRACK);
        let near_the_floor = x_for_dbfs(-60.0, TRACK) - x_for_dbfs(-66.0, TRACK);
        assert!(
            (near_full_scale - near_the_floor).abs() < 1e-3,
            "{near_full_scale} vs {near_the_floor}"
        );
        assert!(
            (near_full_scale - 60.0).abs() < 1e-3,
            "10 px per dB on a 720 px track"
        );
    }

    #[test]
    fn levels_above_full_scale_clamp_to_the_right_edge_instead_of_overdrawing() {
        // A +6 dBFS sample (magnitude 2.0) must not draw outside the widget.
        let over = to_dbfs(2.0);
        assert!(over > 0.0, "sanity: {over} should be above full scale");
        assert!((x_for_dbfs(over, TRACK) - (TRACK.x + TRACK.width)).abs() < 1e-4);
        assert_eq!(zone_for(over), Zone::Danger);
    }

    #[test]
    fn colour_zones_switch_at_the_documented_thresholds() {
        assert_eq!(zone_for(FLOOR_DBFS), Zone::Safe);
        assert_eq!(zone_for(-24.0), Zone::Safe);
        assert_eq!(zone_for(CAUTION_DBFS - 0.01), Zone::Safe);
        assert_eq!(zone_for(CAUTION_DBFS), Zone::Caution);
        assert_eq!(zone_for(-10.0), Zone::Caution);
        assert_eq!(zone_for(DANGER_DBFS - 0.01), Zone::Caution);
        assert_eq!(zone_for(DANGER_DBFS), Zone::Danger);
        assert_eq!(zone_for(0.0), Zone::Danger);
    }

    #[test]
    fn zero_channels_lays_out_no_rows_but_still_draws_a_track() {
        assert!(rows(TRACK, 0).is_empty());

        let empty = layout(panel(0), 0);
        assert!(empty.rows.is_empty());
        assert!(
            empty.track.width > 0.0 && empty.track.height > 0.0,
            "an empty meter must still show its scale"
        );
        assert!(
            (empty.track.height - ROW_HEIGHT).abs() < 1e-4,
            "the empty track is one row tall so the panel does not jump when audio \
             arrives, got {}",
            empty.track.height
        );
    }

    #[test]
    fn zero_channels_leaves_the_state_empty_and_idle() {
        let mut state = MeterState::default();
        state.advance(MeterData::default(), 0.016);
        assert!(state.holds.is_empty());
        assert!(!state.is_animating());
        // Reading a channel that does not exist must not panic.
        assert_eq!(state.hold_dbfs(3), FLOOR_DBFS);
    }

    #[test]
    fn rows_stack_downward_without_overlapping_or_overflowing() {
        for channels in [1usize, 2, 8] {
            let area = Rectangle {
                height: 8.0 * (ROW_HEIGHT + ROW_GAP),
                ..TRACK
            };
            let stacked = rows(area, channels);
            assert_eq!(stacked.len(), channels);

            for pair in stacked.windows(2) {
                let (upper, lower) = (pair[0], pair[1]);
                assert!(
                    upper.y + upper.height <= lower.y + 1e-3,
                    "{channels} channels: rows overlap"
                );
                assert_eq!(upper.x, lower.x, "{channels} channels: rows must align");
                assert_eq!(upper.width, lower.width);
            }

            let last = stacked.last().unwrap();
            assert!(
                last.y + last.height <= area.y + area.height + 1e-3,
                "{channels} channels: rows overflow the stack"
            );
            assert!(last.height >= 1.0, "{channels} channels: row collapsed");
            assert!(
                (stacked[0].width - area.width).abs() < 1e-4,
                "every row spans the full scale"
            );
        }
    }

    #[test]
    fn a_lone_channel_gets_a_bar_not_a_slab() {
        // The bug this fixes: one mono channel expanded to fill the panel and read as
        // a meaningless solid block.
        let tall = Size::new(900.0, 400.0);
        let one = layout(tall, 1);

        assert_eq!(one.rows.len(), 1);
        assert!(
            one.rows[0].height <= MAX_ROW_HEIGHT + 1e-4,
            "a single row must be capped, got {}",
            one.rows[0].height
        );
        assert!(
            (one.track.height - one.rows[0].height).abs() < 1e-4,
            "the track must hug the rows rather than framing empty space"
        );
        assert!(
            one.rows[0].y < tall.height / 4.0,
            "the stack must be top-aligned, not centred or stretched"
        );
    }

    #[test]
    fn a_squeezed_stack_shrinks_its_rows_rather_than_dropping_channels() {
        // Eight channels in a panel sized for two: every channel must still be
        // visible, because a channel that is silently missing is worse than a thin one.
        let cramped = layout(panel(2), 8);
        assert_eq!(cramped.rows.len(), 8);
        assert!(cramped.rows.iter().all(|r| r.height >= 1.0));

        let last = cramped.rows.last().unwrap();
        assert!(
            last.y + last.height <= cramped.track.y + cramped.track.height + 1e-3,
            "rows must stay inside the track"
        );
    }

    #[test]
    fn preferred_height_gives_every_row_its_full_height() {
        for channels in [0usize, 1, 2, 8] {
            let size = panel(channels);
            let laid_out = layout(size, channels);

            assert_eq!(laid_out.rows.len(), channels);
            for row in &laid_out.rows {
                assert!(
                    (row.height - ROW_HEIGHT).abs() < 1e-4,
                    "{channels} channels: row is {} px, wanted {ROW_HEIGHT}",
                    row.height
                );
            }

            // The whole widget is accounted for: stack, gap, label strip, nothing over.
            assert!(
                laid_out.labels_y + LABEL_STRIP_HEIGHT <= size.height + 1e-3,
                "{channels} channels: the label strip falls off the bottom"
            );
            assert!(
                (laid_out.labels_y + LABEL_STRIP_HEIGHT - size.height).abs() < 1e-3,
                "{channels} channels: the preferred height must not leave slack"
            );
        }

        assert!(
            preferred_height(8) > preferred_height(2),
            "more channels need more height"
        );
        assert_eq!(
            preferred_height(0),
            preferred_height(1),
            "an empty meter is sized like a mono one so the panel does not resize"
        );
        for channels in [1usize, 2, 8] {
            let per_row = preferred_height(channels) / channels as f32;
            assert!(
                (22.0..=45.0).contains(&per_row),
                "{channels} channels: {per_row} px per row is out of budget"
            );
        }
    }

    #[test]
    fn the_label_strip_sits_under_the_stack_and_the_chrome_keeps_its_margins() {
        let laid_out = layout(panel(2), 2);

        assert!(
            laid_out.labels_y >= laid_out.track.y + laid_out.track.height,
            "dB labels belong below the last row, not over it"
        );
        assert!(
            laid_out.track.x >= CHANNEL_LABEL_WIDTH,
            "the channel-number gutter must not be drawn over"
        );
        assert!(
            (laid_out.track.x + laid_out.track.width + STRIP_GAP + CLIP_STRIP_WIDTH
                - panel(2).width)
                .abs()
                < 1e-3,
            "the clip indicators must fit between the track and the right edge"
        );
    }

    #[test]
    fn a_widget_too_small_to_draw_produces_no_rows_instead_of_garbage() {
        // Parents can hand a canvas a zero or negative content box mid-relayout.
        for size in [
            Size::new(0.0, 0.0),
            Size::new(10.0, 200.0),
            Size::new(900.0, 4.0),
        ] {
            let laid_out = layout(size, 2);
            assert!(
                laid_out.track.width <= 0.0 || laid_out.track.height <= 0.0,
                "{size:?} should be rejected as undrawable"
            );
            assert!(laid_out.rows.is_empty(), "{size:?} should lay out no rows");
        }
    }

    #[test]
    fn a_narrow_track_labels_fewer_ticks_than_a_wide_one() {
        let wide = labelled_ticks(900.0);
        let narrow = labelled_ticks(150.0);

        assert_eq!(
            wide.len(),
            TICKS_DBFS.len(),
            "a full-width track can label everything"
        );
        assert!(
            narrow.len() < wide.len(),
            "a narrow track must thin its labels"
        );
        // 0 dBFS is the reference; it is never the one dropped.
        assert_eq!(narrow.first(), Some(&0.0));
        assert_eq!(wide.first(), Some(&0.0));
    }

    #[test]
    fn labels_never_collide() {
        for width in [60.0f32, 150.0, 400.0, 900.0] {
            let labels = labelled_ticks(width);
            for pair in labels.windows(2) {
                // Labels run right to left: the louder tick of the pair is further right.
                let gap = (width * dbfs_to_fraction(pair[0])) - (width * dbfs_to_fraction(pair[1]));
                assert!(
                    gap >= MIN_LABEL_SPACING,
                    "width {width}: {:?} and {:?} are {gap} px apart",
                    pair[0],
                    pair[1]
                );
            }
        }
    }

    #[test]
    fn state_follows_a_change_of_channel_count() {
        let mut state = MeterState::default();

        state.advance(MeterData::new(&[0.5, 0.5], &[0.2, 0.2], &[]), 0.016);
        assert_eq!(state.holds.len(), 2);
        assert!((state.hold_dbfs(0) - to_dbfs(0.5)).abs() < 1e-4);

        // Operator switches to an 8-channel interface: the new channels start at the
        // floor rather than inheriting channel 0's peak.
        state.advance(MeterData::new(&[0.0; 8], &[0.0; 8], &[]), 0.016);
        assert_eq!(state.holds.len(), 8);
        assert_eq!(state.hold_dbfs(7), FLOOR_DBFS);
        assert!(
            (state.hold_dbfs(0) - to_dbfs(0.5)).abs() < 1e-4,
            "still dwelling"
        );
    }

    #[test]
    fn ragged_input_slices_do_not_panic() {
        // The two buffers can disagree for a frame while the device is reconfiguring.
        let data = MeterData::new(&[0.5, 0.25, 0.1], &[0.1], &[true]);
        assert_eq!(data.channels(), 3);
        assert_eq!(data.rms_dbfs(2), FLOOR_DBFS, "missing RMS reads as silence");
        assert!(data.clipped(0));
        assert!(!data.clipped(2), "missing clip flags read as not clipped");

        let mut state = MeterState::default();
        state.advance(data, 0.016);
        assert_eq!(state.holds.len(), 3);
    }

    #[test]
    fn the_widget_composes_into_any_parents_message_type() {
        // A compile-time check, not a rendering test: the payoff of implementing
        // `Program<Message>` generically is that the parent never has to name a
        // message the meter will not send.
        #[derive(Debug)]
        enum ParentMessage {
            #[allow(dead_code)]
            Tick,
        }

        let peak = [0.5, 0.2];
        let rms = [0.3, 0.1];
        let clipped = [false, true];

        let element: iced::Element<'_, ParentMessage> =
            meter(MeterData::new(&peak, &rms, &clipped)).into();

        drop(element);
    }

    #[test]
    fn a_stalled_frame_cannot_teleport_the_marker_to_the_floor() {
        // The clamp is applied in `update`; assert the constant it relies on is small
        // enough that a stall costs at most a sensible amount of fall.
        let worst_case_fall = MAX_FRAME_SECS * PEAK_FALL_DB_PER_SEC;
        assert!(
            worst_case_fall < -FLOOR_DBFS,
            "a single stalled frame must not consume the whole scale"
        );
    }
}
