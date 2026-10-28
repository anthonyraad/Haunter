//! Drive: a warm, oversampled tape-style saturator for the blended sample and Dream Eater's
//! chord, and the bend Drive gives the duck's envelope.
//!
//! The saturator runs at four times the sample rate behind two polyphase half-band IIR stages
//! (about 110 dB of image and alias rejection), and works on a soft asymmetric tanh curve that
//! is linear at the start of the knob and never clips. A high shelf is cut before the curve and
//! put back after it, so the top end distorts less than the body, and a gentle low-pass that
//! closes as Drive rises rounds off what is left. A DC blocker (DC_HZ) takes out what the bias
//! adds; it hears only what the curve and ceiling change, so the input's own lows pass untouched.
//!
//! The level is matched to the input's loudness above LOUDNESS_HZ, then the body is lifted with
//! Drive (about 3 dB at the top of the knob, since 1.7.15; 1.7.14 held the loudness) into a soft
//! ceiling under the input's peak, the way the waveform display has shown it since 1.7.14: the
//! sound fattens and comes forward while its peaks stay where they were.
//!
//! At 0 the saturator is bypassed outright: its output is its input, bit for bit, and it costs
//! nothing. Turning it on or off crossfades over `ENGAGE_MS`.

use std::f64::consts::PI;

/// The curve's gain at full Drive. It grows in step with Drive (since 1.7.15; before, with its
/// square), so the middle of the knob is already warm.
const MAX_GAIN: f64 = 2.5;
/// The curve's bias at full Drive, which adds the even harmonics: enough that the second
/// harmonic leads the third all along the knob.
const MAX_BIAS: f64 = 0.6;
/// How far the high shelf is cut before the curve, and put back after it, at full Drive.
const EMPHASIS_DB: f64 = 4.0;
const EMPHASIS_HZ: f64 = 2_500.0;
/// The low-pass after the curve, wide open at the bottom of the knob and at this at the top.
const LOW_PASS_OPEN_HZ: f64 = 40_000.0;
const LOW_PASS_CLOSED_HZ: f64 = 12_000.0;
/// How quickly the level the curve sees lets go after a peak. Slow (since 1.7.15; 300 ms
/// before), so a sound's quieter parts reach the curve quieter, and Drive fills them out the way
/// the waveform shows instead of bending every part of the sound alike.
const PEAK_RELEASE_MS: f64 = 3_000.0;
/// The level is held while the input keeps coming back to within this of it, and for
/// PEAK_HOLD_MS after, so it stays still over a steady tone instead of rippling with it.
const PEAK_HOLD_WITHIN: f64 = 0.9;
const PEAK_HOLD_MS: f64 = 30.0;
const PEAK_FLOOR: f64 = 1e-6;
/// How quickly the loudness match follows, and above what it listens, so the low end the bias
/// leaves behind never counts toward it.
const LOUDNESS_MS: f64 = 200.0;
const LOUDNESS_HZ: f64 = 100.0;
const MAX_MATCH: f64 = 4.0;
/// How far the body comes up over the loudness match at full Drive, in step with Drive.
const LIFT_DB: f64 = 3.5;
/// How far over the input's peak that may take the signal into the ceiling at full Drive, with
/// the square of Drive, so the bottom of the knob barely touches the ceiling.
const HEADROOM_DB: f64 = 4.0;
/// Where the ceiling tops out, against the input's peak, leaving room for what the half-band
/// filters ring past it.
const CEILING: f64 = 0.94;
/// What still rings past the input's peak is turned down (attacking over GUARD_ATTACK_MS and
/// letting go with the peak), and the last of it rounded off from GUARD_KNEE of the peak up.
const GUARD_KNEE: f64 = 0.99;
const GUARD_ATTACK_MS: f64 = 2.0;
/// How quickly the loudness match moves, so it does not ripple with the signal.
const MATCH_GLIDE_MS: f64 = 30.0;
/// Where the DC blocker sits.
const DC_HZ: f64 = 20.0;
const ENGAGE_MS: f64 = 10.0;

/// How far the dips thicken at full Drive.
const BEND: f32 = 3.0;

/// Bends the duck's 0..1 envelope with Drive, the way the curve bends the sample: low levels are
/// lifted, so the dips are fuller and hold longer, while 1 stays 1 so the deepest point is not
/// moved. At 0 the envelope is returned untouched.
pub fn bend_envelope(envelope: f32, drive: f32) -> f32 {
    if drive <= 0.0 {
        return envelope;
    }
    let drive = drive.min(1.0);
    let bent = (BEND * envelope).tanh() / BEND.tanh();
    envelope + drive * (bent - envelope)
}

/// The coefficients of a polyphase half-band IIR filter with `attenuation` dB of stopband and a
/// `transition` band (as a fraction of the oversampled rate) either side of a quarter of it.
fn half_band(attenuation: f64, transition: f64) -> Vec<f64> {
    let mut k = ((1.0 - 2.0 * transition) * PI / 4.0).tan();
    k *= k;
    let kk = (1.0 - k * k).powf(0.25);
    let e = 0.5 * (1.0 - kk) / (1.0 + kk);
    let e4 = e.powi(4);
    let q = e * (1.0 + e4 * (2.0 + e4 * (15.0 + 150.0 * e4)));
    let power = 10f64.powf(-attenuation / 10.0);
    let a = power / (1.0 - power);
    let mut order = ((a * a / 16.0).ln() / q.ln()).ceil() as usize;
    if order % 2 == 0 {
        order += 1;
    }
    order = order.max(3);
    (0..(order - 1) / 2)
        .map(|index| {
            let c = (index + 1) as f64;
            let o = order as f64;
            let (mut i, mut sign, mut num) = (0i32, 1.0, 0.0);
            loop {
                let term = q.powi(i * (i + 1)) * (((2 * i + 1) as f64) * c * PI / o).sin() * sign;
                num += term;
                sign = -sign;
                i += 1;
                if term.abs() <= 1e-100 {
                    break;
                }
            }
            let (mut i, mut sign, mut den) = (1i32, -1.0, 0.0);
            loop {
                let term = q.powi(i * i) * (((2 * i) as f64) * c * PI / o).cos() * sign;
                den += term;
                sign = -sign;
                i += 1;
                if term.abs() <= 1e-100 {
                    break;
                }
            }
            let ww = num * q.powf(0.25) / (den + 0.5);
            let ww2 = ww * ww;
            let x = ((1.0 - ww2 * k) * (1.0 - ww2 / k)).sqrt() / (1.0 + ww2);
            (1.0 - x) / (1.0 + x)
        })
        .collect()
}

/// A left and right sample, worked on side by side so the two channels share instructions.
type Stereo = [f64; 2];

/// A chain of first-order all-pass sections in z^-2 of the oversampled rate, for both channels.
#[derive(Debug, Clone, Copy)]
struct AllPasses<const N: usize> {
    coefficients: [f64; N],
    inputs: [Stereo; N],
    outputs: [Stereo; N],
}

impl<const N: usize> AllPasses<N> {
    fn new(coefficients: impl Iterator<Item = f64>) -> Self {
        let mut chain = AllPasses { coefficients: [0.0; N], inputs: [[0.0; 2]; N], outputs: [[0.0; 2]; N] };
        let mut count = 0;
        for (slot, c) in chain.coefficients.iter_mut().zip(coefficients) {
            *slot = c;
            count += 1;
        }
        assert_eq!(count, N, "the design gave a different order");
        chain
    }

    #[inline]
    fn run(&mut self, mut x: Stereo) -> Stereo {
        for i in 0..N {
            let c = self.coefficients[i];
            let (x1, y1) = (self.inputs[i], self.outputs[i]);
            let y = [c * (x[0] - y1[0]) + x1[0], c * (x[1] - y1[1]) + x1[1]];
            self.inputs[i] = x;
            self.outputs[i] = y;
            x = y;
        }
        x
    }

    fn reset(&mut self) {
        self.inputs = [[0.0; 2]; N];
        self.outputs = [[0.0; 2]; N];
    }
}

/// One half-band stage, which doubles or halves the rate, with `E` and `O` sections on its two
/// paths.
#[derive(Debug, Clone, Copy)]
struct HalfBand<const E: usize, const O: usize> {
    even: AllPasses<E>,
    odd: AllPasses<O>,
}

impl<const E: usize, const O: usize> HalfBand<E, O> {
    fn new(attenuation: f64, transition: f64) -> Self {
        let coefficients = half_band(attenuation, transition);
        HalfBand {
            even: AllPasses::new(coefficients.iter().step_by(2).copied()),
            odd: AllPasses::new(coefficients.iter().skip(1).step_by(2).copied()),
        }
    }

    /// One sample in, two out at twice the rate.
    #[inline]
    fn up(&mut self, x: Stereo) -> [Stereo; 2] {
        [self.even.run(x), self.odd.run(x)]
    }

    /// Two samples in, one out at half the rate.
    #[inline]
    fn down(&mut self, pair: [Stereo; 2]) -> Stereo {
        let (even, odd) = (self.even.run(pair[1]), self.odd.run(pair[0]));
        [0.5 * (even[0] + odd[0]), 0.5 * (even[1] + odd[1])]
    }

    fn reset(&mut self) {
        self.even.reset();
        self.odd.reset();
    }
}

/// The first stage keeps everything to 20 kHz flat at 44.1 and 48 kHz and rejects from 28 kHz
/// up, with 100 dB: nine sections. The second only has to clear the space the first left
/// empty, with 110 dB: six.
type FirstStage = HalfBand<5, 4>;
type SecondStage = HalfBand<3, 3>;

fn first_stage() -> FirstStage {
    HalfBand::new(100.0, 0.04)
}

fn second_stage() -> SecondStage {
    HalfBand::new(110.0, 0.125)
}

/// A first-order filter, `b0 + b1 z^-1` over `1 + a1 z^-1`, for both channels.
#[derive(Debug, Clone, Copy, Default)]
struct FirstOrder {
    x1: Stereo,
    y1: Stereo,
}

impl FirstOrder {
    #[inline]
    fn run(&mut self, [b0, b1, a1]: [f64; 3], x: Stereo) -> Stereo {
        let y = [0, 1].map(|side| b0 * x[side] + b1 * self.x1[side] - a1 * self.y1[side]);
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// A second-order high-pass for both channels, in transposed direct form II.
#[derive(Debug, Clone, Copy, Default)]
struct HighPass {
    coefficients: [f64; 5],
    state: [Stereo; 2],
}

impl HighPass {
    /// A Butterworth high-pass at `hz`.
    fn set(&mut self, hz: f64, sample_rate: f64) {
        let w = 2.0 * PI * hz / sample_rate;
        let alpha = w.sin() / 2f64.sqrt();
        let a0 = 1.0 + alpha;
        let b = (1.0 + w.cos()) / 2.0;
        self.coefficients = [b / a0, -2.0 * b / a0, b / a0, -2.0 * w.cos() / a0, (1.0 - alpha) / a0];
    }

    #[inline]
    fn run(&mut self, x: Stereo) -> Stereo {
        let [b0, b1, b2, a1, a2] = self.coefficients;
        [0, 1].map(|side| {
            let y = b0 * x[side] + self.state[0][side];
            self.state[0][side] = b1 * x[side] - a1 * y + self.state[1][side];
            self.state[1][side] = b2 * x[side] - a2 * y;
            y
        })
    }

    fn reset(&mut self) {
        self.state = [[0.0; 2]; 2];
    }
}

/// A level that jumps to each new peak, holds while the signal keeps coming back to within
/// PEAK_HOLD_WITHIN of it, and lets go slowly after.
#[derive(Debug, Clone, Copy, Default)]
struct HeldPeak {
    peak: f64,
    hold: usize,
}

impl HeldPeak {
    #[inline]
    fn follow(&mut self, loudest: f64, hold_frames: usize, release: f64) -> f64 {
        if loudest >= self.peak * PEAK_HOLD_WITHIN {
            self.peak = self.peak.max(loudest);
            self.hold = hold_frames;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            self.peak *= release;
        }
        self.peak = self.peak.max(PEAK_FLOOR);
        self.peak
    }
}

/// The filters and their state.
#[derive(Debug, Clone, Copy)]
struct Filters {
    up: (FirstStage, SecondStage),
    down: (FirstStage, SecondStage),
    pre: FirstOrder,
    post: FirstOrder,
    low_pass: Stereo,
    /// The same for what the curve bends, apart, so only that goes through the DC blocker.
    bend_post: FirstOrder,
    bend_low_pass: Stereo,
    dc: FirstOrder,
    /// And for what the ceiling rounds off, since it rounds off the curve's longer side more.
    dc_after: FirstOrder,
    /// What the loudness match hears of the saturated signal.
    heard: HighPass,
}

impl Filters {
    fn new() -> Self {
        Filters {
            up: (first_stage(), second_stage()),
            down: (first_stage(), second_stage()),
            pre: FirstOrder::default(),
            post: FirstOrder::default(),
            low_pass: [0.0; 2],
            bend_post: FirstOrder::default(),
            bend_low_pass: [0.0; 2],
            dc: FirstOrder::default(),
            dc_after: FirstOrder::default(),
            heard: HighPass::default(),
        }
    }

    fn reset(&mut self) {
        self.up.0.reset();
        self.up.1.reset();
        self.down.0.reset();
        self.down.1.reset();
        self.pre = FirstOrder::default();
        self.post = FirstOrder::default();
        self.low_pass = [0.0; 2];
        self.bend_post = FirstOrder::default();
        self.bend_low_pass = [0.0; 2];
        self.dc = FirstOrder::default();
        self.dc_after = FirstOrder::default();
        self.heard.reset();
    }

    /// One sample at four times the rate through the curve, lifted by `gain` into the ceiling,
    /// with the power the loudness match hears in it and its peak, both before `gain`. Only the
    /// curve and the ceiling see the signal brought to the level they expect, `level`, so a
    /// change of level reaches their input and output together.
    #[inline]
    fn shape(&mut self, curve: &Curve, x: Stereo, level: f64, to_unit: f64, gain: f64) -> (Stereo, f64, f64) {
        let x = self.pre.run(curve.pre, x).map(|side| side * to_unit);
        // The curve is the identity plus what it bends, and the filters after it are linear, so
        // the two go through them apart and only the bend goes through the DC blocker: it takes
        // out what the bias leaves behind, before the ceiling so it holds both sides alike, and
        // the input's own lows pass untouched.
        let straight = self.post.run(curve.post, x.map(|side| level * side));
        let bend = self.bend_post.run(curve.post, x.map(|side| level * (curve.shape(side) - side)));
        for side in 0..2 {
            self.low_pass[side] += curve.low_pass * (straight[side] - self.low_pass[side]);
            self.bend_low_pass[side] += curve.low_pass * (bend[side] - self.bend_low_pass[side]);
        }
        let bend = self.dc.run(curve.dc, self.bend_low_pass);
        let y = [0, 1].map(|side| self.low_pass[side] + bend[side]);
        let heard = self.heard.run(y);
        // After the ceiling, the bend and what the ceiling rounds off go through it again, and
        // the input's own lows still pass untouched.
        let lift = level * gain * to_unit;
        let lifted = y.map(|side| level * CEILING * ceiling(gain * side * to_unit / CEILING, curve.knee));
        let bent = self.dc_after.run(curve.dc, [0, 1].map(|side| lifted[side] - lift * self.low_pass[side]));
        let out = [0, 1].map(|side| lift * self.low_pass[side] + bent[side]);
        (out, heard[0] * heard[0] + heard[1] * heard[1], y[0].abs().max(y[1].abs()))
    }
}

/// The soft ceiling the lifted signal goes through, on a scale where the input's peak is 1: the
/// identity up to `knee`, then a tanh shoulder that never quite reaches 1, so the body can come up
/// while the peaks round off under the input's. The shoulder meets the identity with the same
/// slope and curvature, so there is no corner to add harshness. A knee of 1 (Drive at 0) leaves
/// the signal as it is.
#[inline]
pub fn ceiling(x: f64, knee: f64) -> f64 {
    let room = 1.0 - knee;
    let size = x.abs();
    if size <= knee || room <= 1e-12 {
        x
    } else {
        (knee + room * ((size - knee) / room).tanh()).copysign(x)
    }
}

/// How far the body comes up at `drive` over the loudness match, and how far over the input's
/// peak that may take the signal into the ceiling, both as gains, and the ceiling's knee, chosen
/// so that a peak taken that far lands on the shoulder at tanh(1.5), 0.9 of the way up it.
fn lift_and_knee(drive: f64) -> (f64, f64, f64) {
    let lift = 10f64.powf(LIFT_DB * drive / 20.0);
    let headroom = 10f64.powf(HEADROOM_DB * drive * drive / 20.0);
    (lift, headroom, (2.0 / headroom - 1.0).clamp(0.0, 1.0))
}

/// The saturation curve for one Drive, 0 to 1, on its own: tanh with a bias, moved so 0 stays
/// at 0 and the slope there is 1. As Drive goes to 0 this becomes the identity, so the knob
/// comes in from nothing. The audio and the waveform display both bend through this, so what
/// you see is what you hear. It expects its input brought to a peak of about 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transfer {
    gain: f64,
    bias: f64,
    offset: f64,
    scale: f64,
}

impl Transfer {
    pub fn new(drive: f64) -> Self {
        let drive = if drive.is_finite() { drive.clamp(0.0, 1.0) } else { 0.0 };
        let gain = (MAX_GAIN * drive).max(1e-6);
        let bias = MAX_BIAS * drive;
        let offset = bias.tanh();
        let scale = 1.0 / (gain * (1.0 - offset * offset));
        Transfer { gain, bias, offset, scale }
    }

    #[inline]
    pub fn apply(&self, x: f64) -> f64 {
        ((self.gain * x + self.bias).tanh() - self.offset) * self.scale
    }

    /// The offset the curve gives a sine that swings `swing` either side of 0, which is what the
    /// saturator's DC blocker takes back out of it.
    pub fn offset_of_swing(&self, swing: f64) -> f64 {
        const STEPS: usize = 16;
        (0..STEPS).map(|n| self.apply(swing * (2.0 * PI * (n as f64 + 0.5) / STEPS as f64).sin())).sum::<f64>() / STEPS as f64
    }
}

/// `x` through the curve at `drive`; at 0 (or below) it is `x` exactly, as the saturator gives
/// back its input bit for bit there.
pub fn transfer(x: f64, drive: f64) -> f64 {
    if drive > 0.0 { Transfer::new(drive).apply(x) } else { x }
}

/// The waveform display's columns (each a column's lowest and highest sample) as Drive bends
/// them, the way the audio is: brought to a peak of 1 as the saturator brings its input to its
/// held peak, through the same curve, less the offset the DC blocker takes out of a tone that
/// swings as far as the column does (since 1.7.15), and brought back so the loudest point is
/// exactly as high as it was (since 1.7.14). So the waveform never shrinks: the peaks stay full
/// height and round off, and the quieter parts swell, as the audio's body comes up under its
/// peaks. At 0 they are returned untouched.
pub fn driven_columns(columns: &[(f32, f32)], drive: f32) -> Vec<(f32, f32)> {
    if !(drive > 0.0) {
        return columns.to_vec();
    }
    let transfer = Transfer::new(drive as f64);
    let peak = columns.iter().fold(1e-6f64, |m, &(lo, hi)| m.max((lo as f64).abs()).max((hi as f64).abs()));
    let bent: Vec<(f64, f64)> = columns
        .iter()
        .map(|&(lo, hi)| {
            let (lo, hi) = (lo as f64 / peak, hi as f64 / peak);
            let offset = transfer.offset_of_swing(0.5 * (hi - lo));
            (transfer.apply(lo) - offset, transfer.apply(hi) - offset)
        })
        .collect();
    let bent_peak = bent.iter().fold(0f64, |m, &(lo, hi)| m.max(lo.abs()).max(hi.abs()));
    let matched = if bent_peak > 0.0 { peak / bent_peak } else { peak };
    bent.into_iter().map(|(lo, hi)| ((lo * matched) as f32, (hi * matched) as f32)).collect()
}

/// What one frame's Drive works out to.
#[derive(Debug, Clone, Copy)]
struct Curve {
    transfer: Transfer,
    pre: [f64; 3],
    post: [f64; 3],
    low_pass: f64,
    dc: [f64; 3],
    lift: f64,
    headroom: f64,
    knee: f64,
}

impl Curve {
    #[inline]
    fn shape(&self, x: f64) -> f64 {
        self.transfer.apply(x)
    }
}

#[derive(Debug, Clone)]
pub struct Saturator {
    sample_rate: f64,
    filters: Filters,
    /// 0..1, crossfades between the dry input and the saturated one.
    engage: f64,
    engage_step: f64,
    /// The input's held peak, which the curve's input is brought to and the output's peaks are
    /// kept under, and the output's own, which catches what the filters after the ceiling add.
    peak: HeldPeak,
    out_peak: HeldPeak,
    bent_peak: HeldPeak,
    peak_release: f64,
    hold_frames: usize,
    /// What the loudness match hears of the input.
    heard: HighPass,
    in_power: f64,
    out_power: f64,
    loudness_rate: f64,
    /// The same at four times the rate, for the saturated signal.
    oversampled_loudness_rate: f64,
    matched: f64,
    match_glide: f64,
    guard: f64,
    guard_attack: f64,
    dc_coefficient: f64,
    shelf_warp: f64,
    /// The last frame's Drive and what it worked out to.
    cached: Option<(f64, Curve)>,
}

impl Saturator {
    pub fn new(sample_rate: f64) -> Self {
        let mut saturator = Saturator {
            sample_rate,
            filters: Filters::new(),
            engage: 0.0,
            engage_step: 0.0,
            peak: HeldPeak::default(),
            out_peak: HeldPeak::default(),
            bent_peak: HeldPeak::default(),
            peak_release: 0.0,
            hold_frames: 0,
            heard: HighPass::default(),
            in_power: 0.0,
            out_power: 0.0,
            loudness_rate: 0.0,
            oversampled_loudness_rate: 0.0,
            matched: 1.0,
            match_glide: 0.0,
            guard: 1.0,
            guard_attack: 0.0,
            dc_coefficient: 0.0,
            shelf_warp: 0.0,
            cached: None,
        };
        saturator.prepare(sample_rate);
        saturator
    }

    /// Sets the sample rate. Not for the audio thread.
    pub fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.engage_step = 1.0 / (ENGAGE_MS * 0.001 * sample_rate).max(1.0);
        self.peak_release = (-1.0 / (PEAK_RELEASE_MS * 0.001 * sample_rate)).exp();
        self.hold_frames = (PEAK_HOLD_MS * 0.001 * sample_rate) as usize;
        self.loudness_rate = 1.0 - (-1.0 / (LOUDNESS_MS * 0.001 * sample_rate)).exp();
        self.oversampled_loudness_rate = 1.0 - (-1.0 / (LOUDNESS_MS * 0.001 * 4.0 * sample_rate)).exp();
        self.match_glide = 1.0 - (-1.0 / (MATCH_GLIDE_MS * 0.001 * sample_rate)).exp();
        self.guard_attack = 1.0 - (-1.0 / (GUARD_ATTACK_MS * 0.001 * sample_rate)).exp();
        self.dc_coefficient = (-2.0 * PI * DC_HZ / (4.0 * sample_rate)).exp();
        self.shelf_warp = (PI * EMPHASIS_HZ / (4.0 * sample_rate)).tan();
        self.heard.set(LOUDNESS_HZ, sample_rate);
        self.filters.heard.set(LOUDNESS_HZ, 4.0 * sample_rate);
        self.cached = None;
        self.reset();
    }

    /// Back to bypassed, from silence.
    pub fn reset(&mut self) {
        self.engage = 0.0;
        self.clear();
    }

    fn clear(&mut self) {
        self.filters.reset();
        self.peak = HeldPeak::default();
        self.out_peak = HeldPeak::default();
        self.bent_peak = HeldPeak::default();
        self.heard.reset();
        self.in_power = 0.0;
        self.out_power = 0.0;
        self.matched = 1.0;
        self.guard = 1.0;
    }

    /// Whether the last frame was saturated at all.
    pub fn engaged(&self) -> bool {
        self.engage > 0.0
    }

    fn curve(&self, drive: f64) -> Curve {
        let transfer = Transfer::new(drive);
        // A first-order high shelf, cut by `cut` above EMPHASIS_HZ, and its exact inverse.
        let cut = 10f64.powf(-EMPHASIS_DB * drive / 20.0);
        let w = self.shelf_warp;
        let pre = [(cut + w) / (1.0 + w), (w - cut) / (1.0 + w), (w - 1.0) / (1.0 + w)];
        let post = [(1.0 + w) / (cut + w), (w - 1.0) / (cut + w), (w - cut) / (cut + w)];
        let low_pass_hz = LOW_PASS_OPEN_HZ * (LOW_PASS_CLOSED_HZ / LOW_PASS_OPEN_HZ).powf(drive);
        let low_pass = 1.0 - (-2.0 * PI * low_pass_hz / (4.0 * self.sample_rate)).exp();
        let dc = [1.0, -1.0, -self.dc_coefficient];
        let (lift, headroom, knee) = lift_and_knee(drive);
        Curve { transfer, pre, post, low_pass, dc, lift, headroom, knee }
    }

    /// Saturates one stereo frame by `drive`, 0 to 1. At 0, once any fade out is done, the
    /// input comes back as it was, bit for bit.
    pub fn process(&mut self, input: Stereo, drive: f64) -> Stereo {
        let drive = if drive.is_finite() { drive.clamp(0.0, 1.0) } else { 0.0 };
        if drive <= 0.0 {
            if self.engage <= 0.0 {
                return input;
            }
            self.engage = (self.engage - self.engage_step).max(0.0);
        } else {
            if self.engage <= 0.0 {
                self.clear();
            }
            self.engage = (self.engage + self.engage_step).min(1.0);
        }
        let curve = match self.cached {
            Some((cached, curve)) if cached == drive => curve,
            _ => {
                let curve = self.curve(drive);
                self.cached = Some((drive, curve));
                curve
            }
        };
        // The curve works on the input brought to its slowly held peak, so how hard it drives
        // depends on the knob and not on how loud the input is, while the quieter parts of a
        // sound still reach it quieter than its peaks.
        let peak = self.peak.follow(input[0].abs().max(input[1].abs()), self.hold_frames, self.peak_release);
        let to_unit = 1.0 / peak;
        let gain = self.matched;
        let f = &mut self.filters;
        let [a, b] = f.up.0.up(input);
        let [a0, a1] = f.up.1.up(a);
        let [b0, b1] = f.up.1.up(b);
        let (a0, p0, q0) = f.shape(&curve, a0, peak, to_unit, gain);
        let (a1, p1, q1) = f.shape(&curve, a1, peak, to_unit, gain);
        let (b0, p2, q2) = f.shape(&curve, b0, peak, to_unit, gain);
        let (b1, p3, q3) = f.shape(&curve, b1, peak, to_unit, gain);
        let a = f.down.1.down([a0, a1]);
        let b = f.down.1.down([b0, b1]);
        let y = f.down.0.down([a, b]);
        // Matches the loudness above LOUDNESS_HZ to the input's, so the low end the bias leaves
        // behind never counts, then lifts the body with Drive, but never so far that the peaks
        // go more than the headroom into the ceiling, which holds them under the input's.
        let rate = self.oversampled_loudness_rate;
        for power in [p0, p1, p2, p3] {
            self.out_power += rate * (power - self.out_power);
        }
        let heard = self.heard.run(input);
        self.in_power += self.loudness_rate * (heard[0] * heard[0] + heard[1] * heard[1] - self.in_power);
        let target = ((self.in_power + 1e-24) / (self.out_power + 1e-24)).sqrt().clamp(1.0 / MAX_MATCH, MAX_MATCH) * curve.lift;
        let bent = self.bent_peak.follow(q0.max(q1).max(q2).max(q3), self.hold_frames, self.peak_release);
        let target = target.min(curve.headroom * peak / bent);
        self.matched += self.match_glide * (target - self.matched);
        // What the half-band filters let ring past the ceiling is turned down, so no peak is ever
        // louder than the input's.
        let out_peak = self.out_peak.follow(y[0].abs().max(y[1].abs()), self.hold_frames, self.peak_release);
        let room = (peak / out_peak).min(1.0);
        let glide = if room < self.guard { self.guard_attack } else { self.match_glide };
        self.guard += glide * (room - self.guard);
        let (guard, knee) = (self.guard * to_unit, GUARD_KNEE);
        let y = y.map(|side| peak * ceiling(guard * side, knee));
        let engage = self.engage;
        [0, 1].map(|side| input[side] + engage * (y[side] - input[side]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    #[test]
    fn the_shared_curve_is_the_identity_at_0_rising_and_bounded() {
        for x in [-1e9, -3.0, -1.0, -0.37, -1e-12, 0.0, 1e-12, 0.5, 1.0, 7.0, 1e9] {
            assert_eq!(transfer(x, 0.0).to_bits(), x.to_bits(), "{x}");
            assert_eq!(transfer(x, -0.5).to_bits(), x.to_bits(), "{x}");
        }
        for drive in [1e-4, 0.01, 0.1, 0.25, 0.5, 0.75, 1.0] {
            let curve = Transfer::new(drive);
            assert_eq!(curve.apply(0.0), 0.0);
            // Slope 1 at 0, so quiet parts are left as they are.
            let slope = (curve.apply(1e-7) - curve.apply(-1e-7)) / 2e-7;
            assert!((slope - 1.0).abs() < 1e-6, "{drive}: slope {slope}");
            let mut last = f64::NEG_INFINITY;
            for n in -4000..=4000 {
                let x = n as f64 / 1000.0;
                let y = curve.apply(x);
                assert!(y.is_finite() && (y > last || (x.abs() > 1.0 && y >= last)), "{drive}: not rising at {x}");
                last = y;
                // Never past the input's own peak by more than a hair, near the identity.
                if x.abs() <= 1.0 {
                    assert!(y.abs() <= 1.0 + 0.01, "{drive}: {x} gives {y}");
                }
            }
            // Bounded however hard it is pushed.
            let bound = (1.0 + curve.offset) * curve.scale;
            for x in [-1e12, -1e6, 1e6, 1e12] {
                assert!(curve.apply(x).abs() <= bound, "{drive}");
            }
            if drive >= 0.5 {
                // Peaks round off: a full-scale input comes out well under the identity.
                assert!(curve.apply(1.0) < 0.9 && curve.apply(-1.0) > -0.95, "{drive}");
            }
        }
        // The curve the saturator runs is this one.
        let saturator = Saturator::new(RATE);
        for drive in [0.1, 0.6, 1.0] {
            assert_eq!(saturator.curve(drive).transfer, Transfer::new(drive));
        }
    }

    fn kick_columns() -> Vec<(f32, f32)> {
        (0..1024)
            .map(|n| {
                let level = 0.8 * (-(n as f32) / 200.0).exp();
                let wobble = (n as f32 * 0.07).sin();
                (-level * (0.6 + 0.4 * wobble.abs()), level * (0.7 + 0.3 * wobble))
            })
            .collect()
    }

    #[test]
    fn the_waveform_at_0_is_the_waveform() {
        let columns = kick_columns();
        for drive in [0.0, -0.0, -1.0, f32::NAN] {
            let shown = driven_columns(&columns, drive);
            assert_eq!(shown.len(), columns.len());
            for (a, b) in shown.iter().zip(&columns) {
                assert_eq!((a.0.to_bits(), a.1.to_bits()), (b.0.to_bits(), b.1.to_bits()));
            }
        }
    }

    #[test]
    fn drive_keeps_the_waveforms_peak_and_swells_its_tail_smoothly() {
        let columns = kick_columns();
        let peak_of = |c: &[(f32, f32)]| c.iter().fold(0f32, |m, &(lo, hi)| m.max(lo.abs()).max(hi.abs()));
        let tail_of = |c: &[(f32, f32)]| c[700..].iter().map(|&(lo, hi)| hi - lo).sum::<f32>();
        let (peak, tail) = (peak_of(&columns), tail_of(&columns));
        let mut last = 1.0f32;
        let mut previous = columns.clone();
        for step in 1..=100 {
            let drive = step as f32 / 100.0;
            let shown = driven_columns(&columns, drive);
            let (crest, swell) = (peak_of(&shown) / peak, tail_of(&shown) / tail);
            // The loudest point stays at its height, so the picture never shrinks, and the tail
            // grows, step by step (within a hair while the curve barely bends, where the asymmetry
            // nudges it by about 0.01%, far under a pixel).
            assert!((crest - 1.0).abs() < 0.01, "{drive}: peak at {crest}");
            assert!(swell >= last * (1.0 - 1e-3), "{drive}: tail {swell} after {last}");
            last = swell;
            // A small turn moves the picture a little: no jumps to flicker.
            let moved = shown.iter().zip(&previous).map(|(a, b)| (a.0 - b.0).abs().max((a.1 - b.1).abs())).fold(0f32, f32::max);
            assert!(moved < 0.03 * peak, "{drive}: moved {moved}");
            previous = shown;
            // Every column keeps its sign and order.
            for (&(lo, hi), &(lo0, hi0)) in previous.iter().zip(&columns) {
                assert!(lo <= hi && lo.signum() == lo0.signum() && hi.signum() == hi0.signum());
            }
        }
        let full = driven_columns(&columns, 1.0);
        // Below the peak everything is lifted: the loud part flattens toward full height.
        assert!(tail_of(&full) > 1.4 * tail, "the quiet part swells");
        let near_peak = |c: &[(f32, f32)]| c.iter().filter(|&&(_, hi)| hi > 0.5 * peak).count();
        assert!(near_peak(&full) > near_peak(&columns) * 21 / 20, "{} against {}", near_peak(&full), near_peak(&columns));
    }

    #[test]
    fn the_waveform_leans_the_way_the_audio_does() {
        // A steady tone's two sides as drawn, and as heard, once the DC blocker has settled.
        for drive in [0.25, 0.5, 1.0] {
            let shown = driven_columns(&[(-0.8, 0.8)], drive as f32)[0];
            let drawn = shown.1 as f64 / -shown.0 as f64;
            let mut saturator = Saturator::new(RATE);
            let out: Vec<f64> = sine(220.0, 4 * 48_000, 0.8).iter().map(|&x| saturator.process([x, x], drive)[0]).collect();
            let (top, bottom) = out[3 * 48_000..].iter().fold((0f64, 0f64), |(t, b), &v| (t.max(v), b.min(v)));
            let heard = top / -bottom;
            // The ceiling rounds the longer side a touch more than the picture shows.
            assert!((drawn - heard).abs() < 0.15, "{drive}: drawn {drawn}, heard {heard}");
        }
    }

    fn sine(freq: f64, frames: usize, level: f64) -> Vec<f64> {
        (0..frames).map(|n| level * (2.0 * PI * freq * n as f64 / RATE).sin()).collect()
    }

    /// The level of `freq` in `x`, relative to a full-scale sine.
    fn level_db(x: &[f64], freq: f64) -> f64 {
        let w = 2.0 * PI * freq / RATE;
        let (mut s1, mut s2) = (0.0, 0.0);
        for &v in x {
            let s = v + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        let power = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
        10.0 * (power.max(1e-300) * 4.0 / (x.len() as f64).powi(2)).log10()
    }

    fn rms(x: &[f64]) -> f64 {
        (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// One second of a sine at `level` through Drive `drive`, after half a second to settle.
    fn driven_sine(freq: f64, level: f64, drive: f64) -> Vec<f64> {
        let mut saturator = Saturator::new(RATE);
        let input = sine(freq, 72_000, level);
        let out: Vec<f64> = input.iter().map(|&x| saturator.process([x, x], drive)[0]).collect();
        out[24_000..].to_vec()
    }

    /// The harmonics of `freq` below Nyquist, and the loudest alias of the ones above it that
    /// folds back under `audible`.
    fn harmonics_and_aliases(out: &[f64], freq: f64, audible: f64) -> (Vec<(f64, f64)>, f64) {
        let nyquist = RATE / 2.0;
        let mut harmonics = Vec::new();
        let mut alias = f64::NEG_INFINITY;
        for k in 2..400 {
            let f = freq * k as f64;
            if f < nyquist - 100.0 {
                harmonics.push((f, level_db(out, f)));
            } else {
                let folded = f % RATE;
                let folded = if folded > nyquist { RATE - folded } else { folded };
                if folded > 20.0 && folded < audible {
                    alias = alias.max(level_db(out, folded));
                }
            }
        }
        (harmonics, alias)
    }

    fn thd(out: &[f64], freq: f64) -> f64 {
        let fundamental = level_db(out, freq);
        let (harmonics, _) = harmonics_and_aliases(out, freq, 20_000.0);
        let power: f64 = harmonics.iter().map(|(_, db)| 10f64.powf(db / 10.0)).sum();
        100.0 * (power / 10f64.powf(fundamental / 10.0)).sqrt()
    }

    #[test]
    fn half_bands_pass_the_audio_band_and_reject_the_images() {
        let (mut up, mut down) = (first_stage(), first_stage());
        // A 10 kHz tone up and back down is unchanged in level.
        let input = sine(10_000.0, 48_000, 0.5);
        let mut images = Vec::new();
        let out: Vec<f64> = input
            .iter()
            .map(|&x| {
                let pair = up.up([x, -x]);
                images.push([pair[0][0], pair[1][0]]);
                let y = down.down(pair);
                assert_eq!(y[1], -y[0]);
                y[0]
            })
            .collect();
        let gain = level_db(&out[4_800..], 10_000.0) - level_db(&input[4_800..], 10_000.0);
        assert!(gain.abs() < 0.01, "{gain} dB");
        // At the doubled rate, the image at 38 kHz (48 - 10) is gone.
        let doubled: Vec<f64> = images[4_800..].iter().flat_map(|p| *p).collect();
        let w = |f: f64| {
            let w = 2.0 * PI * f / (2.0 * RATE);
            let (mut s1, mut s2) = (0.0, 0.0);
            for &v in &doubled {
                let s = v + 2.0 * w.cos() * s1 - s2;
                s2 = s1;
                s1 = s;
            }
            (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt()
        };
        let rejection = 20.0 * (w(38_000.0) / w(10_000.0)).log10();
        assert!(rejection < -95.0, "{rejection} dB");
    }

    #[test]
    fn zero_is_the_input_bit_for_bit() {
        let mut saturator = Saturator::new(RATE);
        for (n, x) in sine(997.0, 4_800, 0.8).into_iter().enumerate() {
            let input = [x, -0.5 * x + n as f64 * 1e-7];
            assert_eq!(saturator.process(input, 0.0), input);
        }
        assert!(!saturator.engaged());
    }

    #[test]
    fn fades_back_to_bit_exact_after_turning_off() {
        let mut saturator = Saturator::new(RATE);
        let input = sine(997.0, 9_600, 0.8);
        for &x in &input[..4_800] {
            saturator.process([x, x], 0.7);
        }
        assert!(saturator.engaged());
        for &x in &input[4_800..5_400] {
            saturator.process([x, x], 0.0);
        }
        assert!(!saturator.engaged());
        for &x in &input[5_400..] {
            assert_eq!(saturator.process([x, x], 0.0), [x, x]);
        }
    }

    #[test]
    fn never_clips() {
        for drive in [0.25, 0.5, 1.0] {
            let out = driven_sine(997.0, 1.0, drive);
            let peak = out.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            // The curve and the ceiling are smooth: the peaks are rounded, not flattened.
            let flat = out.windows(3).filter(|w| (w[0] - peak).abs() < 1e-9 && (w[1] - peak).abs() < 1e-9).count();
            assert_eq!(flat, 0, "{drive}");
            assert!(peak <= 1.0, "{drive}: {peak}");
        }
    }

    /// Hits like a sample's: a loud one, then quieter ones, each a falling tone with some noise.
    fn hits(frames: usize) -> Vec<f64> {
        let mut noise = 1u32;
        (0..frames)
            .map(|n| {
                let hit = n / 12_000;
                let t = (n % 12_000) as f64 / RATE;
                noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = noise as f64 / u32::MAX as f64 - 0.5;
                let level = if hit % 4 == 0 { 0.9 } else { 0.45 };
                let phase = 2.0 * PI * (60.0 * t + 300.0 * (1.0 - (-t * 30.0).exp()) / 30.0);
                level * (-t * 9.0).exp() * (0.85 * phase.sin() + 0.3 * white * (-t * 40.0).exp())
            })
            .collect()
    }

    #[test]
    fn the_peaks_never_pass_the_inputs() {
        let input = hits(4 * 48_000);
        let peak = input.iter().fold(0.0f64, |m, v| m.max(v.abs()));
        for drive in [0.05, 0.25, 0.5, 0.75, 1.0] {
            let mut saturator = Saturator::new(RATE);
            let out: Vec<f64> = input.iter().map(|&x| saturator.process([x, -x], drive)[0]).collect();
            let loudest = out.iter().fold(0.0f64, |m, v| m.max(v.abs()));
            assert!(loudest <= peak, "{drive}: {loudest} against {peak}");
        }
    }

    #[test]
    fn the_body_comes_up_with_drive() {
        let input = hits(4 * 48_000);
        let reference = rms(&input[48_000..]);
        let mut last = 0.0;
        for step in 0..=10 {
            let drive = step as f64 / 10.0;
            let mut saturator = Saturator::new(RATE);
            let out: Vec<f64> = input.iter().map(|&x| saturator.process([x, x], drive)[0]).collect();
            let db = 20.0 * (rms(&out[48_000..]) / reference).log10();
            // Up with the knob: a little at the middle, about 3 dB at the top. At the bottom the
            // level barely moves (a tenth of a dB or so either way while the curve barely bends).
            if step <= 3 {
                assert!(db.abs() < 0.8, "{drive}: {db} dB");
            }
            if step >= 2 {
                assert!(db > last, "{drive}: {db} dB after {last}");
            }
            
            match step {
                0 => assert!(db.abs() < 1e-9, "{db}"),
                5 => assert!((0.5..2.0).contains(&db), "{drive}: {db} dB"),
                10 => assert!((2.0..4.0).contains(&db), "{drive}: {db} dB"),
                _ => {}
            }
            last = db;
        }
        // A steady tone comes up too, by less, since its peaks are its body.
        let reference = rms(&sine(997.0, 48_000, 0.7));
        let db = 20.0 * (rms(&driven_sine(997.0, 0.7, 1.0)) / reference).log10();

        assert!((0.3..4.0).contains(&db), "{db} dB");
    }

    /// `x` through four one-pole low-passes at `hz`.
    fn low_passed(x: &[f64], hz: f64) -> Vec<f64> {
        let a = 1.0 - (-2.0 * PI * hz / RATE).exp();
        let mut poles = [0.0; 4];
        x.iter()
            .map(|&v| {
                let mut v = v;
                for pole in &mut poles {
                    *pole += a * (v - *pole);
                    v = *pole;
                }
                v
            })
            .collect()
    }

    #[test]
    fn the_bias_builds_up_no_sub() {
        let sub = |x: &[f64]| 20.0 * (rms(&low_passed(x, 30.0)[48_000..]) / rms(&x[48_000..])).log10();
        for drive in [0.5, 1.0] {
            // A steady tone: what the bias adds is taken out, leaving nothing down low.
            let steady = sine(220.0, 4 * 48_000, 0.8);
            let mut saturator = Saturator::new(RATE);
            let out: Vec<f64> = steady.iter().map(|&x| saturator.process([x, x], drive)[0]).collect();
            let below = sub(&out);
            assert!(below < -60.0, "{drive}: steady tone's sub at {below} dB");
            // The same tone in bursts, where the bias comes and goes with them: about as little
            // as the bursts themselves have down there.
            let bursts: Vec<f64> = steady.iter().enumerate().map(|(n, &x)| if (n / 4_800) % 2 == 0 { x } else { 0.0 }).collect();
            let mut saturator = Saturator::new(RATE);
            let out: Vec<f64> = bursts.iter().map(|&x| saturator.process([x, x], drive)[0]).collect();
            let (below, clean) = (sub(&out), sub(&bursts));
            assert!(below < clean + 8.0, "{drive}: bursts' sub at {below} dB, clean {clean} dB");
        }
    }

    #[test]
    fn the_ceiling_is_smooth_and_holds_the_peaks() {
        assert_eq!(lift_and_knee(0.0), (1.0, 1.0, 1.0));
        let mut last_knee = 1.0;
        for step in 1..=20 {
            let drive = step as f64 / 20.0;
            let (lift, headroom, knee) = lift_and_knee(drive);
            assert!(lift > 1.0 && headroom > 1.0 && knee < last_knee && knee >= 0.0, "{drive}");
            last_knee = knee;
            // A peak taken as far over as the knob allows lands near the top of the shoulder.
            let top = ceiling(headroom, knee);
            assert!(top < 1.0 && top > 0.9, "{drive}: {top}");
            let mut last = f64::NEG_INFINITY;
            for n in -3000..=3000 {
                let x = n as f64 / 1000.0;
                let y = ceiling(x, knee);
                assert!(y >= last && y.abs() <= 1.0f64.max(x.abs()), "{drive}: {x}");
                if x.abs() <= knee {
                    assert_eq!(y, x);
                } else {
                    assert!(y.abs() <= 1.0);
                }
                last = y;
            }
            // No corner at the knee: the slope is 1 either side of it.
            let slope = (ceiling(knee + 1e-7, knee) - ceiling(knee - 1e-7, knee)) / 2e-7;
            assert!((slope - 1.0).abs() < 1e-4, "{drive}: {slope}");
        }
    }

    #[test]
    fn aliasing_stays_far_below_the_harmonics() {
        for freq in [997.0, 4_999.0] {
            let out = driven_sine(freq, 1.0, 1.0);
            let fundamental = level_db(&out, freq);
            // The oversampling keeps 20 kHz flat, so what folds above that is in its transition.
            let (harmonics, alias) = harmonics_and_aliases(&out, freq, 20_000.0);
            let strongest = harmonics.iter().map(|h| h.1).fold(f64::NEG_INFINITY, f64::max);
            assert!(strongest - fundamental > -40.0, "{freq}: harmonics {strongest} vs {fundamental}");
            // The ceiling's shoulder adds a little more than the curve alone (1.7.14 was under
            // -120 dB), still far under anything audible.
            assert!(alias - fundamental < -80.0, "{freq}: alias {alias} vs fundamental {fundamental}");
            assert!(alias < strongest - 65.0, "{freq}: alias {alias} vs harmonic {strongest}");
        }
    }

    #[test]
    fn distortion_rises_smoothly_with_drive() {
        let mut last = 0.0;
        let mut last_step = 0.0f64;
        for step in 1..=10 {
            let drive = step as f64 / 10.0;
            let now = thd(&driven_sine(997.0, 0.7, drive), 997.0);
            assert!(now > last, "{drive}: {now} after {last}");
            last_step = last_step.max(now - last);
            last = now;
        }
        assert!(last > 5.0, "{last}");
        assert!(last_step < 8.0, "{last_step}");
    }

    /// The biggest jump from one frame to the next.
    fn biggest_step(x: &[f64]) -> f64 {
        x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f64::max)
    }

    #[test]
    fn sweeping_the_knob_does_not_click() {
        let input = sine(220.0, 4 * 48_000, 0.7);
        // The steepest the waveform gets with the knob held anywhere.
        let held = (0..=10)
            .map(|step| {
                let mut saturator = Saturator::new(RATE);
                let out: Vec<f64> = input.iter().map(|&x| saturator.process([x, x], step as f64 / 10.0)[0]).collect();
                biggest_step(&out[4_800..])
            })
            .fold(0.0, f64::max);
        let mut saturator = Saturator::new(RATE);
        let frames = input.len();
        let out: Vec<f64> = input
            .iter()
            .enumerate()
            .map(|(n, &x)| {
                // Up and down the knob, off at both ends.
                let t = n as f64 / frames as f64;
                let drive = (1.0 - (2.0 * t - 1.0).abs()) * 1.2 - 0.1;
                saturator.process([x, x], drive.clamp(0.0, 1.0))[0]
            })
            .collect();
        // Moving the knob, turning it on, and turning it off add no jump of their own.
        let biggest = biggest_step(&out);
        assert!(biggest < held * 1.05, "{biggest} against {held} held");
    }
    #[test]
    fn the_bend_keeps_the_deepest_point_and_lifts_the_rest() {
        assert_eq!(bend_envelope(0.4, 0.0), 0.4);
        for drive in [0.3, 1.0] {
            assert!((bend_envelope(1.0, drive) - 1.0).abs() < 1e-6);
            assert_eq!(bend_envelope(0.0, drive), 0.0);
            let mut last = 0.0;
            for step in 1..=100 {
                let e = step as f32 / 100.0;
                let bent = bend_envelope(e, drive);
                assert!(bent >= e && bent <= 1.0 && bent > last, "{drive} {e}");
                last = bent;
            }
        }
    }

    /// Prints the measurements behind the tests. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn report() {
        for freq in [997.0, 4_999.0] {
            let out = driven_sine(freq, 1.0, 1.0);
            let fundamental = level_db(&out, freq);
            let (harmonics, alias) = harmonics_and_aliases(&out, freq, 20_000.0);
            let (_, above) = harmonics_and_aliases(&out, freq, RATE / 2.0 - 20.0);
            let strongest = harmonics.iter().map(|h| h.1).fold(f64::NEG_INFINITY, f64::max);
            println!(
                "{freq} Hz at full Drive: strongest harmonic {:.1} dB, worst alias under 20 kHz {:.1} dB (re fundamental, {:.1} dB under the strongest harmonic), worst alias up to Nyquist {:.1} dB",
                strongest - fundamental,
                alias - fundamental,
                strongest - alias,
                above - fundamental
            );
        }
        let reference = rms(&sine(997.0, 48_000, 0.7));
        for step in 0..=10 {
            let drive = step as f64 / 10.0;
            let out = driven_sine(997.0, 0.7, drive);
            println!(
                "drive {drive:.1}: THD {:.2}%, level {:+.2} dB, H2 {:.1} dB, H3 {:.1} dB",
                thd(&out, 997.0),
                20.0 * (rms(&out) / reference).log10(),
                level_db(&out, 2.0 * 997.0) - level_db(&out, 997.0),
                level_db(&out, 3.0 * 997.0) - level_db(&out, 997.0)
            );
        }
        // Group delay at 1 kHz: the phase lag of the oversampler, in frames.
        let mut saturator = Saturator::new(RATE);
        let input = sine(1_000.0, 48_000, 0.01);
        let out: Vec<f64> = input.iter().map(|&x| saturator.process([x, x], 0.01)[0]).collect();
        let correlate = |shift: f64| -> f64 {
            (24_000..48_000).map(|n| out[n] * (2.0 * PI * 1_000.0 * (n as f64 - shift) / RATE).sin()).sum()
        };
        let s = correlate(0.0);
        let cos: f64 = (24_000..48_000).map(|n| out[n] * (2.0 * PI * 1_000.0 * n as f64 / RATE).cos()).sum();
        let lag = (-cos).atan2(s) / (2.0 * PI * 1_000.0) * RATE;
        println!("delay at 1 kHz: {lag:.2} frames ({:.3} ms)", lag / RATE * 1000.0);
    }
}