//! Dream Eater's spectral gate.
//!
//! One Hann short-time transform, with the same window on the way back out so a
//! changing gain crossfades instead of stepping at the hop. Left and right are
//! transformed separately and share a gate decision, so the stereo image holds
//! still. The knob sets how far below the frame's loudest partial a bin has to
//! sit before it is thinned, and how far a closed bin is pushed down.
//!
//! A bin is not switched. Its level is smoothed, blurred across its neighbours
//! so a lone spike cannot become a chirp, and then eased toward the open gain
//! or the floor in decibels. Partials bloom in and fade out.

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// How far above the threshold a bin is left fully open. The Hann main lobe of a
/// strong partial sits inside this, so the partial itself is not carved up.
const OPEN_ABOVE_DB: f32 = 3.0;
/// How wide the fade is from fully open down to the floor, in dB of bin level.
const SLOPE_DB: f32 = 28.0;
const RAMP_KNOB: f32 = 0.05;
/// Per-bin bloom, in the decibel domain.
const ATTACK_SEC: f32 = 0.022;
/// Per-bin fade, in the decibel domain.
const RELEASE_SEC: f32 = 0.180;
/// Follows a bin's level. Fast enough to catch a new partial, slow enough that
/// one hop of noise does not open the gate.
const MAG_ATTACK_SEC: f32 = 0.010;
const MAG_RELEASE_SEC: f32 = 0.050;
const KNOB_SEC: f32 = 0.020;
/// Gaussian blur of the bin levels, in bins. Wider toward the bottom of the knob.
const BLUR_RADIUS: isize = 8;

pub struct SpectralGate {
    sample_rate: f32,
    frame: usize,
    hop: usize,
    /// Analysis window, already scaled by `1 / (frame * cola)`.
    window: Vec<f32>,
    latency: u32,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    scratch: Vec<Complex32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    wet_l: Vec<f32>,
    wet_r: Vec<f32>,
    time_l: Vec<f32>,
    time_r: Vec<f32>,
    spec_l: Vec<Complex32>,
    spec_r: Vec<Complex32>,
    gains: Vec<f32>,
    /// Smoothed bin level, shared by both channels.
    env: Vec<f32>,
    /// `env` blurred across frequency, which is what the gate listens to.
    blurred: Vec<f32>,
    /// Synthesis window. The analysis window is `window`, already scaled.
    synth: Vec<f32>,
    pos: usize,
    knob: f32,
    knob_coeff: f32,
    attack_coeff: f32,
    release_coeff: f32,
    mag_attack: f32,
    mag_release: f32,
    /// Rises over one frame after the effect leaves bypass, so a cold STFT fades in.
    warm: f32,
    idle: bool,
    /// Samples of already-buffered audio still waiting to be emitted.
    /// Hyperchroma uses this to keep a voice alive; Haunter's insert always runs.
    #[allow(dead_code)]
    tail: u32,
    /// How many samples have been written into the delay rings since the last reset.
    filled: usize,
}

impl SpectralGate {
    pub fn new(sample_rate: f32) -> Self {
        let mut gate = Self {
            sample_rate: 0.0,
            frame: 0,
            hop: 1,
            window: Vec::new(),
            latency: 0,
            forward: RealFftPlanner::<f32>::new().plan_fft_forward(2048),
            inverse: RealFftPlanner::<f32>::new().plan_fft_inverse(2048),
            scratch: Vec::new(),
            out_l: Vec::new(),
            out_r: Vec::new(),
            dry_l: Vec::new(),
            dry_r: Vec::new(),
            wet_l: Vec::new(),
            wet_r: Vec::new(),
            time_l: Vec::new(),
            time_r: Vec::new(),
            spec_l: Vec::new(),
            spec_r: Vec::new(),
            gains: Vec::new(),
            env: Vec::new(),
            blurred: Vec::new(),
            synth: Vec::new(),
            pos: 0,
            knob: 0.0,
            knob_coeff: 1.0,
            attack_coeff: 1.0,
            release_coeff: 1.0,
            mag_attack: 1.0,
            mag_release: 1.0,
            warm: 0.0,
            idle: true,
            tail: 0,
            filled: 0,
        };
        gate.set_sample_rate(sample_rate);
        gate
    }

    /// How long the engine crossfades when the gate opens or closes onto its delay.
    /// A quarter of the frame, about 10 ms, whatever the hop is.
    pub fn align_fade_samples(&self) -> usize {
        (self.frame / 4).max(1)
    }

    /// The smoothed knob, 0..1.
    pub fn amount(&self) -> f32 {
        self.knob
    }

    /// True once the delay rings hold a full frame, so engaging does not start from silence.
    pub fn primed(&self) -> bool {
        self.frame > 0 && self.filled >= self.frame
    }

    pub fn latency_samples(&self) -> u32 {
        self.latency
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        let sample_rate = if sample_rate.is_finite() && sample_rate > 1.0 {
            sample_rate
        } else {
            44_100.0
        };
        let frame = frame_samples(sample_rate);
        if (self.sample_rate - sample_rate).abs() < 0.1
            && self.frame == frame
            && !self.window.is_empty()
        {
            return;
        }
        self.sample_rate = sample_rate;
        self.frame = frame;
        // An eighth of the frame: the squared Hann window still sums flat, and a
        // gain change is crossfaded eight times per frame instead of four.
        self.hop = (frame / 8).max(1);
        self.latency = frame as u32;

        let mut planner = RealFftPlanner::<f32>::new();
        self.forward = planner.plan_fft_forward(frame);
        self.inverse = planner.plan_fft_inverse(frame);
        let scratch_len = self
            .forward
            .get_scratch_len()
            .max(self.inverse.get_scratch_len());
        self.scratch = vec![Complex32::default(); scratch_len];
        self.spec_l = self.forward.make_output_vec();
        self.spec_r = self.forward.make_output_vec();
        self.time_l = vec![0.0; frame];
        self.time_r = vec![0.0; frame];
        self.out_l = vec![0.0; frame];
        self.out_r = vec![0.0; frame];
        self.dry_l = vec![0.0; frame];
        self.dry_r = vec![0.0; frame];
        self.wet_l = vec![0.0; frame];
        self.wet_r = vec![0.0; frame];
        let bins = self.spec_l.len();
        self.gains = vec![1.0; bins];
        self.env = vec![0.0; bins];
        self.blurred = vec![0.0; bins];

        let cola = cola_hann_squared(frame, self.hop);
        let scale = 1.0 / (frame as f32 * cola);
        self.synth = (0..frame).map(|i| hann(i, frame)).collect();
        self.window = self.synth.iter().map(|window| window * scale).collect();

        let hop_sec = self.hop as f32 / sample_rate;
        self.attack_coeff = one_pole_coeff(hop_sec, ATTACK_SEC);
        self.release_coeff = one_pole_coeff(hop_sec, RELEASE_SEC);
        self.mag_attack = one_pole_coeff(hop_sec, MAG_ATTACK_SEC);
        self.mag_release = one_pole_coeff(hop_sec, MAG_RELEASE_SEC);
        self.knob_coeff = one_pole_coeff(1.0 / sample_rate, KNOB_SEC);
        self.reset();
    }

    pub fn reset(&mut self) {
        for buf in [
            &mut self.out_l,
            &mut self.out_r,
            &mut self.dry_l,
            &mut self.dry_r,
            &mut self.wet_l,
            &mut self.wet_r,
            &mut self.time_l,
            &mut self.time_r,
        ] {
            buf.fill(0.0);
        }
        self.spec_l.fill(Complex32::default());
        self.spec_r.fill(Complex32::default());
        self.gains.fill(1.0);
        self.env.fill(0.0);
        self.blurred.fill(0.0);
        self.pos = 0;
        self.knob = 0.0;
        self.warm = 0.0;
        self.idle = true;
        self.tail = 0;
        self.filled = 0;
    }

    /// Writes one block into the delay rings without reading them and without an FFT.
    /// Off uses this so the rings stay current and turning the knob up does not open onto silence.
    pub fn feed(&mut self, wet_l: &[f32], wet_r: &[f32], dry_l: &[f32], dry_r: &[f32]) {
        let n = wet_l
            .len()
            .min(wet_r.len())
            .min(dry_l.len())
            .min(dry_r.len());
        if n == 0 || self.frame == 0 {
            return;
        }
        for i in 0..n {
            self.dry_l[self.pos] = finite(dry_l[i]);
            self.dry_r[self.pos] = finite(dry_r[i]);
            self.wet_l[self.pos] = finite(wet_l[i]);
            self.wet_r[self.pos] = finite(wet_r[i]);
            self.pos += 1;
            if self.pos == self.frame {
                self.pos = 0;
            }
        }
        self.filled = (self.filled + n).min(self.frame);
    }

    /// Gate `wet` and mix it with the delayed `dry` bus.
    /// The final stereo mix is written back into `wet_l` / `wet_r`.
    /// `amount` is the Splatter knob for this block, before smoothing.
    /// The plugin calls `process_varying`; the constant-amount path is what the unit tests drive.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn process_block(
        &mut self,
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        dry_l: &[f32],
        dry_r: &[f32],
        amount: f32,
    ) {
        self.process_with(wet_l, wet_r, dry_l, dry_r, |_| amount);
    }

    /// Like `process_block`, with a knob target for each sample.
    /// When the block ends in bypass, the returned index is the first sample of that bypass.
    pub fn process_varying(
        &mut self,
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        dry_l: &[f32],
        dry_r: &[f32],
        amounts: &[f32],
    ) -> Option<usize> {
        self.process_with(wet_l, wet_r, dry_l, dry_r, |i| {
            amounts.get(i).copied().unwrap_or(0.0)
        })
    }

    fn process_with(
        &mut self,
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        dry_l: &[f32],
        dry_r: &[f32],
        mut amount_at: impl FnMut(usize) -> f32,
    ) -> Option<usize> {
        let n = wet_l
            .len()
            .min(wet_r.len())
            .min(dry_l.len())
            .min(dry_r.len());
        if n == 0 || self.frame == 0 {
            return None;
        }
        let mut peak = 0.0f32;
        // First sample of the bypass stretch the block ends in, if it ends bypassed.
        let mut closed_at: Option<usize> = None;
        for i in 0..n {
            let amount = amount_at(i);
            let amount = if amount.is_finite() {
                amount.clamp(0.0, 1.0)
            } else {
                0.0
            };
            self.smooth_knob(amount);
            let entering = self.idle && (amount > 1.0e-5 || self.knob > 1.0e-5);
            let was_idle = self.idle;
            if self.knob <= 1.0e-5 && amount <= 1.0e-5 {
                if !self.idle {
                    self.enter_bypass();
                }
                if closed_at.is_none() || !was_idle {
                    closed_at = Some(i);
                }
            } else if entering || self.idle {
                self.idle = false;
                self.warm = 0.0;
                self.gains.fill(1.0);
                closed_at = None;
            } else {
                closed_at = None;
            }

            let delayed_dry = self.dry_l[self.pos];
            let delayed_dry_r = self.dry_r[self.pos];
            let delayed_wet = self.wet_l[self.pos];
            let delayed_wet_r = self.wet_r[self.pos];
            let stft_l = self.out_l[self.pos];
            let stft_r = self.out_r[self.pos];
            self.dry_l[self.pos] = finite(dry_l[i]);
            self.dry_r[self.pos] = finite(dry_r[i]);
            self.wet_l[self.pos] = finite(wet_l[i]);
            self.wet_r[self.pos] = finite(wet_r[i]);
            self.out_l[self.pos] = 0.0;
            self.out_r[self.pos] = 0.0;

            peak = peak
                .max(self.dry_l[self.pos].abs())
                .max(self.dry_r[self.pos].abs())
                .max(self.wet_l[self.pos].abs())
                .max(self.wet_r[self.pos].abs());

            self.pos += 1;
            if self.pos == self.frame {
                self.pos = 0;
            }
            if !self.idle && self.pos % self.hop == 0 {
                self.process_hop();
            }

            let blend = wet_mix(self.knob) * self.warm;
            let left = delayed_dry + delayed_wet * (1.0 - blend) + stft_l * blend;
            let right = delayed_dry_r + delayed_wet_r * (1.0 - blend) + stft_r * blend;
            wet_l[i] = finite(left);
            wet_r[i] = finite(right);
            if !self.idle {
                self.warm = (self.warm + 1.0 / self.frame as f32).min(1.0);
            }
        }
        self.filled = (self.filled + n).min(self.frame);
        if peak > 1.0e-7 {
            self.tail = self.latency;
        } else {
            self.tail = self.tail.saturating_sub(n as u32);
        }
        closed_at.filter(|_| self.idle)
    }

    fn enter_bypass(&mut self) {
        self.idle = true;
        self.warm = 0.0;
        self.gains.fill(1.0);
        self.env.fill(0.0);
        self.out_l.fill(0.0);
        self.out_r.fill(0.0);
    }

    fn smooth_knob(&mut self, amount: f32) {
        if amount == 0.0 && self.knob < 1.0e-4 {
            self.knob = 0.0;
            return;
        }
        self.knob += self.knob_coeff * (amount - self.knob);
        if (self.knob - amount).abs() < 1.0e-6 {
            self.knob = amount;
        }
    }

    fn process_hop(&mut self) {
        window_into(&self.wet_l, &self.window, self.pos, &mut self.time_l);
        window_into(&self.wet_r, &self.window, self.pos, &mut self.time_r);
        if self
            .forward
            .process_with_scratch(&mut self.time_l, &mut self.spec_l, &mut self.scratch)
            .is_err()
        {
            return;
        }
        if self
            .forward
            .process_with_scratch(&mut self.time_r, &mut self.spec_r, &mut self.scratch)
            .is_err()
        {
            return;
        }

        let bins = self
            .spec_l
            .len()
            .min(self.spec_r.len())
            .min(self.gains.len())
            .min(self.env.len());
        let mut peak = 0.0f32;
        for i in 0..bins {
            let mag = self.spec_l[i].norm().max(self.spec_r[i].norm());
            let coeff = if mag >= self.env[i] {
                self.mag_attack
            } else {
                self.mag_release
            };
            let next = self.env[i] + coeff * (mag - self.env[i]);
            self.env[i] = if next.abs() < 1.0e-20 { 0.0 } else { next };
            peak = peak.max(self.env[i]);
        }
        // The decision listens to the neighbourhood, so a one-bin spike is not a partial.
        // The threshold still follows the loudest smoothed bin, which is where the knob's
        // anchors are measured.
        blur_into(
            &self.env[..bins],
            blur_sigma(self.knob),
            &mut self.blurred[..bins],
        );
        let threshold = peak * db_to_gain(threshold_db(self.knob));
        let floor = db_to_gain(floor_db(self.knob));
        for i in 0..bins {
            let target = if peak <= 1.0e-12 {
                1.0
            } else {
                gate_gain(self.blurred[i], threshold, floor)
            };
            // Smoothed in decibels, so a partial blooms and fades instead of jumping.
            let state_db = 20.0 * self.gains[i].max(1.0e-8).log10();
            let target_db = 20.0 * target.max(1.0e-8).log10();
            let coeff = if target_db >= state_db {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            let gain = db_to_gain(state_db + coeff * (target_db - state_db));
            self.gains[i] = gain;
            self.spec_l[i].re *= gain;
            self.spec_l[i].im *= gain;
            self.spec_r[i].re *= gain;
            self.spec_r[i].im *= gain;
        }

        if self
            .inverse
            .process_with_scratch(&mut self.spec_l, &mut self.time_l, &mut self.scratch)
            .is_err()
        {
            return;
        }
        if self
            .inverse
            .process_with_scratch(&mut self.spec_r, &mut self.time_r, &mut self.scratch)
            .is_err()
        {
            return;
        }
        for i in 0..self.frame {
            let window = self.synth[i];
            self.time_l[i] *= window;
            self.time_r[i] *= window;
        }
        add_ring(&mut self.out_l, &self.time_l, self.pos);
        add_ring(&mut self.out_r, &self.time_r, self.pos);
    }
}

/// Nearest power of two to 40 ms. 44.1 kHz and 48 kHz both land on 2048.
pub fn frame_samples(sample_rate: f32) -> usize {
    let target = (sample_rate * 0.040).max(128.0);
    let power = target.log2().round().clamp(7.0, 16.0);
    2.0f32.powf(power) as usize
}

/// Threshold relative to the frame peak. Anchored at -60 dB, -30 dB, and -12 dB.
fn threshold_db(knob: f32) -> f32 {
    let k = knob.clamp(0.0, 1.0) as f64;
    let db = -104.142_857_142_857_14 + k * 170.714_285_714_285_72 + k * k * -78.571_428_571_428_57;
    db as f32
}

/// Closed-gate floor. A gentle bed at the bottom of the knob, a deep thin at the top.
/// About -22 dB as the effect opens, -31 dB at 30%, -43 dB at 60%, and -64 dB at full.
fn floor_db(knob: f32) -> f32 {
    let k = knob.clamp(0.0, 1.0);
    -22.0 - 26.0 * k - 16.0 * k * k
}

/// Wider at the bottom of the knob, where the sound should wash, and still wide
/// enough at full that a partial is a soft cluster rather than one bin.
fn blur_sigma(knob: f32) -> f32 {
    let k = knob.clamp(0.0, 1.0);
    2.4 - 1.3 * k
}

fn wet_mix(knob: f32) -> f32 {
    let t = (knob / RAMP_KNOB).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn gate_gain(mag: f32, threshold: f32, floor: f32) -> f32 {
    if threshold <= 1.0e-12 {
        return 1.0;
    }
    let open = threshold * db_to_gain(OPEN_ABOVE_DB);
    if mag >= open {
        return 1.0;
    }
    let shut = open * db_to_gain(-SLOPE_DB);
    if mag <= shut {
        return floor;
    }
    let pos = ((mag / shut).ln() / (open / shut).ln()).clamp(0.0, 1.0);
    let shape = pos * pos * (3.0 - 2.0 * pos);
    let floor_db = 20.0 * floor.max(1.0e-8).log10();
    db_to_gain(floor_db * (1.0 - shape))
}

fn db_to_gain(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

fn one_pole_coeff(elapsed: f32, time: f32) -> f32 {
    if time <= 1.0e-6 {
        1.0
    } else {
        (1.0 - (-elapsed / time).exp()).clamp(0.0, 1.0)
    }
}

fn finite(sample: f32) -> f32 {
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

fn hann(i: usize, frame: usize) -> f32 {
    let phase = i as f32 / frame as f32;
    0.5 - 0.5 * (std::f32::consts::TAU * phase).cos()
}

/// Sum of the squared Hann window at this hop. Flat for hops of a quarter and an
/// eighth of the frame, which is what analysis-plus-synthesis Hann needs.
fn cola_hann_squared(frame: usize, hop: usize) -> f32 {
    let hop = hop.max(1);
    let mut sum = 0.0f32;
    let mut i = 0;
    while i < frame {
        let window = hann(i, frame);
        sum += window * window;
        i += hop;
    }
    sum.max(1.0e-4)
}

fn blur_into(input: &[f32], sigma: f32, output: &mut [f32]) {
    let n = input.len().min(output.len());
    let sigma = sigma.max(0.35);
    let mut weight = [0.0f32; (BLUR_RADIUS as usize) * 2 + 1];
    for offset in -BLUR_RADIUS..=BLUR_RADIUS {
        let x = offset as f32 / sigma;
        weight[(offset + BLUR_RADIUS) as usize] = (-0.5 * x * x).exp();
    }
    for i in 0..n {
        let mut acc = 0.0f32;
        let mut sum = 0.0f32;
        for offset in -BLUR_RADIUS..=BLUR_RADIUS {
            let j = i as isize + offset;
            if (0..n as isize).contains(&j) {
                let w = weight[(offset + BLUR_RADIUS) as usize];
                acc += input[j as usize] * w;
                sum += w;
            }
        }
        output[i] = if sum > 1.0e-12 { acc / sum } else { 0.0 };
    }
}

fn window_into(ring: &[f32], window: &[f32], start: usize, dest: &mut [f32]) {
    let n = ring.len();
    let first = n - start;
    for i in 0..first {
        dest[i] = ring[start + i] * window[i];
    }
    for i in 0..start {
        dest[first + i] = ring[i] * window[first + i];
    }
}

fn add_ring(ring: &mut [f32], src: &[f32], start: usize) {
    let n = ring.len();
    let first = n - start;
    for i in 0..first {
        ring[start + i] += src[i];
    }
    for i in 0..start {
        ring[i] += src[first + i];
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn frame_is_2048_at_common_rates_and_tracks_forty_ms() {
        assert_eq!(frame_samples(44_100.0), 2048);
        assert_eq!(frame_samples(48_000.0), 2048);
        let wide = frame_samples(96_000.0);
        assert_eq!(wide, 4096);
        let ms = wide as f32 / 96_000.0 * 1000.0;
        assert!(
            ms > 30.0 && ms < 50.0,
            "96 kHz frame should stay near 40 ms, {ms}"
        );
    }

    #[test]
    fn threshold_curve_hits_the_anchor_points() {
        assert!(
            (threshold_db(0.3) + 60.0).abs() < 0.05,
            "{}",
            threshold_db(0.3)
        );
        assert!(
            (threshold_db(0.6) + 30.0).abs() < 0.05,
            "{}",
            threshold_db(0.6)
        );
        assert!(
            (threshold_db(1.0) + 12.0).abs() < 0.05,
            "{}",
            threshold_db(1.0)
        );
        assert!(threshold_db(0.0) < threshold_db(0.3));
        assert!(threshold_db(0.3) < threshold_db(0.6));
        assert!(threshold_db(0.6) < threshold_db(1.0));
    }

    #[test]
    fn floor_stays_open_at_the_bottom_and_closes_at_the_top() {
        assert!((floor_db(0.0) + 22.0).abs() < 0.01, "{}", floor_db(0.0));
        let low = floor_db(0.3);
        assert!(low < -28.0 && low > -36.0, "30% keeps a gentle bed, {low}");
        let mid = floor_db(0.6);
        assert!(mid < low && mid > -50.0, "60% is deeper than 30%, {mid}");
        assert!((floor_db(1.0) + 64.0).abs() < 0.01, "{}", floor_db(1.0));
        assert!(floor_db(0.0) > floor_db(0.5) && floor_db(0.5) > floor_db(1.0));
    }

    #[test]
    fn slope_is_open_above_and_at_the_floor_below() {
        let floor = db_to_gain(-40.0);
        let threshold = db_to_gain(-30.0);
        let open = gate_gain(threshold * db_to_gain(OPEN_ABOVE_DB), threshold, floor);
        let above = gate_gain(threshold * db_to_gain(12.0), threshold, floor);
        let shut = gate_gain(
            threshold * db_to_gain(OPEN_ABOVE_DB - SLOPE_DB - 3.0),
            threshold,
            floor,
        );
        assert!(
            (open - 1.0).abs() < 1.0e-4,
            "the top of the slope is open, {open}"
        );
        assert!(
            (above - 1.0).abs() < 1.0e-4,
            "above the slope is open, {above}"
        );
        assert!(
            (shut - floor).abs() < 1.0e-4,
            "below the slope is the floor, {shut}"
        );
        let mid = gate_gain(
            threshold * db_to_gain(OPEN_ABOVE_DB - SLOPE_DB * 0.5),
            threshold,
            floor,
        );
        assert!(
            mid < 0.5 && mid > floor * 4.0,
            "the middle of the slope is between, {mid}"
        );
    }

    #[test]
    fn squared_hann_sums_flat_at_the_hop() {
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            let frame = frame_samples(rate);
            let hop = frame / 8;
            let mut lowest = f32::MAX;
            let mut highest = 0.0f32;
            for offset in 0..hop {
                let mut sum = 0.0f32;
                let mut i = offset;
                while i < frame {
                    let window = hann(i, frame);
                    sum += window * window;
                    i += hop;
                }
                lowest = lowest.min(sum);
                highest = highest.max(sum);
            }
            assert!(
                (highest - lowest) / highest < 1.0e-5,
                "rate {rate} ripples from {lowest} to {highest}"
            );
            assert!((cola_hann_squared(frame, hop) - highest).abs() / highest < 1.0e-5);
        }
    }

    #[test]
    fn bypass_is_a_pure_delay_of_one_frame() {
        let mut gate = SpectralGate::new(44_100.0);
        let latency = gate.latency_samples() as usize;
        assert_eq!(latency, 2048);
        let mut wet_l = vec![0.0f32; latency * 3];
        let mut wet_r = vec![0.0f32; latency * 3];
        let dry_l = vec![0.0f32; latency * 3];
        let dry_r = vec![0.0f32; latency * 3];
        let mut state = 0x1234_5678u32;
        for (index, sample) in wet_l.iter_mut().enumerate() {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *sample = (state >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0;
            wet_r[index] = *sample * 0.5;
        }
        let input_l = wet_l.clone();
        let input_r = wet_r.clone();
        gate.process_block(&mut wet_l, &mut wet_r, &dry_l, &dry_r, 0.0);
        for i in 0..latency {
            assert_eq!(wet_l[i], 0.0, "latency should lead with silence at {i}");
            assert_eq!(wet_r[i], 0.0);
        }
        for i in latency..input_l.len() {
            assert!(
                (wet_l[i] - input_l[i - latency]).abs() < 1.0e-6,
                "left sample {i} drifted"
            );
            assert!(
                (wet_r[i] - input_r[i - latency]).abs() < 1.0e-6,
                "right sample {i} drifted"
            );
        }
    }

    #[test]
    fn open_gate_reconstructs_a_sine() {
        let mut gate = SpectralGate::new(44_100.0);
        let latency = gate.latency_samples() as usize;
        let n = latency * 6;
        let mut wet_l = vec![0.0f32; n];
        let mut wet_r = vec![0.0f32; n];
        for i in 0..n {
            let t = i as f32 / 44_100.0;
            wet_l[i] = (std::f32::consts::TAU * 440.0 * t).sin() * 0.4;
            wet_r[i] = (std::f32::consts::TAU * 440.0 * t).sin() * 0.25;
        }
        let input = wet_l.clone();
        let dry = vec![0.0f32; n];
        gate.process_block(&mut wet_l, &mut wet_r, &dry, &dry, 0.05);
        let start = latency * 3;
        let mut err = 0.0f32;
        let mut energy = 0.0f32;
        for i in start..n {
            let diff = wet_l[i] - input[i - latency];
            err += diff * diff;
            energy += input[i - latency] * input[i - latency];
        }
        let ratio = err / energy.max(1.0e-12);
        assert!(
            ratio < 1.0e-3,
            "an almost-open gate should pass the tone, error ratio {ratio}"
        );
    }

    #[test]
    fn full_knob_keeps_the_loud_partial_and_strips_the_quiet_one() {
        let mut gate = SpectralGate::new(48_000.0);
        assert_eq!(gate.latency_samples(), 2048);
        let n = 48_000;
        let mut wet_l = vec![0.0f32; n];
        let mut wet_r = vec![0.0f32; n];
        for i in 0..n {
            let t = i as f32 / 48_000.0;
            let loud = (std::f32::consts::TAU * 440.0 * t).sin() * 0.5;
            let quiet = (std::f32::consts::TAU * 3_000.0 * t).sin() * 0.01;
            wet_l[i] = loud + quiet;
            wet_r[i] = loud * 0.2 + quiet;
        }
        let dry = vec![0.0f32; n];
        gate.process_block(&mut wet_l, &mut wet_r, &dry, &dry, 1.0);
        let start = 24_000;
        let body = &wet_l[start..];
        let loud = goertzel(body, 48_000.0, 440.0);
        let quiet = goertzel(body, 48_000.0, 3_000.0);
        assert!(loud > 0.05, "the loud partial should remain, {loud}");
        assert!(
            quiet < loud * 0.02,
            "the partial under the threshold should be stripped, quiet {quiet} loud {loud}"
        );
    }

    #[test]
    fn shared_decision_keeps_a_quiet_channel_of_a_loud_partial() {
        let mut gate = SpectralGate::new(44_100.0);
        let n = 44_100;
        let mut wet_l = vec![0.0f32; n];
        let mut wet_r = vec![0.0f32; n];
        for i in 0..n {
            let tone = (std::f32::consts::TAU * 440.0 * i as f32 / 44_100.0).sin();
            wet_l[i] = tone * 0.5;
            wet_r[i] = tone * 0.05;
        }
        let dry = vec![0.0f32; n];
        gate.process_block(&mut wet_l, &mut wet_r, &dry, &dry, 1.0);
        let right = goertzel(&wet_r[22_050..], 44_100.0, 440.0);
        assert!(
            right > 0.01,
            "the quiet side of a loud partial should stay, {right}"
        );
    }

    fn goertzel(samples: &[f32], sample_rate: f32, freq: f32) -> f32 {
        let omega = std::f32::consts::TAU * freq / sample_rate;
        let coeff = 2.0 * omega.cos();
        let mut s1 = 0.0f32;
        let mut s2 = 0.0f32;
        for sample in samples {
            let s0 = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / samples.len() as f32
    }

    /// A soft chord, a slow swell, and a little noise: the kind of material a hard gate chirps on.
    pub(crate) fn botanical(seconds: f32, sample_rate: f32) -> Vec<f32> {
        let n = (seconds * sample_rate) as usize;
        let mut state = 0x1234_5678u32;
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate;
                let mut tone = 0.0;
                for (harmonic, amp) in [
                    (1.0, 0.22),
                    (2.02, 0.11),
                    (3.0, 0.07),
                    (4.97, 0.045),
                    (7.0, 0.028),
                    (9.0, 0.018),
                ] {
                    tone += (std::f32::consts::TAU * 196.0 * harmonic * t).sin() * amp;
                }
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                let noise = (state >> 8) as f32 / 16_777_216.0 * 2.0 - 1.0;
                let swell = 0.62 + 0.38 * (std::f32::consts::TAU * 0.55 * t).sin();
                (tone + noise * 0.035) * swell
            })
            .collect()
    }

    fn stft_mags(signal: &[f32]) -> Vec<Vec<f32>> {
        let frame = 2048;
        let hop = 512;
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(frame);
        let mut time = fft.make_input_vec();
        let mut spec = fft.make_output_vec();
        let mut scratch = vec![Complex32::default(); fft.get_scratch_len()];
        let mut frames = Vec::new();
        let mut start = 0;
        while start + frame <= signal.len() {
            for i in 0..frame {
                let window = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / frame as f32).cos();
                time[i] = signal[start + i] * window;
            }
            fft.process_with_scratch(&mut time, &mut spec, &mut scratch)
                .unwrap();
            frames.push(spec.iter().map(|bin| bin.norm()).collect());
            start += hop;
        }
        frames
    }

    /// Mean frame-to-frame spectral flux, normalised by the frame energy.
    pub(crate) fn spectral_flux(signal: &[f32]) -> f32 {
        let mags = stft_mags(signal);
        if mags.len() < 2 {
            return 0.0;
        }
        let mut flux = 0.0f32;
        let mut energy = 0.0f32;
        for pair in mags.windows(2) {
            let mut diff = 0.0f32;
            let mut frame = 0.0f32;
            for (earlier, later) in pair[0].iter().zip(&pair[1]) {
                let delta = later - earlier;
                diff += delta * delta;
                frame += later * later;
            }
            flux += diff.sqrt();
            energy += frame.sqrt();
        }
        flux / energy.max(1.0e-12)
    }

    /// Mean standard deviation of the frame-to-frame change in per-bin gain, in dB.
    /// Bins far under the frame peak are the ones a hard gate turns into chirps.
    pub(crate) fn gain_chatter(input: &[f32], output: &[f32], latency: usize) -> f32 {
        let end = input.len().saturating_sub(latency);
        let incoming = stft_mags(&input[..end]);
        let gated = stft_mags(&output[latency..]);
        let frames = incoming.len().min(gated.len());
        if frames < 16 {
            return 0.0;
        }
        let bins = incoming[0].len();
        let mut chatter = 0.0f32;
        let mut counted = 0.0f32;
        for bin in 1..bins.saturating_sub(1) {
            let mut previous = None;
            let mut steps = Vec::new();
            for frame in 8..frames {
                let loud = incoming[frame].iter().copied().fold(0.0f32, f32::max);
                let mag = incoming[frame][bin];
                if mag < loud * db_to_gain(-46.0) {
                    previous = None;
                    continue;
                }
                let gain = (gated[frame][bin] / mag).max(1.0e-6);
                let db = 20.0 * gain.log10();
                if let Some(before) = previous {
                    steps.push(db - before);
                }
                previous = Some(db);
            }
            if steps.len() < 8 {
                continue;
            }
            let mean = steps.iter().sum::<f32>() / steps.len() as f32;
            let variance = steps
                .iter()
                .map(|step| (step - mean) * (step - mean))
                .sum::<f32>()
                / steps.len() as f32;
            chatter += variance.sqrt();
            counted += 1.0;
        }
        chatter / counted.max(1.0)
    }

    fn render_gate(amount: f32) -> (Vec<f32>, Vec<f32>, usize) {
        let sample_rate = 48_000.0;
        let input = botanical(2.0, sample_rate);
        let mut left = input.clone();
        let mut right = input.clone();
        let dry = vec![0.0f32; input.len()];
        let mut gate = SpectralGate::new(sample_rate);
        gate.process_block(&mut left, &mut right, &dry, &dry, amount);
        let latency = gate.latency_samples() as usize;
        (input, left, latency)
    }

    #[test]
    fn the_gate_stays_smooth_while_it_thins() {
        let mut previous = None;
        for amount in [0.3_f32, 0.6, 1.0] {
            let (input, output, latency) = render_gate(amount);
            let settled = &output[latency + 8_000..];
            let flux = spectral_flux(settled);
            let dry_flux = spectral_flux(&input[8_000..input.len() - latency]);
            let ratio = flux / dry_flux.max(1.0e-9);
            let chatter = gain_chatter(&input, &output, latency);
            assert!(
                output.iter().all(|sample| sample.is_finite()),
                "amount {amount}"
            );
            // 1.7.16 measured 0.02 dB, 4.65 dB and 7.21 dB at 30%, 60% and 100%.
            // This gate measures about 0.00 dB, 0.57 dB and 0.58 dB on the same fixture.
            assert!(
                chatter < 1.0,
                "amount {amount} still chatters {chatter:.2} dB per frame"
            );
            if amount >= 0.6 {
                assert!(
                    ratio < 0.85,
                    "amount {amount} should thin the wash, flux ratio {ratio:.3}"
                );
            }
            if let Some(quieter) = previous {
                assert!(
                    ratio <= quieter + 0.05,
                    "turning up should not bring the wash back: {ratio} vs {quieter}"
                );
            }
            previous = Some(ratio);
        }
    }
}
